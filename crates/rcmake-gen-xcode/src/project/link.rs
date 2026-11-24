//! Select libraries for the Xcode Frameworks build phase.

use std::{collections::BTreeSet, path::Path};

use rcmake_base::BString;
use rcmake_model::TargetKind;
use rcmake_resolve::{
    LinkFragmentRole, ResolvedTarget,
    ide::{IdeLinkItem, IdeSourceKind},
};

use crate::{
    object::{Isa, Reference, Value},
    settings::{CONFIG_DIR, Product, xcode_flag},
    text,
};

use super::{Project, TargetEntry};

pub(crate) struct PhaseLink {
    pub(crate) item: IdeLinkItem,
    pub(crate) path: String,
    pub(crate) file_type: &'static str,
    pub(crate) source_tree: &'static str,
    pub(crate) weak: bool,
}

impl Project<'_> {
    pub(super) fn remove_object_inputs(
        &mut self,
        target: Option<&ResolvedTarget>,
        files: &mut Vec<Reference>,
    ) {
        let Some(target) = target else {
            return;
        };
        let paths: BTreeSet<_> = target
            .ide
            .iter()
            .flat_map(|ide| &ide.sources)
            .filter(|file| file.kind == IdeSourceKind::Object)
            .flat_map(|file| {
                let original = text(&file.path);
                let configured = object_input(&original, &text(&target.config.0));
                [original, configured]
            })
            .collect();
        let objects = &mut self.objects.entries;
        files.retain(|build| {
            let Some(Value::Ref(file)) = objects
                .get(&(Isa::PBXBuildFile.name(), build.id.clone()))
                .and_then(|object| object.attrs.get("fileRef"))
            else {
                return true;
            };
            let internal = matches!(
                objects.get(&(Isa::PBXFileReference.name(), file.id.clone()))
                    .and_then(|object| object.attrs.get("path")),
                Some(Value::Text(path)) if paths.contains(path)
            );
            if internal {
                objects.remove(&(Isa::PBXBuildFile.name(), build.id.clone()));
            }
            !internal
        });
    }

    pub(super) fn phase_links(
        &self,
        target: &TargetEntry<'_>,
        mode: Option<&BString>,
        xcframeworks: &BTreeSet<BString>,
    ) -> Vec<PhaseLink> {
        let external = mode.is_some_and(|mode| mode == "KNOWN_LOCATION");
        if !matches!(
            target.raw.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) || !mode
            .is_some_and(|mode| matches!(mode.as_slice(), b"KNOWN_LOCATION" | b"BUILT_ONLY"))
        {
            return Vec::new();
        }
        let facts = |config: &String| self.resolved.get(&(target.id, config.clone())).copied();
        let Some(config) = self.configs.first() else {
            return Vec::new();
        };
        let Some(first) = facts(config).and_then(|target| target.ide.as_deref()) else {
            return Vec::new();
        };
        let mut used = BTreeSet::new();
        first
            .link_items
            .iter()
            .filter_map(|item| {
                if !self.configs.iter().all(|config| {
                    facts(config)
                        .and_then(|facts| facts.ide.as_deref())
                        .is_some_and(|facts| facts.link_items.contains(item))
                }) {
                    return None;
                }
                let (path, file_type, source_tree) = match item {
                    IdeLinkItem::Target(id) => {
                        let producer = self.model.target(*id)?;
                        if producer.is_imported()
                            || !matches!(
                                producer.kind,
                                TargetKind::StaticLibrary
                                    | TargetKind::SharedLibrary
                                    | TargetKind::ModuleLibrary
                            )
                        {
                            return None;
                        }
                        if producer.props.names().iter().any(|name| {
                            [
                                b"ARCHIVE_OUTPUT_DIRECTORY".as_slice(),
                                b"LIBRARY_OUTPUT_DIRECTORY",
                                b"RUNTIME_OUTPUT_DIRECTORY",
                            ]
                            .iter()
                            .any(|prefix| name.starts_with(prefix))
                        }) {
                            return None;
                        }
                        let product = |config: &String| {
                            Product::new(
                                self.model,
                                producer,
                                self.resolved.get(&(*id, config.clone())).copied(),
                                config,
                                self.bin,
                            )
                        };
                        let built = product(config);
                        let path = built.file();
                        if !self
                            .configs
                            .iter()
                            .all(|config| product(config).file() == path)
                        {
                            return None;
                        }
                        (path, built.types().1, "BUILT_PRODUCTS_DIR")
                    }
                    IdeLinkItem::File(path) | IdeLinkItem::Framework(path)
                        if external
                            && !xcframeworks
                                .iter()
                                .any(|root| first.xcframeworks.get(root) == Some(path)) =>
                    {
                        let path = text(path);
                        let path = framework_root(&path).unwrap_or(&path);
                        if !Path::new(path).is_absolute() {
                            return None;
                        }
                        let file_type = match Path::new(path).extension()?.to_str()? {
                            "a" => "archive.ar",
                            "dylib" | "so" => "compiled.mach-o.dylib",
                            "tbd" => "sourcecode.text-based-dylib-definition",
                            "framework" => "wrapper.framework",
                            _ => return None,
                        };
                        (path.into(), file_type, "<absolute>")
                    }
                    _ => return None,
                };
                let weak = phase_weak(facts(config), &path)?;
                if !self
                    .configs
                    .iter()
                    .all(|config| phase_weak(facts(config), &path) == Some(weak))
                {
                    return None;
                }
                used.insert((source_tree, path.clone())).then(|| PhaseLink {
                    item: item.clone(),
                    path,
                    file_type,
                    source_tree,
                    weak,
                })
            })
            .collect()
    }
}

fn phase_weak(target: Option<&ResolvedTarget>, library: &str) -> Option<bool> {
    match apple_link_feature(target, library) {
        None | Some("-framework") => Some(false),
        Some("-weak_framework" | "-weak_library" | "-weak-l") => Some(true),
        _ => None,
    }
}

fn apple_link_feature(target: Option<&ResolvedTarget>, library: &str) -> Option<&'static str> {
    let link = target?.link.as_ref()?;
    let name = library_basename(library)?;
    link.fragments
        .iter()
        .filter(|fragment| fragment.role == LinkFragmentRole::Libraries)
        .find_map(|fragment| {
            let value = text(&fragment.text.value);
            let payload = value.strip_prefix("LINKER:").unwrap_or(&value);
            [
                "-weak_framework",
                "-reexport_framework",
                "-needed_framework",
                "-lazy_framework",
                "-weak_library",
                "-reexport_library",
                "-needed_library",
                "-lazy_library",
                "-framework",
                "-weak-l",
                "-reexport-l",
                "-needed-l",
                "-lazy-l",
            ]
            .into_iter()
            .find(|flag| {
                let Some(argument) = payload.strip_prefix(*flag) else {
                    return false;
                };
                let argument = if flag.ends_with("-l") {
                    argument
                } else if let Some(argument) = argument.strip_prefix([',', ' ']) {
                    argument
                } else {
                    return false;
                };
                let argument = argument.trim().trim_matches('\'');
                let argument = argument.strip_prefix("-framework ").unwrap_or(argument);
                library_basename(argument) == Some(name)
            })
        })
}

pub(crate) fn apple_feature_flag(target: Option<&ResolvedTarget>, library: &str) -> Option<String> {
    let flag = apple_link_feature(target, library)?;
    if flag == "-framework" {
        return Some(format!("{flag} {}", xcode_flag(library_basename(library)?)));
    }
    let name = if flag.ends_with("_framework") || flag.ends_with("-l") {
        library_basename(library)?
    } else {
        library
    };
    if flag.ends_with("-l") {
        Some(format!("-Xlinker {}", xcode_flag(&format!("{flag}{name}"))))
    } else {
        Some(format!("-Xlinker {flag} -Xlinker {}", xcode_flag(name)))
    }
}

pub(crate) fn library_basename(path: &str) -> Option<&str> {
    let path = framework_root(path).unwrap_or(path);
    let name = Path::new(path).file_name()?.to_str()?;
    Some(name.strip_suffix(".framework").unwrap_or(name))
}

pub(crate) fn object_input(path: &str, config: &str) -> String {
    let Some((prefix, suffix)) = path.rsplit_once("/Objects-normal/") else {
        return path.into();
    };
    let Some((target_dir, directory)) = prefix.rsplit_once('/') else {
        return path.into();
    };
    if directory != config {
        return path.into();
    }
    format!("{target_dir}/{CONFIG_DIR}/Objects-normal/{suffix}")
}

pub(crate) fn framework_root(path: &str) -> Option<&str> {
    path.match_indices(".framework")
        .find_map(|(start, matched)| {
            let end = start + matched.len();
            let rest = path.get(end..)?;
            if rest.is_empty() || rest.starts_with('/') {
                path.get(..end)
            } else {
                None
            }
        })
}
