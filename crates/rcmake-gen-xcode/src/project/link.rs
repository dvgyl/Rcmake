//! Select libraries for the Xcode Frameworks build phase.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use rcmake_base::{BString, shell::quote};
use rcmake_host::path::to_native;
use rcmake_model::TargetKind;
use rcmake_resolve::{
    BuildStep, LinkFragment, LinkFragmentRole, ResolvedTarget,
    ide::{IdeFacts, IdeLinkItem, IdeSourceKind},
};

use crate::{
    object::{Isa, Reference, Value},
    settings::{CONFIG_DIR, Product, TargetSettings, xcode_flag},
    text,
};

use super::{Project, TargetEntry};

pub(crate) struct PhaseLink {
    pub(crate) item: IdeLinkItem,
    pub(crate) path: String,
    pub(crate) file_type: &'static str,
    pub(crate) source_tree: &'static str,
    pub(crate) weak: bool,
    fragments: BTreeMap<String, usize>,
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
                let configured =
                    configured_object_path(&original, &text(&target.config.0), "Objects-normal");
                std::iter::once(original).chain(configured)
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
        xcode_items(first)
            .filter_map(|(item, feature)| {
                let weak = phase_weak(feature)?;
                if !self.configs.iter().all(|config| {
                    facts(config)
                        .and_then(|facts| facts.ide.as_deref())
                        .is_some_and(|facts| {
                            xcode_items(facts).any(|entry| entry == (item, feature))
                        })
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
                let mut fragments = BTreeMap::new();
                for config in &self.configs {
                    let facts = facts(config)?;
                    if !has_link_features(facts.ide.as_deref()?) {
                        continue;
                    }
                    let library = match item {
                        IdeLinkItem::Target(id) => {
                            let producer = self.resolved.get(&(*id, config.clone()))?;
                            producer
                                .artifacts
                                .linker_file
                                .as_ref()
                                .or(producer.artifacts.file.as_ref())?
                        }
                        IdeLinkItem::File(path) | IdeLinkItem::Framework(path) => path,
                        _ => return None,
                    };
                    let fragment = phase_fragment(facts, library, feature, self.bin)?;
                    fragments.insert(config.clone(), fragment);
                }
                used.insert((source_tree, path.clone())).then(|| PhaseLink {
                    item: item.clone(),
                    path,
                    file_type,
                    source_tree,
                    weak,
                    fragments,
                })
            })
            .collect()
    }
}

fn phase_weak(feature: Option<&BString>) -> Option<bool> {
    match feature.map(|name| name.as_slice()) {
        None | Some(b"FRAMEWORK" | b"XCFRAMEWORK") => Some(false),
        Some(b"WEAK_FRAMEWORK" | b"WEAK_LIBRARY") => Some(true),
        _ => None,
    }
}

fn xcode_items(ide: &IdeFacts) -> impl Iterator<Item = (&IdeLinkItem, Option<&BString>)> {
    let mut feature = None;
    ide.link_items.iter().filter_map(move |item| match item {
        IdeLinkItem::XcodeFeature(name) => {
            feature = Some(name);
            None
        }
        _ => Some((item, feature.take())),
    })
}

fn has_link_features(ide: &IdeFacts) -> bool {
    ide.link_items
        .iter()
        .any(|item| matches!(item, IdeLinkItem::XcodeFeature(_)))
}

fn library_fragments(target: &ResolvedTarget) -> impl Iterator<Item = (usize, &LinkFragment)> {
    target
        .link
        .iter()
        .flat_map(|link| &link.fragments)
        .enumerate()
        .filter(|(_, fragment)| fragment.role == LinkFragmentRole::Libraries)
}

fn phase_fragment(
    target: &ResolvedTarget,
    library: &[u8],
    feature: Option<&BString>,
    bin: &Path,
) -> Option<usize> {
    let mut paths = vec![BString::from(library)];
    if let Ok(relative) = to_native(library).strip_prefix(bin) {
        paths.push(rcmake_host::path::from_native(relative));
    }
    let mut signatures = Vec::new();
    for path in paths {
        let path_text = text(&path);
        let framework =
            library_basename(&path_text).filter(|_| framework_root(&path_text).is_some());
        let flag = match feature.map(|name| name.as_slice()) {
            Some(b"FRAMEWORK") => Some(("-framework", framework?.as_bytes())),
            Some(b"WEAK_FRAMEWORK") => Some(("-weak_framework", framework?.as_bytes())),
            Some(b"WEAK_LIBRARY") => Some(("-weak_library", path.as_slice())),
            _ => {
                signatures.push(quote(&path));
                if let Some(name) = framework {
                    signatures.push(format!("-framework {}", quote(name.as_bytes())).into());
                }
                None
            }
        };
        if let Some((flag, arg)) = flag {
            signatures.push(format!("-Xlinker {flag} -Xlinker {}", quote(arg)).into());
        }
    }
    let mut matches =
        library_fragments(target).filter(|(_, fragment)| signatures.contains(&fragment.text.value));
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

fn xcframework_fragment(target: &ResolvedTarget, root: &BString, bin: &Path) -> Option<usize> {
    let ide = target.ide.as_deref()?;
    let path = ide.xcframeworks.get(root)?;
    let feature = xcode_items(ide).find_map(|(item, feature)| {
        matches!(item, IdeLinkItem::File(file) | IdeLinkItem::Framework(file) if file == path || file == root)
            .then_some(feature)
    })?;
    (phase_weak(feature) == Some(false))
        .then(|| phase_fragment(target, path, feature, bin))
        .flatten()
}

pub(super) fn xcframework_phase_allowed(
    target: &ResolvedTarget,
    root: &BString,
    bin: &Path,
) -> bool {
    target.ide.as_deref().is_some_and(|ide| {
        ide.xcframeworks.contains_key(root)
            && (!has_link_features(ide) || xcframework_fragment(target, root, bin).is_some())
    })
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
    configured_object_path(path, config, "$(OBJECT_FILE_DIR_normal:base)")
        .unwrap_or_else(|| path.into())
}

fn configured_object_path(path: &str, config: &str, objects: &str) -> Option<String> {
    let (prefix, suffix) = path.rsplit_once("/Objects-normal/")?;
    let (target_dir, directory) = prefix.rsplit_once('/')?;
    (directory == config).then(|| format!("{target_dir}/{CONFIG_DIR}/{objects}/{suffix}"))
}

impl TargetSettings<'_> {
    pub(crate) fn rendered_link_flags(&self, ide: &IdeFacts) -> Option<Vec<String>> {
        let target = self.target.filter(|_| has_link_features(ide))?;
        let phase_fragments = self
            .phase_links
            .iter()
            .filter_map(|link| link.fragments.get(self.config).copied())
            .chain(
                self.xcframeworks
                    .iter()
                    .filter_map(|root| xcframework_fragment(target, root, self.bin)),
            )
            .collect::<BTreeSet<_>>();
        let objects = ide
            .sources
            .iter()
            .filter(|source| {
                matches!(
                    source.kind,
                    IdeSourceKind::Object | IdeSourceKind::ExternalObject
                )
            })
            .map(|source| quote(&source.path))
            .collect::<BTreeSet<_>>();
        Some(
            library_fragments(target)
                .filter(|(index, fragment)| {
                    !phase_fragments.contains(index) && !objects.contains(&fragment.text.value)
                })
                .map(|(_, fragment)| {
                    let value = &fragment.text.value;
                    text(value.strip_prefix(b"SHELL:").unwrap_or(value))
                })
                .collect(),
        )
    }

    pub(crate) fn link_objects(&self) -> impl Iterator<Item = String> {
        let inputs = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config.0.as_slice() == self.config.as_bytes())
            .and_then(|graph| {
                let entry = graph
                    .targets
                    .iter()
                    .find(|entry| entry.name == self.raw.name)?;
                entry
                    .steps
                    .iter()
                    .filter_map(|index| graph.steps.get(*index))
                    .find_map(|step| {
                        if let BuildStep::Link { inputs, .. } = step {
                            Some(inputs)
                        } else {
                            None
                        }
                    })
            });
        inputs.into_iter().flatten().filter_map(move |input| {
            let target = self.target?;
            let path = self.bin.join(text(input));
            if target.ide.as_ref().is_some_and(|ide| {
                ide.sources.iter().any(|file| {
                    file.kind == IdeSourceKind::Object
                        && (file.path == *input || path == Path::new(&text(&file.path)))
                })
            }) {
                return None;
            }
            self.result
                .targets
                .iter()
                .any(|dep| {
                    dep.metadata.kind == TargetKind::ObjectLibrary
                        && dep.config == target.config
                        && target.dependencies.contains(&dep.id)
                        && dep.compile_units.iter().any(|unit| unit.object == *input)
                })
                .then(|| object_input(&path.to_string_lossy(), self.config))
        })
    }
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
