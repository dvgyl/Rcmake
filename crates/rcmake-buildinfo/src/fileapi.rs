//! File API v1 reply reader for `codemodel-v2`, `cache-v2`, `toolchains-v1`, and
//! `cmakeFiles-v1`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use rcmake_base::{BString, cmdline::Arguments};
use rcmake_host::{FileKind, FileTime, Follow};
use rcmake_json::Value;

use crate::reader::ArtifactReader;
use crate::snapshot::{
    Attribution, CacheRecord, CompileSettings, IncludeDir, LinkFragment, TargetSnapshot,
    ToolchainSnapshot,
};

const REPLY: &str = ".cmake/api/v1/reply";
const MAX_TARGETS: usize = 100_000;
const MAX_REPLY_FILES: usize = 200_000;

/// Data from the current reply index.
#[derive(Debug, Default)]
pub(crate) struct FileApiData {
    pub(crate) index_mtime: Option<FileTime>,
    pub(crate) failed_generation: bool,
    pub(crate) generator: Option<BString>,
    pub(crate) cmake_path: Option<BString>,
    pub(crate) version: Option<BString>,
    pub(crate) configs: Option<Vec<BString>>,
    pub(crate) source_root: Option<BString>,
    pub(crate) targets: Option<BTreeMap<(BString, BString), TargetSnapshot>>,
    pub(crate) targets_partial: Option<BString>,
    pub(crate) cache: Option<BTreeMap<BString, CacheRecord>>,
    pub(crate) toolchains: Option<Vec<ToolchainSnapshot>>,
    pub(crate) inputs: Option<Vec<BString>>,
    pub(crate) warnings: Vec<BString>,
}

fn text(value: Option<&Value>) -> Option<BString> {
    value.and_then(Value::as_bytes).map(BString::from)
}

fn items(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).unwrap_or_default()
}

/// Return the name of the current index: the largest name across `index-*.json` and
/// `error-*.json` after the prefix.
pub(crate) fn current_index(names: impl IntoIterator<Item = BString>) -> Option<(BString, bool)> {
    names
        .into_iter()
        .filter_map(|name| {
            let stem = name.strip_suffix(b".json")?;
            let (suffix, error) = if let Some(rest) = stem.strip_prefix(b"index-") {
                (rest, false)
            } else {
                (stem.strip_prefix(b"error-")?, true)
            };
            Some((BString::from(suffix), error, name.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, error, name)| (name, error))
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.contains(&b'/')
        && !name.contains(&b'\\')
        && !name.contains(&b':')
        && name != b"."
        && name != b".."
}

struct Reply<'r, 'h> {
    reader: &'r mut ArtifactReader<'h>,
    dir: PathBuf,
    warnings: Vec<BString>,
    files: usize,
}

impl Reply<'_, '_> {
    fn object(&mut self, name: &[u8]) -> Option<Value> {
        if !plain_name(name) {
            self.warnings.push(
                format!(
                    "Ignored File API reply file {} because it is not a plain file name.",
                    String::from_utf8_lossy(name)
                )
                .into(),
            );
            return None;
        }
        self.files += 1;
        if self.files > MAX_REPLY_FILES {
            return None;
        }
        let path = self.dir.join(String::from_utf8_lossy(name).as_ref());
        match self.reader.json(&path) {
            Ok(value) => Some(value),
            Err(error) => {
                self.warnings.push(
                    format!(
                        "Ignored File API reply file {}. {error}",
                        String::from_utf8_lossy(name)
                    )
                    .into(),
                );
                None
            }
        }
    }
}

/// Supported major versions by kind.
fn supported(kind: &[u8]) -> Option<u64> {
    match kind {
        b"codemodel" | b"cache" => Some(2),
        b"toolchains" | b"cmakeFiles" => Some(1),
        _ => None,
    }
}

/// Collect the newest supported reference of each kind from `objects`, the shared
/// stateless `reply` members, and client members.
fn references(index: &Value) -> BTreeMap<BString, (u64, BString)> {
    let mut found: BTreeMap<BString, (u64, BString)> = BTreeMap::new();
    let mut add = |reference: &Value| {
        let (Some(kind), Some(file)) =
            (text(reference.get("kind")), text(reference.get("jsonFile")))
        else {
            return;
        };
        let version = reference.get("version");
        let major = version.and_then(|v| v.get("major")).and_then(Value::as_u64);
        let minor = version
            .and_then(|v| v.get("minor"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if supported(&kind).is_none() || major != supported(&kind) {
            return;
        }
        let newer = found.get(&kind).is_none_or(|(old, _)| minor > *old);
        if newer {
            found.insert(kind, (minor, file));
        }
    };
    for reference in items(index.get("objects")) {
        add(reference);
    }
    if let Some(reply) = index.get("reply").and_then(Value::as_object) {
        for (key, value) in reply {
            if key.starts_with(b"client-") {
                for nested in value.as_object().into_iter().flat_map(|map| map.values()) {
                    add(nested);
                    for response in items(nested.get("responses")) {
                        add(response);
                    }
                }
            } else {
                add(value);
            }
        }
    }
    found
}

/// Read the current reply index and the objects it references.
pub(crate) fn load(reader: &mut ArtifactReader<'_>) -> Option<FileApiData> {
    let dir = PathBuf::from(REPLY);
    let names = reader
        .list(&dir, MAX_REPLY_FILES)
        .into_iter()
        .filter(|(_, kind)| *kind == FileKind::File)
        .map(|(name, _)| name);
    let (index_name, failed) = current_index(names)?;
    let index_path = dir.join(String::from_utf8_lossy(&index_name).as_ref());
    let index_mtime = reader
        .check(&index_path)
        .ok()
        .and_then(|path| reader.host().metadata(&path, Follow::No).ok())
        .map(|meta| meta.mtime);
    let mut data = FileApiData {
        index_mtime,
        failed_generation: failed,
        ..FileApiData::default()
    };
    let index = match reader.json(&index_path) {
        Ok(index) => index,
        Err(error) => {
            data.warnings
                .push(format!("Cannot read the File API index. {error}").into());
            return Some(data);
        }
    };
    let cmake = index.get("cmake");
    data.generator = text(
        cmake
            .and_then(|c| c.get("generator"))
            .and_then(|g| g.get("name")),
    );
    data.cmake_path = text(
        cmake
            .and_then(|c| c.get("paths"))
            .and_then(|p| p.get("cmake")),
    );
    data.version = text(
        cmake
            .and_then(|c| c.get("version"))
            .and_then(|v| v.get("string")),
    );
    if failed {
        data.warnings.push(
            "The last generation failed. The File API keeps only an error index for it.".into(),
        );
        return Some(data);
    }
    let refs = references(&index);
    let mut reply = Reply {
        reader,
        dir,
        warnings: Vec::new(),
        files: 0,
    };
    if let Some((_, file)) = refs.get(b"cache".as_slice())
        && let Some(cache) = reply.object(file)
    {
        data.cache = Some(cache_entries(&cache));
    }
    if let Some((_, file)) = refs.get(b"toolchains".as_slice())
        && let Some(toolchains) = reply.object(file)
    {
        data.toolchains = Some(toolchain_entries(&toolchains));
    }
    if let Some((_, file)) = refs.get(b"cmakeFiles".as_slice())
        && let Some(files) = reply.object(file)
    {
        data.inputs = Some(
            items(files.get("inputs"))
                .iter()
                .filter_map(|input| text(input.get("path")))
                .collect(),
        );
    }
    if let Some((_, file)) = refs.get(b"codemodel".as_slice())
        && let Some(codemodel) = reply.object(file)
    {
        let msvc = data.toolchains.as_ref().is_some_and(|toolchains| {
            toolchains
                .iter()
                .any(|toolchain| toolchain.frontend.as_deref().is_some_and(|f| f == b"MSVC"))
        });
        codemodel_targets(&mut reply, &codemodel, msvc, &mut data);
    }
    data.warnings.append(&mut reply.warnings);
    Some(data)
}

fn cache_entries(cache: &Value) -> BTreeMap<BString, CacheRecord> {
    items(cache.get("entries"))
        .iter()
        .filter_map(|entry| {
            let help = items(entry.get("properties"))
                .iter()
                .find(|property| {
                    property.get("name").and_then(Value::as_bytes) == Some(b"HELPSTRING")
                })
                .and_then(|property| text(property.get("value")))
                .unwrap_or_default();
            Some((
                text(entry.get("name"))?,
                CacheRecord {
                    ty: text(entry.get("type")).unwrap_or_default(),
                    value: text(entry.get("value")).unwrap_or_default(),
                    help,
                },
            ))
        })
        .collect()
}

fn toolchain_entries(toolchains: &Value) -> Vec<ToolchainSnapshot> {
    items(toolchains.get("toolchains"))
        .iter()
        .filter_map(|toolchain| {
            let compiler = toolchain.get("compiler");
            let field = |name| text(compiler.and_then(|c| c.get(name)));
            let id = field("id");
            let frontend = id.as_ref().map(|id| {
                BString::from(if id.as_slice() == b"MSVC" {
                    "MSVC"
                } else {
                    "GNU"
                })
            });
            Some(ToolchainSnapshot {
                language: text(toolchain.get("language"))?,
                compiler_id: id,
                version: field("version"),
                path: field("path"),
                target: field("target"),
                frontend,
            })
        })
        .collect()
}

fn split(fragments: impl Iterator<Item = BString>, msvc: bool) -> Vec<BString> {
    crate::compdb::group_pairs(fragments.flat_map(|fragment| {
        Arguments::new(&fragment, msvc)
            .take(65_536)
            .collect::<Vec<_>>()
    }))
}

fn codemodel_targets(
    reply: &mut Reply<'_, '_>,
    codemodel: &Value,
    msvc: bool,
    data: &mut FileApiData,
) {
    data.source_root = text(codemodel.get("paths").and_then(|paths| paths.get("source")));
    let mut configs = Vec::new();
    let mut targets = BTreeMap::new();
    let mut count = 0_usize;
    for configuration in items(codemodel.get("configurations")) {
        let config = text(configuration.get("name")).unwrap_or_default();
        configs.push(config.clone());
        let listed = items(configuration.get("targets"));
        let names: HashMap<BString, BString> = listed
            .iter()
            .filter_map(|target| Some((text(target.get("id"))?, text(target.get("name"))?)))
            .collect();
        for entry in listed {
            count += 1;
            if count > MAX_TARGETS {
                data.targets_partial = Some("The File API lists more than 100000 targets.".into());
                break;
            }
            let (Some(name), Some(file)) = (text(entry.get("name")), text(entry.get("jsonFile")))
            else {
                continue;
            };
            let Some(target) = reply.object(&file) else {
                data.targets_partial = Some(
                    format!(
                        "Target {} has no readable reply file.",
                        String::from_utf8_lossy(&name)
                    )
                    .into(),
                );
                continue;
            };
            let root = data.source_root.as_deref().map(|root| root.as_slice());
            targets.insert(
                (config.clone(), name),
                target_snapshot(&target, &names, msvc, root),
            );
        }
    }
    data.configs = Some(configs);
    data.targets = Some(targets);
}

fn backtrace(target: &Value) -> Option<BString> {
    let node = usize::try_from(target.get("backtrace")?.as_u64()?).ok()?;
    let graph = target.get("backtraceGraph")?;
    let node = items(graph.get("nodes")).get(node)?;
    let file = usize::try_from(node.get("file")?.as_u64()?).ok()?;
    let file = text(items(graph.get("files")).get(file))?;
    let line = node.get("line").and_then(Value::as_u64).unwrap_or(0);
    Some(format!("{}:{line}", String::from_utf8_lossy(&file)).into())
}

/// Return a source path as an absolute path. The File API gives paths under the
/// source root relative to it.
fn source_path(path: BString, root: Option<&[u8]>) -> BString {
    let absolute = path.starts_with(b"/") || path.get(1) == Some(&b':');
    match root {
        Some(root) if !absolute => [root, b"/", path.as_slice()].concat().into(),
        _ => path,
    }
}

fn target_snapshot(
    target: &Value,
    names: &HashMap<BString, BString>,
    msvc: bool,
    root: Option<&[u8]>,
) -> TargetSnapshot {
    let groups: Vec<CompileSettings> = items(target.get("compileGroups"))
        .iter()
        .map(|group| CompileSettings {
            language: text(group.get("language")),
            standard: text(
                group
                    .get("languageStandard")
                    .and_then(|s| s.get("standard")),
            ),
            defines: items(group.get("defines"))
                .iter()
                .filter_map(|define| text(define.get("define")))
                .collect(),
            includes: items(group.get("includes"))
                .iter()
                .filter_map(|include| {
                    Some(IncludeDir {
                        path: text(include.get("path"))?,
                        system: include
                            .get("isSystem")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect(),
            fragments: split(
                items(group.get("compileCommandFragments"))
                    .iter()
                    .filter_map(|fragment| text(fragment.get("fragment"))),
                msvc,
            ),
            opaque: false,
        })
        .collect();
    let sources = items(target.get("sources"))
        .iter()
        .filter_map(|source| {
            let group = usize::try_from(source.get("compileGroupIndex")?.as_u64()?).ok()?;
            Some((
                source_path(text(source.get("path"))?, root),
                groups.get(group)?.clone(),
            ))
        })
        .collect();
    let fragments = |step: Option<&Value>| -> Option<Vec<LinkFragment>> {
        let step = step?;
        Some(
            items(step.get("commandFragments"))
                .iter()
                .filter_map(|fragment| {
                    Some(LinkFragment {
                        role: text(fragment.get("role")).unwrap_or_else(|| "flags".into()),
                        fragment: text(fragment.get("fragment"))?,
                    })
                })
                .collect(),
        )
    };
    TargetSnapshot {
        kind: text(target.get("type")),
        attribution: Attribution::Exact,
        sources,
        link: fragments(target.get("link")).or_else(|| fragments(target.get("archive"))),
        dependencies: items(target.get("dependencies"))
            .iter()
            .filter_map(|dependency| {
                let id = text(dependency.get("id"))?;
                Some(names.get(&id).cloned().unwrap_or(id))
            })
            .collect::<BTreeSet<_>>(),
        artifacts: items(target.get("artifacts"))
            .iter()
            .filter_map(|artifact| text(artifact.get("path")))
            .collect(),
        backtrace: backtrace(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_index_spans_both_prefixes() {
        let names = [
            "index-2026-11-01T10-00-00-0000.json",
            "error-2026-11-02T10-00-00-0000.json",
            "codemodel-v2-abc.json",
        ]
        .map(BString::from);
        assert_eq!(
            current_index(names.clone()),
            Some(("error-2026-11-02T10-00-00-0000.json".into(), true))
        );
        let newer = [
            "error-2026-11-02T10-00-00-0000.json",
            "index-2026-11-03T10-00-00-0000.json",
        ]
        .map(BString::from);
        assert_eq!(
            current_index(newer),
            Some(("index-2026-11-03T10-00-00-0000.json".into(), false))
        );
        assert_eq!(current_index([BString::from("x.json")]), None);
    }

    #[test]
    fn references_use_objects_and_client_replies_with_version_checks() {
        let index = rcmake_json::parse(
            br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":3},"jsonFile":"cm-3.json"},
                            {"kind":"codemodel","version":{"major":3,"minor":0},"jsonFile":"cm-v3.json"},
                            {"kind":"cache","version":{"major":1,"minor":0},"jsonFile":"old-cache.json"}],
                 "reply":{"client-ide":{"toolchains-v1":{"kind":"toolchains","version":{"major":1,"minor":0},"jsonFile":"tc.json"}}}}"#,
            &rcmake_json::ReadOptions::default(),
        )
        .expect("json");
        let refs = references(&index);
        assert_eq!(
            refs.get(b"codemodel".as_slice()).map(|r| r.1.clone()),
            Some("cm-3.json".into())
        );
        assert!(!refs.contains_key(b"cache".as_slice()));
        assert_eq!(
            refs.get(b"toolchains".as_slice()).map(|r| r.1.clone()),
            Some("tc.json".into())
        );
    }

    #[test]
    fn plain_names_reject_paths() {
        for bad in [
            "../x.json",
            "/etc/passwd",
            "a/b.json",
            "C:\\x.json",
            "..",
            "",
        ] {
            assert!(!plain_name(bad.as_bytes()), "{bad}");
        }
        assert!(plain_name(b"target-app-Debug-1234.json"));
    }
}
