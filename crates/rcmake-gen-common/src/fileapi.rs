//! CMake File API v1 query and reply files.

mod codemodel;
mod queries;

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
    hash::HashAlgo,
};
use rcmake_host::{DirEntry, FileKind, Host, WriteMode, path::from_native, path::to_native};
use rcmake_json::Value::{self, Array, Bool, Object, UInt};
use rcmake_model::{DirId, GeneratorKind, ProjectModel};
use rcmake_resolve::{ConfigureMetadata, GenerationResult};

use crate::{GenError, write_error};

type Members = BTreeMap<BString, Value>;

const API_DIR: &str = ".cmake/api/v1";

/// Answer the File API queries under `<build_dir>/.cmake/api/v1/query`.
///
/// Use resolved target data for each configuration and the configure model for the
/// cache, directory tree, and install rules. Publish the index after its reply files
/// are available. Remove stale files after publication so clients can read the
/// previous reply while this one is prepared.
pub fn write_replies(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    let api_dir = build_dir.join(API_DIR);
    let reply_dir = api_dir.join("reply");
    let old_entries = read_dir(host, &reply_dir)?;
    let root = model.dir(DirId::ROOT);
    let build = from_native(build_dir);
    let source = if root.source_dir.is_empty() {
        cache_value(model, b"CMAKE_HOME_DIRECTORY").unwrap_or(&build)
    } else {
        root.source_dir.as_slice()
    };
    let mut writer = ReplyWriter {
        host,
        dir: &reply_dir,
        files: BTreeSet::new(),
    };
    let mut api = FileApi {
        model,
        result,
        source,
        build: &build,
        writer: &mut writer,
        objects: BTreeMap::new(),
    };
    let replies = api.read_queries(&api_dir.join("query"))?;
    if !replies.is_empty() {
        let index = object([
            ("cmake", api.cmake()),
            ("objects", array(api.objects.into_values())),
            ("reply", Object(replies)),
        ]);
        writer.write(&index_name(host, &old_entries), &json_bytes(&index))?;
    }
    writer.remove_stale(old_entries)
}

fn read_dir(host: &dyn Host, dir: &Path) -> Result<Vec<DirEntry>, GenError> {
    match host.read_dir(dir) {
        Ok(entries) => Ok(entries),
        Err(err) if matches!(err.kind, ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(Vec::new())
        }
        Err(err) => Err(write_error(dir, err)),
    }
}

fn index_name(host: &dyn Host, old: &[DirEntry]) -> String {
    let now = host.now();
    let stamp = format!("index-{:020}-{:09}-", now.secs, now.nanos);
    let id = old
        .iter()
        .filter_map(|entry| entry.name.as_slice().strip_prefix(stamp.as_bytes()))
        .filter_map(|suffix| suffix.strip_suffix(b".json"))
        .filter_map(|id| std::str::from_utf8(id).ok()?.parse::<u64>().ok())
        .max()
        .map_or(0, |id| id.saturating_add(1));
    format!("{stamp}{id:04}.json")
}

struct ReplyWriter<'a> {
    host: &'a dyn Host,
    dir: &'a Path,
    files: BTreeSet<BString>,
}

impl ReplyWriter<'_> {
    fn hashed(&mut self, prefix: &str, value: &Value) -> Result<Value, GenError> {
        let bytes = json_bytes(value);
        let name = format!("{prefix}-{}.json", hash_prefix(&bytes));
        self.write(&name, &bytes)?;
        Ok(Value::string(name.as_str()))
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), GenError> {
        if self.files.contains(name.as_bytes()) {
            return Ok(());
        }
        self.host
            .create_dir_all(self.dir)
            .map_err(|err| write_error(self.dir, err))?;
        let path = self.dir.join(name);
        let pid = self.host.system().pid;
        for attempt in 0..64 {
            let temp = self.dir.join(format!(".rcmake-{pid}-{attempt}.tmp"));
            match self.host.write(&temp, bytes, WriteMode::CreateNew) {
                Ok(()) => {
                    if let Err(err) = self.host.rename(&temp, &path) {
                        let _ = self.host.remove_file(&temp);
                        return Err(write_error(&path, err));
                    }
                    self.files.insert(name.into());
                    return Ok(());
                }
                Err(err) if err.kind == ErrorKind::AlreadyExists => {}
                Err(err) => return Err(write_error(&path, err)),
            }
        }
        Err(GenError::Write {
            path: from_native(&path),
            message: "cannot create temporary reply file".into(),
        })
    }

    fn remove_stale(&self, mut entries: Vec<DirEntry>) -> Result<(), GenError> {
        entries.sort_by_key(|entry| !entry.name.starts_with(b"index-"));
        for entry in entries {
            if !matches!(entry.kind, FileKind::File | FileKind::Symlink)
                || self.files.contains(&entry.name)
                || (entry.name.starts_with(b".rcmake-") && entry.name.ends_with(b".tmp"))
            {
                continue;
            }
            let path = self.dir.join(to_native(&entry.name));
            if let Err(err) = self.host.remove_file(&path)
                && err.kind != ErrorKind::NotFound
            {
                return Err(write_error(&path, err));
            }
        }
        Ok(())
    }
}

fn json_bytes(value: &Value) -> BString {
    let mut bytes = rcmake_json::write_styled(value, "\t");
    bytes.push(b'\n');
    bytes
}

fn hash_prefix(bytes: &[u8]) -> String {
    HashAlgo::Sha256
        .hex_digest(bytes)
        .chars()
        .take(20)
        .collect()
}

struct FileApi<'a, 'w> {
    model: &'a ProjectModel,
    result: &'a GenerationResult,
    source: &'a [u8],
    build: &'a [u8],
    writer: &'w mut ReplyWriter<'a>,
    objects: BTreeMap<Kind, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Codemodel,
    ConfigureLog,
    Cache,
    CmakeFiles,
    Toolchains,
}

impl Kind {
    const ALL: [Self; 5] = [
        Self::Codemodel,
        Self::ConfigureLog,
        Self::Cache,
        Self::CmakeFiles,
        Self::Toolchains,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Codemodel => "codemodel",
            Self::ConfigureLog => "configureLog",
            Self::Cache => "cache",
            Self::CmakeFiles => "cmakeFiles",
            Self::Toolchains => "toolchains",
        }
    }

    const fn version(self) -> (u32, u32) {
        match self {
            Self::Codemodel => (2, 12),
            Self::ConfigureLog => (1, 0),
            Self::Cache => (2, 0),
            Self::CmakeFiles | Self::Toolchains => (1, 1),
        }
    }

    fn version_json(self) -> Value {
        let (major, minor) = self.version();
        object([("major", UInt(major.into())), ("minor", UInt(minor.into()))])
    }
}

impl FileApi<'_, '_> {
    fn reference(&mut self, kind: Kind) -> Result<Value, GenError> {
        if let Some(reference) = self.objects.get(&kind) {
            return Ok(reference.clone());
        }
        let mut body = match kind {
            Kind::Codemodel => self.codemodel()?,
            Kind::ConfigureLog => configure_log(&self.result.metadata, self.build),
            Kind::Cache => cache(self.model),
            Kind::CmakeFiles => self.cmake_files(),
            Kind::Toolchains => toolchains(&self.result.metadata),
        };
        body.insert("kind".into(), Value::string(kind.name()));
        body.insert("version".into(), kind.version_json());
        let prefix = format!("{}-v{}", kind.name(), kind.version().0);
        let file = self.writer.hashed(&prefix, &Object(body))?;
        let reference = object([
            ("kind", Value::string(kind.name())),
            ("version", kind.version_json()),
            ("jsonFile", file),
        ]);
        self.objects.insert(kind, reference.clone());
        Ok(reference)
    }

    fn generator(&self) -> Option<GeneratorKind> {
        self.model
            .generator
            .or_else(|| GeneratorKind::from_name(cache_value(self.model, b"CMAKE_GENERATOR")?))
    }

    fn cmake(&self) -> Value {
        let exe = self.writer.host.current_exe().unwrap_or_default();
        let tool_dir = exe.parent().unwrap_or(Path::new(""));
        let generator = self.generator();
        let mut generator_json = members([
            (
                "name",
                Value::string(generator.map_or("", GeneratorKind::name)),
            ),
            (
                "multiConfig",
                Bool(generator.is_some_and(GeneratorKind::multi_config)),
            ),
        ]);
        if let Some(platform) = cache_value(self.model, b"CMAKE_GENERATOR_PLATFORM")
            .filter(|platform| !platform.is_empty())
        {
            generator_json.insert("platform".into(), Value::string(platform));
        }
        object([
            (
                "version",
                object([
                    ("major", UInt(CMAKE_VERSION_MAJOR.into())),
                    ("minor", UInt(CMAKE_VERSION_MINOR.into())),
                    ("patch", UInt(CMAKE_VERSION_PATCH.into())),
                    ("string", Value::string(CMAKE_VERSION_STR)),
                    ("suffix", Value::string("")),
                    ("isDirty", Bool(false)),
                ]),
            ),
            (
                "paths",
                object([
                    ("cmake", Value::string(from_native(&exe))),
                    ("ctest", Value::string(from_native(&tool_dir.join("ctest")))),
                    ("cpack", Value::string(from_native(&tool_dir.join("cpack")))),
                    ("root", Value::string(self.cmake_root())),
                ]),
            ),
            ("generator", Object(generator_json)),
        ])
    }

    fn cmake_root(&self) -> BString {
        cache_value(self.model, b"CMAKE_ROOT")
            .map(BString::from)
            .or_else(|| self.writer.host.env_get(b"RCMAKE_VENDORED_DATA_ROOT"))
            .or_else(|| {
                self.result.metadata.listfiles.iter().find_map(|file| {
                    let suffix = b"/Modules/CMakeDetermineSystem.cmake";
                    let root = file.strip_suffix(suffix)?;
                    Some(root.into())
                })
            })
            .unwrap_or_default()
    }

    fn cmake_files(&self) -> Members {
        let cmake_root = self.cmake_root();
        let inputs = self.result.metadata.listfiles.iter().map(|file| {
            let in_source = is_under(file, self.source);
            let in_build = is_under(file, self.build);
            let mut input = members([("path", Value::string(relative(file, self.source)))]);
            if in_build && self.build != self.source {
                input.insert("isGenerated".into(), Bool(true));
            }
            if !in_source && !in_build {
                input.insert("isExternal".into(), Bool(true));
            }
            if is_under(file, &cmake_root) {
                input.insert("isCMake".into(), Bool(true));
            }
            Object(input)
        });
        members([
            ("paths", paths(self.source, self.build)),
            ("inputs", array(inputs)),
        ])
    }
}

fn cache(model: &ProjectModel) -> Members {
    let mut entries: Vec<_> = model.cache.iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    let entries = entries.into_iter().map(|(name, entry)| {
        let mut properties: BTreeMap<_, _> = entry
            .props
            .names()
            .into_iter()
            .filter_map(|name| Some((name.clone(), entry.props.get(&name)?.clone())))
            .collect();
        properties.insert("HELPSTRING".into(), entry.help.clone());
        if entry.advanced {
            properties
                .entry("ADVANCED".into())
                .or_insert_with(|| "1".into());
        }
        object([
            ("name", Value::string(name.clone())),
            ("type", Value::string(entry.ty.as_str())),
            ("value", Value::string(entry.value.clone())),
            (
                "properties",
                array(properties.into_iter().map(|(name, value)| {
                    object([
                        ("name", Value::string(name)),
                        ("value", Value::string(value)),
                    ])
                })),
            ),
        ])
    });
    members([("entries", array(entries))])
}

fn toolchains(metadata: &ConfigureMetadata) -> Members {
    let mut languages: Vec<_> = metadata.toolchains.iter().collect();
    languages.sort_by_key(|(language, _)| *language);
    let entries = languages.into_iter().map(|(language, toolchain)| {
        let mut compiler = members([(
            "implicit",
            object([
                ("includeDirectories", strings(&toolchain.implicit_includes)),
                ("linkDirectories", strings(&toolchain.implicit_link_dirs)),
                ("linkFrameworkDirectories", array([])),
                ("linkLibraries", array([])),
            ]),
        )]);
        for (key, value) in [
            ("path", &toolchain.compiler_path),
            ("id", &toolchain.compiler_id),
            ("version", &toolchain.compiler_version),
        ] {
            if !value.is_empty() {
                compiler.insert(key.into(), Value::string(value.clone()));
            }
        }
        object([
            ("language", Value::string(language.clone())),
            ("compiler", Object(compiler)),
            (
                "sourceFileExtensions",
                strings(&toolchain.source_extensions),
            ),
        ])
    });
    members([("toolchains", array(entries))])
}

fn configure_log(metadata: &ConfigureMetadata, build: &[u8]) -> Members {
    let path = metadata.configure_log.clone().unwrap_or_else(|| {
        from_native(&to_native(build).join("CMakeFiles/CMakeConfigureLog.yaml"))
    });
    members([
        ("path", Value::string(path)),
        (
            "eventKindNames",
            array(
                [
                    "message-v1",
                    "try_compile-v1",
                    "try_run-v1",
                    "find-v1",
                    "find_package-v1",
                ]
                .map(Value::string),
            ),
        ),
    ])
}

fn cache_value<'a>(model: &'a ProjectModel, key: &[u8]) -> Option<&'a [u8]> {
    model.cache.get(key).map(|entry| entry.value.as_slice())
}

fn paths(source: &[u8], build: &[u8]) -> Value {
    object([
        ("source", Value::string(source)),
        ("build", Value::string(build)),
    ])
}

fn is_under(path: &[u8], root: &[u8]) -> bool {
    !root.is_empty() && to_native(path).starts_with(to_native(root))
}

fn relative(path: &[u8], root: &[u8]) -> BString {
    if !root.is_empty()
        && let Ok(suffix) = to_native(path).strip_prefix(to_native(root))
    {
        return if suffix.as_os_str().is_empty() {
            ".".into()
        } else {
            from_native(suffix)
        };
    }
    path.into()
}

fn members<const N: usize>(fields: [(&str, Value); N]) -> Members {
    BTreeMap::from(fields.map(|(key, value)| (key.into(), value)))
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Object(members(fields))
}

fn array(items: impl IntoIterator<Item = Value>) -> Value {
    Array(items.into_iter().collect())
}

fn strings(items: &[BString]) -> Value {
    array(items.iter().cloned().map(Value::string))
}

#[cfg(test)]
mod tests;
