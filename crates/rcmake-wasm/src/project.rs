use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rcmake_api::{ApiError, CacheDefine, ConfigureRequest, ConfigureResult, run_configure};
use rcmake_host::{Host, PureHost, Stream, path};
use rcmake_model::{CacheType, GeneratorKind};
use rcmake_presets::{EnvSnapshot, PresetKind, PresetSettings};
use serde_json::Value;

use crate::{response, strings};

mod toolchain;

pub(super) const SOURCE: &str = if cfg!(windows) { "C:/src" } else { "/src" };
const CMAKE_ROOT: &str = "/cmake";
const PLACEHOLDER_RETRIES: usize = 32;

pub(super) struct Project<'a> {
    files: BTreeMap<String, String>,
    defines: BTreeMap<String, String>,
    generator: Option<&'a str>,
    build_type: Option<&'a str>,
    preset: Option<&'a str>,
}

pub(super) struct Evaluated {
    pub(super) result: ConfigureResult,
    pub(super) host: Arc<PureHost>,
    pub(super) build: PathBuf,
    pub(super) generator: GeneratorKind,
    pub(super) build_type: String,
}

impl<'a> Project<'a> {
    pub(super) fn from_json(request: &'a Value) -> Result<Self, Value> {
        let files = strings(&request["files"], "files")?;
        for file in files.keys() {
            if file.is_empty()
                || file.bytes().any(|byte| matches!(byte, b'\\' | 0))
                || !Path::new(file)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err(response::failure(
                    "INPUT",
                    &format!("Use a project-relative path for {file:?}."),
                ));
            }
        }
        Ok(Self {
            files,
            defines: request
                .get("defines")
                .map(|value| strings(value, "defines"))
                .transpose()?
                .unwrap_or_default(),
            generator: request["generator"]
                .as_str()
                .filter(|value| !value.is_empty()),
            build_type: request["buildType"].as_str(),
            preset: request["preset"].as_str().filter(|value| !value.is_empty()),
        })
    }

    pub(super) fn evaluate(self) -> Result<Evaluated, Value> {
        let mut placeholders = BTreeSet::new();
        for attempt in 0..=PLACEHOLDER_RETRIES {
            let (request, host, generator, build_type) = self.prepare(&placeholders)?;
            let build = request.build_dir.clone();
            match run_configure(request) {
                Ok(result) => {
                    return Ok(Evaluated {
                        result,
                        host,
                        build,
                        generator,
                        build_type,
                    });
                }
                Err(error) => {
                    let stderr = host.events();
                    let count = placeholders.len();
                    if attempt < PLACEHOLDER_RETRIES
                        && matches!(&error, ApiError::GenerateFailed { .. })
                    {
                        placeholders.extend(
                            stderr
                                .iter()
                                .filter(|event| event.stream == Stream::Stderr)
                                .filter_map(|event| missing_source(&event.bytes)),
                        );
                        if placeholders.len() > count {
                            continue;
                        }
                    }
                    return Err(response::api_error(&error, &stderr));
                }
            }
        }
        Err(response::failure(
            "CONFIGURE",
            "Configure did not complete.",
        ))
    }

    fn prepare(
        &self,
        placeholders: &BTreeSet<PathBuf>,
    ) -> Result<(ConfigureRequest, Arc<PureHost>, GeneratorKind, String), Value> {
        let source = Path::new(SOURCE);
        let host = Arc::new(PureHost::new(
            source.into(),
            virtual_path("/usr/bin/rcmake"),
        ));
        let host_error =
            |error: rcmake_host::HostError| response::failure("FILESYSTEM", &error.to_string());
        for file in placeholders {
            host.seed_file(file, b"").map_err(host_error)?;
        }
        for (file, contents) in &self.files {
            host.seed_file(&source.join(file), contents.as_bytes())
                .map_err(host_error)?;
        }

        let preset = self.resolve_preset(source, &host)?;
        let settings = preset.as_ref().and_then(|preset| match &preset.settings {
            PresetSettings::Configure(settings) => Some(settings.as_ref()),
            _ => None,
        });
        let preset_generator = preset
            .as_ref()
            .and_then(|preset| preset.generator.as_ref())
            .map(ToString::to_string);
        let name = self
            .generator
            .or(preset_generator.as_deref())
            .unwrap_or("Ninja");
        let generator = match name {
            "Ninja" => GeneratorKind::Ninja,
            "Ninja Multi-Config" => GeneratorKind::NinjaMultiConfig,
            _ => {
                return Err(response::failure(
                    "GENERATOR",
                    &format!("Unsupported generator {name:?}. Select Ninja or Ninja Multi-Config."),
                ));
            }
        };
        let build = preset
            .as_ref()
            .and_then(|preset| preset.binary_dir.as_ref())
            .map_or_else(
                || virtual_path("/build"),
                |path| source.join(path::to_native(path)),
            );
        let build = normalize(&build);
        if build == source || build == virtual_path("/") {
            return Err(response::failure(
                "PRESET",
                "Select a separate build directory.",
            ));
        }
        host.create_dir_all(&build).map_err(host_error)?;
        let cmake_root = virtual_path(CMAKE_ROOT);
        toolchain::seed(&host, &build, &cmake_root).map_err(host_error)?;

        let mut request = ConfigureRequest::new(source, &build, cmake_root, host.clone())
            .generator(generator)
            .step_budget(100_000)
            .collect_index(true)
            .define(CacheDefine {
                name: "CMAKE_PLATFORM_INFO_INITIALIZED".into(),
                value: "1".into(),
                ty: Some(CacheType::Internal),
            });
        request.services.collect_annotations = true;
        if let Some(preset) = &preset {
            for (key, value) in &preset.environment {
                host.env_set(key, value.as_deref().map(Vec::as_slice));
            }
        }
        if let Some(settings) = settings {
            if let Some(cache) = &settings.cache_variables {
                request.defines.extend(cache.iter().map(|(name, variable)| {
                    CacheDefine {
                        name: name.clone(),
                        value: variable.value.clone(),
                        ty: variable
                            .kind
                            .as_ref()
                            .map(|kind| CacheType::parse(kind).unwrap_or(CacheType::String)),
                    }
                }));
            }
            for (name, value, ty) in [
                (
                    "CMAKE_TOOLCHAIN_FILE",
                    &settings.toolchain_file,
                    CacheType::Filepath,
                ),
                (
                    "CMAKE_INSTALL_PREFIX",
                    &settings.install_dir,
                    CacheType::Path,
                ),
            ] {
                if let Some(value) = value {
                    request.defines.push(CacheDefine {
                        name: name.into(),
                        value: value.clone(),
                        ty: Some(ty),
                    });
                }
            }
        }
        let build_type = self.build_type.unwrap_or("Debug");
        if generator == GeneratorKind::Ninja
            && !request
                .defines
                .iter()
                .any(|definition| definition.name == b"CMAKE_BUILD_TYPE")
        {
            request.defines.push(CacheDefine {
                name: "CMAKE_BUILD_TYPE".into(),
                value: build_type.into(),
                ty: Some(CacheType::String),
            });
        }
        if generator == GeneratorKind::NinjaMultiConfig
            && !request
                .defines
                .iter()
                .any(|definition| definition.name == b"CMAKE_CONFIGURATION_TYPES")
        {
            request.defines.push(CacheDefine {
                name: "CMAKE_CONFIGURATION_TYPES".into(),
                value: "Debug;Release;RelWithDebInfo".into(),
                ty: Some(CacheType::String),
            });
        }
        request
            .defines
            .extend(self.defines.iter().map(|(name, value)| CacheDefine {
                name: name.as_str().into(),
                value: value.as_str().into(),
                ty: None,
            }));
        Ok((request, host, generator, build_type.to_owned()))
    }

    fn resolve_preset(
        &self,
        source: &Path,
        host: &PureHost,
    ) -> Result<Option<rcmake_presets::ResolvedPreset>, Value> {
        if self.preset.is_none()
            && !["CMakePresets.json", "CMakeUserPresets.json"]
                .iter()
                .any(|file| self.files.contains_key(*file))
        {
            return Ok(None);
        }
        let preset_error =
            |error: rcmake_presets::PresetsError| response::failure("PRESET", &error.to_string());
        let presets = rcmake_presets::load(source, host).map_err(preset_error)?;
        let env = EnvSnapshot::from_host(host);
        let kind = PresetKind::Configure;
        if let Some(name) = self.preset {
            return presets
                .resolve(kind, name.as_bytes(), &env)
                .map(Some)
                .map_err(preset_error);
        }
        match presets.list_visible(kind, &env).as_slice() {
            [preset] => presets
                .resolve(kind, &preset.name, &env)
                .map(Some)
                .map_err(preset_error),
            _ => Ok(None),
        }
    }
}

fn virtual_path(path: &str) -> PathBuf {
    if cfg!(windows) {
        format!("C:{path}").into()
    } else {
        path.into()
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            _ => result.push(part),
        }
    }
    result
}

// Add placeholders only for missing source paths reported by the resolver.
fn missing_source(event: &[u8]) -> Option<PathBuf> {
    let diagnostic = std::str::from_utf8(event).ok()?;
    let (header, body) = diagnostic.split_once("Cannot find source file:\n")?;
    let file = Path::new(body.trim_start_matches('\n').lines().next()?.trim());
    let source = Path::new(SOURCE);
    let file = if file.is_absolute() {
        normalize(file)
    } else {
        let directory = header
            .strip_prefix("CMake Error at ")
            .and_then(|header| header.split_once(" ("))
            .and_then(|(site, _)| site.rsplit_once(':'))
            .map(|(listfile, _)| source.join(listfile))
            .and_then(|listfile| listfile.parent().map(Path::to_owned))
            .unwrap_or_else(|| source.to_owned());
        normalize(&directory.join(file))
    };
    (file.starts_with(source)
        && matches!(
            file.extension().and_then(std::ffi::OsStr::to_str),
            Some("c" | "C" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hh" | "hpp" | "hxx" | "h++")
        ))
    .then_some(file)
}
