use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext};
use rcmake_model::{BT, DirId, Target, TargetId, TargetKind};

use super::{UsageResolver, handle, host::definition, on};
use crate::{absolute_path, lower, sources::external_object};

struct Artifact {
    path: BString,
    prefix: BString,
    base: BString,
    suffix: BString,
}

impl Artifact {
    fn from_path(path: BString, prefix: &[u8], suffix: &[u8]) -> Self {
        let name = Lexical::new(&path).filename();
        let prefix = if name.starts_with(prefix) {
            prefix
        } else {
            b""
        };
        let base = name.strip_prefix(prefix).unwrap_or(name);
        let suffix = if base.ends_with(suffix) { suffix } else { b"" };
        let base = base.strip_suffix(suffix).unwrap_or(base);
        Self {
            prefix: prefix.into(),
            base: base.into(),
            suffix: suffix.into(),
            path,
        }
    }

    fn part(self, part: ArtifactPart) -> BString {
        match part {
            ArtifactPart::Full => self.path,
            ArtifactPart::Dir => Lexical::new(&self.path).parent().into(),
            ArtifactPart::Name => Lexical::new(&self.path).filename().into(),
            ArtifactPart::Prefix => self.prefix,
            ArtifactPart::BaseName => self.base,
            ArtifactPart::Suffix => self.suffix,
        }
    }
}

impl UsageResolver<'_> {
    /// Compute an artifact path or name part for a target and configuration.
    #[must_use]
    pub fn artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        self.output_artifact(id, kind, config)
            .map(|artifact| artifact.part(part))
    }

    fn output_artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        config: &ConfigId,
    ) -> Option<Artifact> {
        use ArtifactKind::*;
        use TargetKind::*;
        let target = self.model.target(id)?;
        let variable = |name: &[u8]| definition(self.model, target.dir, name);
        let system = variable(b"CMAKE_SYSTEM_NAME").unwrap_or_default();
        let apple = matches!(
            system.as_slice(),
            b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
        ) || (system.is_empty() && cfg!(target_os = "macos"));
        let windows = system.starts_with(b"Windows") || system == "CYGWIN";
        let import = matches!(kind, ImportFile | LinkerImportFile | SonameImportFile)
            || (kind == LinkerFile && windows && matches!(target.kind, SharedLibrary | Executable));
        let (category, stem, prefix, suffix) = match (target.kind, import) {
            (StaticLibrary, false) => ("ARCHIVE", "STATIC_LIBRARY", "lib", ".a"),
            (SharedLibrary, false) if windows => ("RUNTIME", "SHARED_LIBRARY", "", ".dll"),
            (SharedLibrary, false) if apple => ("LIBRARY", "SHARED_LIBRARY", "lib", ".dylib"),
            (SharedLibrary, false) => ("LIBRARY", "SHARED_LIBRARY", "lib", ".so"),
            (ModuleLibrary, false) => (
                "LIBRARY",
                "SHARED_MODULE",
                "lib",
                if windows { ".dll" } else { ".so" },
            ),
            (Executable, false) => (
                "RUNTIME",
                "EXECUTABLE",
                "",
                if windows { ".exe" } else { "" },
            ),
            (SharedLibrary | Executable, true) if windows || apple || target.is_imported() => {
                if apple && !target.is_imported() && !on(target, b"ENABLE_EXPORTS") {
                    return None;
                }
                (
                    "ARCHIVE",
                    "IMPORT_LIBRARY",
                    if apple { "lib" } else { "" },
                    if apple { ".tbd" } else { ".lib" },
                )
            }
            (UnknownLibrary, false) if target.is_imported() => {
                ("LIBRARY", "SHARED_LIBRARY", "", "")
            }
            _ => return None,
        };
        if matches!(kind, LinkerFile | LinkerLibraryFile | LinkerImportFile)
            && (target.kind == ModuleLibrary
                || (target.kind == Executable && !on(target, b"ENABLE_EXPORTS")))
        {
            return None;
        }
        if matches!(kind, SonameFile | SonameImportFile) && target.kind != SharedLibrary {
            return None;
        }
        let prefix_prop = if import {
            b"IMPORT_PREFIX".as_slice()
        } else {
            b"PREFIX"
        };
        let suffix_prop = if import {
            b"IMPORT_SUFFIX".as_slice()
        } else {
            b"SUFFIX"
        };
        let prefix = target
            .props
            .get(prefix_prop)
            .cloned()
            .or_else(|| variable(format!("CMAKE_{stem}_PREFIX").as_bytes()))
            .unwrap_or_else(|| prefix.into());
        let suffix = target
            .props
            .get(suffix_prop)
            .cloned()
            .or_else(|| variable(format!("CMAKE_{stem}_SUFFIX").as_bytes()))
            .unwrap_or_else(|| suffix.into());
        if target.is_imported() {
            if matches!(kind, PdbFile | BundleDir | BundleContentDir) {
                return None;
            }
            let property = if import {
                b"IMPORTED_IMPLIB".as_slice()
            } else {
                b"IMPORTED_LOCATION"
            };
            let (path, selected) = imported_property(target, property, config)?;
            let path = if matches!(kind, SonameFile | SonameImportFile) {
                let name = imported_for_config(target, b"IMPORTED_SONAME", &selected)
                    .or_else(|| on(target, b"IMPORTED_NO_SONAME").then(|| path.clone()))?;
                Lexical::new(Lexical::new(&path).parent())
                    .append(name.strip_prefix(b"@rpath/").unwrap_or(&name))
            } else {
                path
            };
            return Some(Artifact {
                path,
                prefix,
                suffix,
                base: target.name.clone(),
            });
        }

        let config_upper = BString::from(config.0.to_ascii_uppercase());
        let bundle = kind == BundleDir || kind == BundleContentDir;
        let app = apple && target.kind == Executable && on(target, b"MACOSX_BUNDLE");
        let framework = apple && target.kind.is_library() && on(target, b"FRAMEWORK");
        if (bundle && !(app || framework))
            || (kind == PdbFile && !windows)
            || (kind == SonameFile && (windows || on(target, b"NO_SONAME")))
        {
            return None;
        }
        let category = if kind == PdbFile { "PDB" } else { category };
        let name = self
            .artifact_property(
                id,
                config,
                &[
                    format!("{category}_OUTPUT_NAME_{config_upper}"),
                    format!("{category}_OUTPUT_NAME"),
                    format!("OUTPUT_NAME_{config_upper}"),
                    format!("{config_upper}_OUTPUT_NAME"),
                    "OUTPUT_NAME".into(),
                ],
            )
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| target.name.clone());
        let postfix = target
            .props
            .get(format!("{config_upper}_POSTFIX").as_bytes());
        let base: BString = if target.kind != Executable || app {
            [
                name.as_slice(),
                postfix.map_or(b"".as_slice(), |value| value.as_slice()),
            ]
            .concat()
            .into()
        } else {
            name
        };
        let binary = Lexical::new(&self.model.dir(target.dir).binary_dir);
        let config_dir = format!("{category}_OUTPUT_DIRECTORY_{config_upper}");
        let general_dir = format!("{category}_OUTPUT_DIRECTORY");
        let configured = !config.0.is_empty() && target.props.get(config_dir.as_bytes()).is_some();
        let general = target.props.get(general_dir.as_bytes());
        let mut dir = self
            .artifact_property(id, config, &[config_dir, general_dir])
            .map_or_else(
                || self.model.dir(target.dir).binary_dir.clone(),
                |path| binary.append(&path),
            );
        if self
            .model
            .generator
            .is_some_and(|generator| generator.multi_config())
            && !config.0.is_empty()
            && !configured
            && !general.is_some_and(|value| value.windows(2).any(|part| part == b"$<"))
        {
            dir = Lexical::new(&dir).append(&config.0);
        }
        let dir = Lexical::new(&dir).normalize();

        let (prefix, suffix) = if framework || kind == PdbFile {
            (
                BString::default(),
                BString::from(if kind == PdbFile { ".pdb" } else { "" }),
            )
        } else {
            (prefix, suffix)
        };
        let version = target.props.get(b"VERSION");
        let soversion = target.props.get(b"SOVERSION");
        let version = if target.kind == SharedLibrary
            && !framework
            && !windows
            && !on(target, b"NO_SONAME")
        {
            match kind {
                File | ImportFile => version.or(soversion),
                SonameFile | SonameImportFile => soversion.or(version),
                _ => None,
            }
        } else {
            None
        };
        let p = prefix.as_slice();
        let n = base.as_slice();
        let s = suffix.as_slice();
        let file = match version {
            Some(version) if apple => [p, n, b".", version.as_slice(), s].concat(),
            Some(version) => [p, n, s, b".", version.as_slice()].concat(),
            None => [p, n, s].concat(),
        };
        let subdir = if app {
            [n, b".app"].concat()
        } else if framework {
            [n, b".framework"].concat()
        } else {
            Vec::new()
        };
        let mut path = Lexical::new(&dir).append(&subdir);
        if kind == BundleDir {
            return Some(Artifact::from_path(path, b"", b""));
        }
        if app && (system == "Darwin" || system.is_empty()) {
            path = Lexical::new(&path).append(b"Contents");
        }
        if kind == BundleContentDir {
            return Some(Artifact::from_path(path, b"", b""));
        }
        if app && (system == "Darwin" || system.is_empty()) {
            path = Lexical::new(&path).append(b"MacOS");
        } else if framework && (system == "Darwin" || system.is_empty()) && kind != LinkerFile {
            let version = target
                .props
                .get(b"FRAMEWORK_VERSION")
                .or(target.props.get(b"VERSION"));
            let version = version.map_or(b"A".as_slice(), |value| value.as_slice());
            path = Lexical::new(&Lexical::new(&path).append(b"Versions")).append(version);
        }
        path = Lexical::new(&path).append(&file);
        Some(Artifact {
            path,
            prefix,
            base,
            suffix,
        })
    }

    fn artifact_property(
        &self,
        id: TargetId,
        config: &ConfigId,
        names: &[String],
    ) -> Option<BString> {
        let target = self.model.target(id)?;
        let (name, value) = names
            .iter()
            .find_map(|name| target.props.get(name.as_bytes()).map(|value| (name, value)))?;
        if !value.windows(2).any(|window| window == b"$<") {
            return Some(value.clone());
        }
        let ctx = EvalContext {
            head: Some(handle(id)),
            config: config.clone(),
            ..Default::default()
        };
        let entry = BT::new(value.clone(), target.backtrace.clone());
        Self::new(self.model)
            .eval_entry(id, target.dir, name.as_bytes(), &entry, &ctx, false)
            .ok()
            .map(|(value, _)| value)
    }

    /// Compute the object directory used by the selected generator.
    #[must_use]
    pub fn object_directory(&self, id: TargetId, config: &ConfigId) -> Option<BString> {
        let target = self.model.target(id)?;
        let path = Lexical::new(&self.model.dir(target.dir).binary_dir)
            .append(format!("CMakeFiles/{}.dir", target.name).as_bytes());
        if self
            .model
            .generator
            .is_some_and(|generator| generator.multi_config())
            && !config.0.is_empty()
        {
            Some(Lexical::new(&path).append(&config.0))
        } else {
            Some(path)
        }
    }

    /// Compute the object library file list, with an optional source filter.
    #[must_use]
    pub fn target_objects(
        &self,
        id: TargetId,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        let target = self.model.target(id)?;
        if target.kind != TargetKind::ObjectLibrary {
            return None;
        }
        if target.is_imported() {
            return imported_property(target, b"IMPORTED_OBJECTS", config).map(|(value, _)| value);
        }
        if let Some(value) = target
            .props
            .get(b"TARGET_OBJECTS")
            .filter(|_| sources.is_none())
        {
            return Some(value.clone());
        }
        let dir = self.model.dir(target.dir);
        let ctx = EvalContext {
            config: config.clone(),
            head: Some(handle(id)),
            ..Default::default()
        };
        // Keep the active property chain when TARGET_OBJECTS queries another target.
        let mut resolver = Self {
            source_paths: self.source_paths,
            active: self.active.clone(),
            ..Self::new(self.model)
        };
        let value = resolver.collect_property(id, b"SOURCES", &ctx).ok()?;
        let path = |entry: &[u8]| {
            self.source_paths
                .and_then(|paths| paths.find(dir, entry))
                .unwrap_or_else(|| absolute_path(&dir.source_dir, entry))
        };
        let source_filter = sources.map(|values| {
            list::split(values, list::Empty::Drop)
                .into_iter()
                .map(|source| path(&source))
                .collect::<IndexSet<_>>()
        });
        let binary_root = &self.model.dir(DirId::ROOT).binary_dir;
        let mut file_set_sources = Vec::new();
        for (_, set) in resolver.source_file_sets(id, &ctx).ok()? {
            let owner = self.model.target(set.target)?;
            let dir = self.model.dir(owner.dir);
            for entry in resolver
                .file_set_paths(set.target, b"SOURCES", &set.files, &ctx)
                .ok()?
            {
                let source = self
                    .source_paths
                    .and_then(|paths| paths.find(dir, &entry.value))
                    .unwrap_or_else(|| absolute_path(&dir.source_dir, &entry.value));
                file_set_sources.push((
                    source,
                    set.props
                        .get(b"LANGUAGE")
                        .is_some_and(|value| !value.is_empty()),
                ));
            }
        }
        let files = value
            .entries
            .into_iter()
            .map(|entry| (path(&entry.value), false))
            .chain(file_set_sources)
            .filter(|(source, _)| {
                source_filter
                    .as_ref()
                    .is_none_or(|filter| filter.contains(source))
            })
            .filter(|(source, explicit_language)| {
                !external_object(dir, source)
                    && (*explicit_language
                        || lower::source_language(self.model, target, source).is_some())
            })
            .map(|(source, _)| {
                let object = lower::object_path(self.model, target, config, &source);
                absolute_path(binary_root, &object)
            })
            .collect::<IndexSet<_>>();
        Some(list::join(files))
    }
}

fn imported_for_config(target: &Target, property: &[u8], config: &[u8]) -> Option<BString> {
    if config.is_empty() {
        return target.props.get(property).cloned();
    }
    target
        .props
        .get(&[property, b"_", config].concat())
        .cloned()
}

fn imported_property(
    target: &Target,
    property: &[u8],
    config: &ConfigId,
) -> Option<(BString, BString)> {
    let config: BString = if config.0.is_empty() {
        "NOCONFIG".into()
    } else {
        config.0.to_ascii_uppercase().into()
    };
    let map = [b"MAP_IMPORTED_CONFIG_", config.as_slice()].concat();
    let select = |config: BString| {
        imported_for_config(target, property, &config)
            .filter(|value| !value.is_empty())
            .map(|value| (value, config))
    };
    if let Some(mapped) = target.props.get(&map) {
        return list::split(mapped, list::Empty::Keep)
            .into_iter()
            .map(|config| config.to_ascii_uppercase().into())
            .find_map(select)
            .or_else(|| {
                mapped
                    .is_empty()
                    .then(|| select(BString::default()))
                    .flatten()
            });
    }
    select(config)
        .or_else(|| select(BString::default()))
        .or_else(|| {
            let available = target.props.get(b"IMPORTED_CONFIGURATIONS")?;
            list::split(available, list::Empty::Drop)
                .into_iter()
                .map(|config| config.to_ascii_uppercase().into())
                .find_map(select)
        })
}
