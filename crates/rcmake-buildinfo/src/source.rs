//! Load order and section evidence for one build directory.

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_host::{FileKind, Follow, Host};

use crate::compdb::{self, Compiler, Dialect};
use crate::reader::{ArtifactReader, ReadError, ReadLimits};
use crate::snapshot::{
    BuildSnapshot, CacheRecord, DataSource, Evidence, Producer, Section, ToolchainSnapshot,
};
use crate::{cache_file, fileapi, packages};

const STALE_TOLERANCE_SECS: i64 = 5;
const COMPILER_FILE_LIMIT: u64 = 256 << 10;

/// Reason why a directory cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The directory has no readable `CMakeCache.txt`.
    NotConfigured(BString),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(path) => write!(
                f,
                "{} is not a configured build directory.",
                String::from_utf8_lossy(path)
            ),
        }
    }
}

fn value<'a>(cache: &'a BTreeMap<BString, CacheRecord>, name: &[u8]) -> Option<&'a BString> {
    cache
        .get(name)
        .map(|record| &record.value)
        .filter(|value| !value.is_empty())
}

fn stem(path: &[u8]) -> &[u8] {
    let name = path
        .rsplit(|byte| *byte == b'/' || *byte == b'\\')
        .next()
        .unwrap_or(path);
    name.strip_suffix(b".exe").unwrap_or(name)
}

fn scrape_compilers(
    reader: &mut ArtifactReader<'_>,
    cache: &BTreeMap<BString, CacheRecord>,
) -> Vec<ToolchainSnapshot> {
    let version = [
        b"CMAKE_CACHE_MAJOR_VERSION".as_slice(),
        b"CMAKE_CACHE_MINOR_VERSION",
        b"CMAKE_CACHE_PATCH_VERSION",
    ]
    .iter()
    .map(|name| value(cache, name).cloned())
    .collect::<Option<Vec<_>>>()
    .map(|parts| BString::from(parts.join(&b'.')));
    let dirs: Vec<BString> = reader
        .list(Path::new("CMakeFiles"), 4096)
        .into_iter()
        .filter(|(name, kind)| {
            *kind == FileKind::Dir && name.first().is_some_and(u8::is_ascii_digit)
        })
        .map(|(name, _)| name)
        .collect();
    let dir = match (version, dirs.as_slice()) {
        (Some(version), _) if dirs.contains(&version) => version,
        (_, [only]) => only.clone(),
        _ => return Vec::new(),
    };
    let base = Path::new("CMakeFiles").join(String::from_utf8_lossy(&dir).as_ref());
    let mut out = Vec::new();
    for (name, kind) in reader.list(&base, 256) {
        let Some(language) = name
            .strip_prefix(b"CMake")
            .and_then(|rest| rest.strip_suffix(b"Compiler.cmake"))
            .filter(|language| !language.is_empty())
        else {
            continue;
        };
        if kind != FileKind::File {
            continue;
        }
        let Ok(bytes) = reader.read_limited(
            &base.join(String::from_utf8_lossy(&name).as_ref()),
            COMPILER_FILE_LIMIT,
        ) else {
            continue;
        };
        let prefix = [b"set(CMAKE_", language, b"_"].concat();
        let mut toolchain = ToolchainSnapshot {
            language: language.into(),
            ..ToolchainSnapshot::default()
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            let Some(rest) = line.trim_ascii().strip_prefix(prefix.as_slice()) else {
                continue;
            };
            let Some((key, literal)) = literal_set(rest) else {
                continue;
            };
            match key {
                b"COMPILER" => toolchain.path = Some(literal),
                b"COMPILER_ID" => toolchain.compiler_id = Some(literal),
                b"COMPILER_VERSION" => toolchain.version = Some(literal),
                b"COMPILER_FRONTEND_VARIANT" => toolchain.frontend = Some(literal),
                b"COMPILER_TARGET" => toolchain.target = Some(literal),
                _ => {}
            }
        }
        if toolchain.frontend.is_none() {
            toolchain.frontend = toolchain.compiler_id.as_ref().map(|id| {
                BString::from(if id.as_slice() == b"MSVC" {
                    "MSVC"
                } else {
                    "GNU"
                })
            });
        }
        out.push(toolchain);
    }
    out
}

/// Split `KEY "value")` into the key and the literal value.
fn literal_set(rest: &[u8]) -> Option<(&[u8], BString)> {
    let space = rest.iter().position(u8::is_ascii_whitespace)?;
    let key = rest.get(..space)?;
    let value = rest
        .get(space..)?
        .trim_ascii()
        .strip_suffix(b")")?
        .trim_ascii();
    let literal = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    (!literal.contains(&b'$')).then(|| (key, BString::from(literal)))
}

/// Read one build directory.
///
/// # Errors
///
/// Returns [`LoadError::NotConfigured`] when the directory has no readable cache.
pub fn load(host: &dyn Host, dir: &Path) -> Result<BuildSnapshot, LoadError> {
    let given = rcmake_host::path::from_native_cmake(dir);
    let mut reader = ArtifactReader::new(host, dir, ReadLimits::default())
        .ok_or_else(|| LoadError::NotConfigured(given.clone()))?;
    let build_root = rcmake_host::path::from_native_cmake(reader.root());
    let cache_bytes = reader
        .read(Path::new("CMakeCache.txt"))
        .map_err(|_| LoadError::NotConfigured(given.clone()))?;
    let text_cache = cache_file::parse(&cache_bytes);
    let cache_mtime = reader
        .check(Path::new("CMakeCache.txt"))
        .ok()
        .and_then(|path| host.metadata(&path, Follow::No).ok())
        .map(|meta| meta.mtime);
    let api = fileapi::load(&mut reader);
    let mut warnings = Vec::new();
    let mut evidence = BTreeMap::new();

    let cache = match api.as_ref().and_then(|api| api.cache.clone()) {
        Some(cache) => {
            evidence.insert(Section::Cache, Evidence::Complete(DataSource::FileApi));
            cache
        }
        None => {
            evidence.insert(Section::Cache, Evidence::Complete(DataSource::Cache));
            text_cache
        }
    };
    let command = value(&cache, b"CMAKE_COMMAND")
        .cloned()
        .or_else(|| api.as_ref().and_then(|api| api.cmake_path.clone()));
    let tool = match command.as_deref().map(|command| stem(command)) {
        Some(b"rcmake") => "rcmake",
        Some(b"cmake") => "cmake",
        _ => "unknown",
    };
    let version = [
        b"CMAKE_CACHE_MAJOR_VERSION".as_slice(),
        b"CMAKE_CACHE_MINOR_VERSION",
        b"CMAKE_CACHE_PATCH_VERSION",
    ]
    .iter()
    .map(|name| value(&cache, name).cloned())
    .collect::<Option<Vec<_>>>()
    .map(|parts| BString::from(parts.join(&b'.')))
    .or_else(|| api.as_ref().and_then(|api| api.version.clone()));
    let generator = value(&cache, b"CMAKE_GENERATOR")
        .cloned()
        .or_else(|| api.as_ref().and_then(|api| api.generator.clone()));
    let source_root = value(&cache, b"CMAKE_HOME_DIRECTORY")
        .cloned()
        .or_else(|| api.as_ref().and_then(|api| api.source_root.clone()));
    let multi = generator.as_deref().is_some_and(|name| {
        name.ends_with(b"Multi-Config")
            || name.starts_with(b"Visual Studio")
            || name.as_slice() == b"Xcode"
    });
    let configs = match api.as_ref().and_then(|api| api.configs.clone()) {
        Some(configs) if !configs.is_empty() => configs,
        _ if multi => value(&cache, b"CMAKE_CONFIGURATION_TYPES")
            .map(|types| list::split(types, list::Empty::Drop))
            .unwrap_or_else(|| {
                ["Debug", "Release", "RelWithDebInfo"]
                    .map(BString::from)
                    .to_vec()
            }),
        _ => vec![
            value(&cache, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        ],
    };

    let (toolchains, toolchain_source) = match api.as_ref().and_then(|api| api.toolchains.clone()) {
        Some(toolchains) => (toolchains, Some(DataSource::FileApi)),
        None => {
            let scraped = scrape_compilers(&mut reader, &cache);
            let source = (!scraped.is_empty()).then_some(DataSource::CompilerFiles);
            (scraped, source)
        }
    };
    evidence.insert(
        Section::Toolchain,
        toolchain_source.map_or_else(
            || Evidence::Unavailable("No toolchains-v1 reply and no compiler files.".into()),
            Evidence::Complete,
        ),
    );

    let stale = match (api.as_ref().and_then(|api| api.index_mtime), cache_mtime) {
        (Some(index), Some(cache)) => index.secs.saturating_add(STALE_TOLERANCE_SECS) < cache.secs,
        _ => false,
    };
    let mut targets = BTreeMap::new();
    let graph_evidence = match &api {
        Some(api) if api.failed_generation => Evidence::Unavailable(
            "The last generation failed. Older File API replies do not describe this tree.".into(),
        ),
        Some(api) if api.targets.is_some() => {
            targets = api.targets.clone().unwrap_or_default();
            if stale {
                Evidence::Partial(
                    DataSource::FileApi,
                    "File API reply is older than CMakeCache.txt. Re-run cmake to refresh it."
                        .into(),
                )
            } else if let Some(reason) = &api.targets_partial {
                Evidence::Partial(DataSource::FileApi, reason.clone())
            } else {
                Evidence::Complete(DataSource::FileApi)
            }
        }
        _ => {
            let compilers: Vec<Compiler> = toolchains
                .iter()
                .filter_map(|toolchain| {
                    let path = toolchain.path.clone().or_else(|| {
                        value(
                            &cache,
                            &[b"CMAKE_", toolchain.language.as_slice(), b"_COMPILER"].concat(),
                        )
                        .cloned()
                    })?;
                    Some(Compiler {
                        language: toolchain.language.clone(),
                        path,
                        dialect: toolchain.frontend.as_deref().map(|frontend| {
                            if frontend.as_slice() == b"MSVC" {
                                Dialect::Msvc
                            } else {
                                Dialect::Gnu
                            }
                        }),
                    })
                })
                .collect();
            match compdb::load(&mut reader, &compilers, &configs) {
                Ok(database) => {
                    targets = database.targets;
                    let mut reason: BString = "Targets inferred from object paths.".into();
                    if database.opaque > 0 {
                        reason.extend_from_slice(
                            format!(
                                " {} commands had no known dialect and are compared as text.",
                                database.opaque
                            )
                            .as_bytes(),
                        );
                    }
                    if let Some(partial) = database.partial {
                        reason.push(b' ');
                        reason.extend_from_slice(&partial);
                    }
                    Evidence::Partial(DataSource::CompileCommands, reason)
                }
                Err(ReadError::Missing) => {
                    Evidence::Unavailable("No File API reply and no compile_commands.json.".into())
                }
                Err(error) => Evidence::Unavailable(
                    format!("compile_commands.json is unusable. {error}").into(),
                ),
            }
        }
    };
    let link_evidence = match &graph_evidence {
        Evidence::Partial(DataSource::CompileCommands, _) => {
            Evidence::Unavailable("compile_commands.json has no link data.".into())
        }
        other => other.clone(),
    };
    evidence.insert(Section::Targets, graph_evidence.clone());
    evidence.insert(Section::Compile, graph_evidence);
    evidence.insert(Section::Link, link_evidence);
    if let Some(api) = &api {
        warnings.extend(api.warnings.iter().cloned());
    } else {
        warnings.push("No File API reply. Run rcmake diff-config --prepare and configure again for exact data.".into());
    }

    let mut packages = packages::from_cache(host, &cache);
    let mut packages_unknown = true;
    let package_evidence = match packages::trace(&mut reader, &build_root) {
        Some(Ok(trace)) => {
            packages_unknown = !trace.complete;
            for (name, observation) in trace.calls {
                packages.entry(name).or_default().observed.push(observation);
            }
            if trace.complete {
                Evidence::Complete(DataSource::FindTrace)
            } else {
                Evidence::Partial(
                    DataSource::FindTrace,
                    "The rcmake find trace is incomplete. Packages without a record are unknown."
                        .into(),
                )
            }
        }
        Some(Err(note)) => {
            warnings.push(note);
            Evidence::Partial(
                DataSource::Cache,
                "Only cached package directories are known. Outcomes are unknown.".into(),
            )
        }
        None => Evidence::Partial(
            DataSource::Cache,
            "Only cached package directories are known. Outcomes are unknown.".into(),
        ),
    };
    evidence.insert(Section::Packages, package_evidence);

    Ok(BuildSnapshot {
        path: given,
        build_root,
        producer: Producer { tool, version },
        generator,
        source_root,
        configs,
        targets,
        toolchains,
        packages,
        packages_unknown,
        cache,
        evidence,
        warnings,
    })
}
