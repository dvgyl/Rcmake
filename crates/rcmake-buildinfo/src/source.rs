//! Load order and section evidence for one build directory.

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_host::{FileKind, FileTime, Follow, Host};

use crate::compdb::{self, Compiler, Frontend};
use crate::diff::{canonical_root, is_trivial_root};
use crate::reader::{ArtifactReader, ReadError, ReadLimits};
use crate::snapshot::{
    BuildSnapshot, CacheRecord, DataSource, Evidence, Producer, Section, SnapshotHost,
    ToolchainSnapshot, TraceInput,
};
use crate::{cache_file, fileapi, packages};

const COMPILER_FILE_LIMIT: u64 = 256 << 10;
/// Most `cmakeFiles-v1` inputs whose modification time the freshness check reads.
const MAX_INPUT_CHECKS: usize = 20_000;

fn time_key(time: FileTime) -> (i64, u32) {
    (time.secs, time.nanos)
}

/// Why the File API reply does not describe the current tree, or `None` when it
/// does. The check uses full nanosecond times and no tolerance.
fn reply_staleness(
    host: &dyn Host,
    api: &fileapi::FileApiData,
    cache_mtime: Option<FileTime>,
) -> Option<BString> {
    let Some(index) = api.index_mtime else {
        return Some("The File API index has no readable time.".into());
    };
    match cache_mtime {
        Some(cache) if time_key(index) < time_key(cache) => {
            return Some(
                "File API reply is older than CMakeCache.txt. Re-run cmake to refresh it.".into(),
            );
        }
        None => return Some("CMakeCache.txt has no readable time.".into()),
        Some(_) => {}
    }
    let inputs = api.inputs.as_deref()?;
    let root: &[u8] = api
        .inputs_root
        .as_deref()
        .map_or(b"", |root| root.as_slice());
    let project: Vec<_> = inputs.iter().filter(|input| !input.cmake).collect();
    if project.len() > MAX_INPUT_CHECKS {
        return Some(
            format!("The File API lists more than {MAX_INPUT_CHECKS} project inputs, so freshness is unknown.")
                .into(),
        );
    }
    for input in project {
        let path = if input.path.starts_with(b"/")
            || matches!(input.path.as_slice(), [letter, b':', ..] if letter.is_ascii_alphabetic())
            || root.is_empty()
        {
            input.path.clone()
        } else {
            Lexical::new(&[root, b"/", input.path.as_slice()].concat()).normalize()
        };
        let newer = host
            .metadata(&rcmake_host::path::to_native(&path), Follow::Yes)
            .is_ok_and(|meta| time_key(meta.mtime) > time_key(index));
        if newer {
            return Some(
                format!(
                    "{} changed after the File API reply was written. Re-run cmake to refresh it.",
                    String::from_utf8_lossy(&path)
                )
                .into(),
            );
        }
    }
    None
}

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
    let Ok(listing) = reader.list_checked(Path::new("CMakeFiles"), 4096) else {
        return Vec::new();
    };
    let dirs: Vec<BString> = listing
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
    let Ok(listing) = reader.list_checked(&base, 256) else {
        return Vec::new();
    };
    for (name, kind) in listing {
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

fn home_root(host: &dyn Host) -> Option<BString> {
    let raw = host
        .env_get(b"HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            host.env_get(b"USERPROFILE")
                .filter(|value| !value.is_empty())
        })?;
    let normalized = canonical_root(&raw);
    (!is_trivial_root(&normalized)).then_some(normalized)
}

/// Read one build directory. `trace` is the native find trace of the directory,
/// already decoded and checked for freshness by the caller.
///
/// # Errors
///
/// Returns [`LoadError::NotConfigured`] when the directory has no readable cache.
pub fn load(host: &dyn Host, dir: &Path, trace: TraceInput) -> Result<BuildSnapshot, LoadError> {
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
    // Freshness comes first, so a stale reply never marks a section complete.
    let stale = api
        .as_ref()
        .filter(|api| api.unusable.is_none() && !api.failed_generation)
        .and_then(|api| reply_staleness(host, api, cache_mtime));
    let current_api = api.as_ref().filter(|_| stale.is_none());

    let cache = match current_api.and_then(|api| api.cache.clone()) {
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

    let (toolchains, toolchain_evidence) = match current_api.and_then(|api| api.toolchains.clone())
    {
        Some(toolchains) => (toolchains, Evidence::Complete(DataSource::FileApi)),
        None => {
            let scraped = scrape_compilers(&mut reader, &cache);
            if !scraped.is_empty() {
                (scraped, Evidence::Complete(DataSource::CompilerFiles))
            } else if let (Some(reason), Some(toolchains)) =
                (&stale, api.as_ref().and_then(|api| api.toolchains.clone()))
            {
                (
                    toolchains,
                    Evidence::Partial(DataSource::FileApi, reason.clone()),
                )
            } else {
                (
                    Vec::new(),
                    Evidence::Unavailable("No toolchains-v1 reply and no compiler files.".into()),
                )
            }
        }
    };
    evidence.insert(Section::Toolchain, toolchain_evidence);

    let mut targets = BTreeMap::new();
    let graph_evidence = match &api {
        Some(api) if api.unusable.is_some() => {
            Evidence::Unavailable(api.unusable.clone().unwrap_or_default())
        }
        Some(api) if api.failed_generation => Evidence::Unavailable(
            "The last generation failed. Older File API replies do not describe this tree.".into(),
        ),
        Some(api) if api.targets.is_some() => {
            targets = api.targets.clone().unwrap_or_default();
            if let Some(reason) = &stale {
                Evidence::Partial(DataSource::FileApi, reason.clone())
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
                        frontend: toolchain.frontend.as_deref().map(|frontend| {
                            if frontend.as_slice() == b"MSVC" {
                                Frontend::Msvc
                            } else {
                                Frontend::Gnu
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

    let (mut packages, hints_complete) = packages::from_cache(host, &cache);
    let mut packages_unknown = true;
    let mut package_evidence = match trace {
        TraceInput::Current { calls, complete } => {
            packages_unknown = !complete;
            for call in calls {
                packages
                    .entry(call.name)
                    .or_default()
                    .observed
                    .push(call.observation);
            }
            if complete {
                Evidence::Complete(DataSource::FindTrace)
            } else {
                Evidence::Partial(
                    DataSource::FindTrace,
                    "The rcmake find trace is incomplete. Packages without a record are unknown."
                        .into(),
                )
            }
        }
        TraceInput::Past(reason) => {
            let mut note = BString::from("The rcmake find trace is from a past run. ");
            note.extend_from_slice(&reason);
            note.extend_from_slice(b" Package outcomes are unknown.");
            warnings.push(note);
            Evidence::Partial(
                DataSource::Cache,
                "Only cached package directories are known. Outcomes are unknown.".into(),
            )
        }
        TraceInput::Missing => Evidence::Partial(
            DataSource::Cache,
            "Only cached package directories are known. Outcomes are unknown.".into(),
        ),
    };
    if !hints_complete {
        let reason: BString = "Version hints stopped at the read budget.".into();
        package_evidence = match package_evidence {
            Evidence::Complete(source) => Evidence::Partial(source, reason),
            Evidence::Partial(source, mut existing) => {
                existing.push(b' ');
                existing.extend_from_slice(&reason);
                Evidence::Partial(source, existing)
            }
            other => other,
        };
    }
    evidence.insert(Section::Packages, package_evidence);

    let system = host.system();
    let snapshot_host = Some(SnapshotHost {
        os: system.sysname,
        arch: system.machine,
    });
    let home_root = home_root(host);

    Ok(BuildSnapshot {
        path: given,
        build_root,
        producer: Producer {
            tool: tool.into(),
            version,
        },
        host: snapshot_host,
        generator,
        source_root,
        home_root,
        is_normalized: false,
        saved_cache_set: None,
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
