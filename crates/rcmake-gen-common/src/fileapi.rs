//! CMake File API v1 query and reply files.

mod codemodel;
mod queries;

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
    hash::HashAlgo,
    list::{self, Empty},
};
use rcmake_host::{
    DirEntry, FileKind, Follow, Host, WriteMode, path::from_native_cmake, path::to_native,
};
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
    write_index(model, result, host, build_dir, false)
}

/// Report a failed generation to File API clients and preserve earlier replies.
pub fn write_error_replies(
    model: &ProjectModel,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    write_index(model, &GenerationResult::default(), host, build_dir, true)
}

fn write_index(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    failed: bool,
) -> Result<(), GenError> {
    let api_dir = build_dir.join(API_DIR);
    let reply_dir = api_dir.join("reply");
    let old_entries = read_dir(host, &reply_dir)?;
    let root = model.dir(DirId::ROOT);
    let build = from_native_cmake(build_dir);
    let source = if root.source_dir.is_empty() {
        cache_value(model, b"CMAKE_HOME_DIRECTORY").unwrap_or(&build)
    } else {
        root.source_dir.as_slice()
    };
    let mut writer = ReplyWriter {
        host,
        dir: &reply_dir,
        files: BTreeSet::new(),
        existing: old_entries
            .iter()
            .filter(|entry| entry.kind == FileKind::File)
            .map(|entry| entry.name.clone())
            .collect(),
        dir_ready: false,
    };
    let mut api = FileApi {
        model,
        result,
        source,
        build: &build,
        writer: &mut writer,
        objects: BTreeMap::new(),
        failed,
    };
    if let Some((replies, tests)) = api.read_queries(&api_dir.join("query"))? {
        let index = object([
            ("cmake", api.cmake()),
            (
                "objects",
                array(api.objects.into_values().chain(tests.into_values())),
            ),
            ("reply", Object(replies)),
        ]);
        let prefix = if failed { "error" } else { "index" };
        writer.write(&index_name(host, &old_entries, prefix), &json_bytes(&index))?;
    }
    if !failed {
        writer.retain_file_aliases(&old_entries)?;
        writer.remove_stale(old_entries)?;
    }
    Ok(())
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

fn index_name(host: &dyn Host, old: &[DirEntry], prefix: &str) -> String {
    let now = host.now();
    let old: BTreeSet<_> = old.iter().map(|entry| entry.name.as_slice()).collect();
    let name = |offset| {
        let ticks = now.nanos as usize / 100_000 + offset;
        let seconds = now.secs.saturating_add((ticks / 10_000) as i64);
        let time = jiff::Timestamp::from_second(seconds).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        format!(
            "{prefix}-{}-{:04}.json",
            time.strftime("%Y-%m-%dT%H-%M-%S"),
            ticks % 10_000
        )
    };
    (0..old.len())
        .map(name)
        .find(|candidate| !old.contains(candidate.as_bytes()))
        .unwrap_or_else(|| name(old.len()))
}

struct ReplyWriter<'a> {
    host: &'a dyn Host,
    dir: &'a Path,
    files: BTreeSet<BString>,
    /// Regular files in the reply directory before this run.
    existing: BTreeSet<BString>,
    /// The reply directory exists.
    dir_ready: bool,
}

impl ReplyWriter<'_> {
    fn hashed(&mut self, prefix: &str, value: &Value) -> Result<Value, GenError> {
        let bytes = json_bytes(value);
        let name = format!("{prefix}-{}.json", hash_prefix(&bytes));
        // The name holds the content hash. Like CMake, keep an existing file with this name.
        if self.existing.contains(name.as_bytes()) {
            self.files.insert(name.as_str().into());
        } else {
            self.write(&name, &bytes)?;
        }
        Ok(Value::string(name.as_str()))
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), GenError> {
        if self.files.contains(name.as_bytes()) {
            return Ok(());
        }
        if !self.dir_ready {
            self.host
                .create_dir_all(self.dir)
                .map_err(|err| write_error(self.dir, err))?;
            self.dir_ready = true;
        }
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
            path: from_native_cmake(&path),
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
    failed: bool,
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
        if self.failed && kind != Kind::ConfigureLog {
            return Ok(queries::error("no buildsystem generated"));
        }
        if let Some(reference) = self.objects.get(&kind) {
            return Ok(reference.clone());
        }
        let mut body = match kind {
            Kind::Codemodel => self.codemodel()?,
            Kind::ConfigureLog => self.configure_log()?,
            Kind::Cache => cache(self.model),
            Kind::CmakeFiles => self.cmake_files(),
            Kind::Toolchains => toolchains(self.model, &self.result.metadata),
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
                    ("cmake", Value::string(from_native_cmake(&exe))),
                    (
                        "ctest",
                        Value::string(from_native_cmake(
                            &tool_dir.join(format!("ctest{}", std::env::consts::EXE_SUFFIX)),
                        )),
                    ),
                    (
                        "cpack",
                        Value::string(from_native_cmake(
                            &tool_dir.join(format!("cpack{}", std::env::consts::EXE_SUFFIX)),
                        )),
                    ),
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
        let mut seen = BTreeSet::new();
        let inputs = self
            .result
            .metadata
            .listfiles
            .iter()
            .filter(|file| seen.insert(file.as_slice()))
            .map(|file| {
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
        let mut body = members([
            ("paths", paths(self.source, self.build)),
            ("inputs", array(inputs)),
        ]);
        if !self.model.glob_checks.is_empty() {
            let globs = self.model.glob_checks.iter().map(|(key, check)| {
                let mut glob = members([
                    ("expression", Value::string(key.expression.clone())),
                    ("paths", strings(&list::split(&check.files, Empty::Drop))),
                ]);
                for (name, enabled) in [
                    ("recurse", key.recursive),
                    ("listDirectories", key.directories),
                    ("followSymlinks", key.follow),
                ] {
                    if enabled {
                        glob.insert(name.into(), Bool(true));
                    }
                }
                if let Some(relative) = &key.relative {
                    glob.insert("relative".into(), Value::string(relative.clone()));
                }
                Object(glob)
            });
            body.insert("globsDependent".into(), array(globs));
        }
        body
    }

    fn configure_log(&self) -> Result<Members, GenError> {
        let metadata = &self.result.metadata;
        let path = metadata.configure_log.clone().unwrap_or_else(|| {
            from_native_cmake(&to_native(self.build).join("CMakeFiles/CMakeConfigureLog.yaml"))
        });
        if self.failed {
            let log_path = to_native(&path);
            if let Err(error) = self.writer.host.metadata(&log_path, Follow::Yes) {
                if error.kind != ErrorKind::NotFound {
                    return Err(write_error(&log_path, error));
                }
                if let Some(dir) = log_path.parent() {
                    self.writer
                        .host
                        .create_dir_all(dir)
                        .map_err(|err| write_error(dir, err))?;
                }
                let content = crate::configure_log::render(metadata)?;
                crate::write_if_changed(self.writer.host, &log_path, &content)?;
            }
        }
        Ok(configure_log(path))
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

fn toolchains(model: &ProjectModel, metadata: &ConfigureMetadata) -> Members {
    let mut languages: Vec<_> = metadata.toolchains.iter().collect();
    languages.sort_by_key(|(language, _)| *language);
    let entries = languages
        .into_iter()
        .filter(|(language, _)| language.as_slice() != b"NONE")
        .map(|(language, toolchain)| {
            let definition = |suffix: &[u8]| {
                let variable = [b"CMAKE_", language.as_slice(), b"_", suffix].concat();
                model
                    .dirs()
                    .find_map(|(dir, _)| model.definition(dir, &variable))
            };
            let mut implicit = Members::new();
            for (key, suffix, fallback) in [
                (
                    "includeDirectories",
                    b"IMPLICIT_INCLUDE_DIRECTORIES".as_slice(),
                    toolchain.implicit_includes.as_slice(),
                ),
                (
                    "linkDirectories",
                    b"IMPLICIT_LINK_DIRECTORIES",
                    toolchain.implicit_link_dirs.as_slice(),
                ),
                (
                    "linkFrameworkDirectories",
                    b"IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
                    &[],
                ),
                ("linkLibraries", b"IMPLICIT_LINK_LIBRARIES", &[]),
            ] {
                if let Some(value) = definition(suffix) {
                    implicit.insert(key.into(), strings(&list::split(value, Empty::Drop)));
                } else if !fallback.is_empty() {
                    implicit.insert(key.into(), strings(fallback));
                }
            }
            let mut compiler = members([("implicit", Object(implicit))]);
            for (key, suffix, fallback) in [
                (
                    "path",
                    b"COMPILER".as_slice(),
                    Some(&toolchain.compiler_path),
                ),
                ("id", b"COMPILER_ID", Some(&toolchain.compiler_id)),
                (
                    "version",
                    b"COMPILER_VERSION",
                    Some(&toolchain.compiler_version),
                ),
                ("target", b"COMPILER_TARGET", None),
                ("commandFragment", b"COMPILER_ARG1", None),
            ] {
                if let Some(value) = definition(suffix)
                    .filter(|value| key != "commandFragment" || !value.is_empty())
                    .or_else(|| fallback.filter(|value| !value.is_empty()))
                {
                    compiler.insert(key.into(), Value::string(value.clone()));
                }
            }
            let mut entry = members([
                ("language", Value::string(language.clone())),
                ("compiler", Object(compiler)),
            ]);
            if let Some(value) = definition(b"SOURCE_FILE_EXTENSIONS") {
                entry.insert(
                    "sourceFileExtensions".into(),
                    strings(&list::split(value, Empty::Drop)),
                );
            } else if !toolchain.source_extensions.is_empty() {
                entry.insert(
                    "sourceFileExtensions".into(),
                    strings(&toolchain.source_extensions),
                );
            }
            Object(entry)
        });
    members([("toolchains", array(entries))])
}

fn configure_log(path: BString) -> Members {
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
            from_native_cmake(suffix)
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
#[cfg(test)]
mod tests_extra;
