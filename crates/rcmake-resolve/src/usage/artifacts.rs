use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError};
use rcmake_model::{BT, DirId, FileSetKind, Target, TargetId, TargetKind};

use super::{UsageResolver, handle, on};
use crate::{
    ArtifactSet, ResolveError,
    ide::{BundleKind, IdeFlavor, OutputNaming},
    lower,
    lower::definition,
    sources::absolute_path,
    sources::{external_object, properties},
};

struct Artifact {
    path: BString,
    output_dir: BString,
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
            output_dir: Lexical::new(&path).parent().into(),
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
    pub(crate) fn compile_framework_targets(
        &mut self,
        id: TargetId,
        context: &EvalContext,
    ) -> Result<Vec<TargetId>, ResolveError> {
        self.compatibility_targets(id, context)
    }

    /// Compute an artifact path or name part for a target and configuration.
    #[must_use]
    pub fn artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        self.checked_artifact(id, kind, part, config).ok().flatten()
    }

    pub(super) fn checked_artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, ResolveError> {
        Ok(self
            .output_artifact(id, kind, part, config)?
            .map(|artifact| artifact.part(part)))
    }

    pub(super) fn expression_artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, ResolveError> {
        let Some(mut artifact) = self.output_artifact(id, kind, part, config)? else {
            return Ok(None);
        };
        if matches!(
            kind,
            ArtifactKind::File | ArtifactKind::LinkerFile | ArtifactKind::LinkerLibraryFile
        ) && Lexical::new(&artifact.path)
            .filename()
            .ends_with(b".xcframework")
            && let Some(paths) = self.source_paths()
            && let Some(target) = self
                .model
                .target(id)
                .filter(|target| target.is_imported() && lower::is_darwin(self.model, target.dir))
        {
            let root = Lexical::new(&artifact.path).normalize();
            let origin = BT::new(root.clone(), target.backtrace.clone());
            let slice = paths
                .xcframeworks
                .select(self.model, paths, target, &root, &origin)?;
            artifact = Artifact::from_path(slice.library, &artifact.prefix, &artifact.suffix);
        }
        Ok(Some(artifact.part(part)))
    }

    fn output_artifact(
        &self,
        id: TargetId,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<Artifact>, ResolveError> {
        use ArtifactKind::*;
        use TargetKind::*;
        let Some(target) = self.model.target(id) else {
            return Ok(None);
        };
        let variable = |name: &[u8]| definition(self.model, target.dir, name).cloned();
        let mut system = variable(b"CMAKE_SYSTEM_NAME").unwrap_or_default();
        if matches!(system.as_slice(), b"" | b"Darwin")
            && let Some(sdk) = variable(b"CMAKE_OSX_SYSROOT")
        {
            let sdk = Lexical::new(&sdk).filename().to_ascii_lowercase();
            for (prefix, platform) in [
                (b"iphone".as_slice(), b"iOS".as_slice()),
                (b"appletv", b"tvOS"),
                (b"watch", b"watchOS"),
                (b"xros", b"visionOS"),
                (b"xrsimulator", b"visionOS"),
            ] {
                if sdk.starts_with(prefix) {
                    system = platform.into();
                    break;
                }
            }
        }
        let platform = ArtifactPlatform::new(
            &system,
            self.ide,
            lower::apple::variant(self.model, target.dir),
        );
        let ArtifactPlatform {
            apple,
            windows,
            visual_studio,
            ..
        } = platform;
        let apple_import = apple
            && target.kind == SharedLibrary
            && on(target, b"ENABLE_EXPORTS")
            && (target.is_imported()
                || definition(self.model, target.dir, b"CMAKE_TAPI")
                    .is_some_and(|value| !value.is_empty() && !value.ends_with(b"-NOTFOUND")))
            && !target
                .props
                .get(b"XCODE_ATTRIBUTE_GENERATE_TEXT_BASED_STUBS")
                .is_some_and(|value| value.eq_ignore_ascii_case(b"NO"));
        let import = matches!(kind, ImportFile | LinkerImportFile | SonameImportFile)
            || (kind == LinkerFile
                && ((windows && matches!(target.kind, SharedLibrary | Executable))
                    || apple_import));
        if apple && import && !apple_import {
            return Ok(None);
        }
        if windows && import && target.kind == Executable && !on(target, b"ENABLE_EXPORTS") {
            return Ok(None);
        }
        let Some(kind_defaults) = artifact_defaults(target, import, apple, windows, visual_studio)
        else {
            return Ok(None);
        };
        if matches!(kind, LinkerFile | LinkerLibraryFile | LinkerImportFile)
            && (target.kind == ModuleLibrary
                || (target.kind == Executable && !on(target, b"ENABLE_EXPORTS")))
        {
            return Ok(None);
        }
        if matches!(kind, SonameFile | SonameImportFile) && target.kind != SharedLibrary {
            return Ok(None);
        }
        let (category, stem, prefix, suffix) = kind_defaults;
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
        if target.is_imported() && part != ArtifactPart::BaseName {
            if matches!(part, ArtifactPart::Prefix | ArtifactPart::Suffix) {
                return Ok(Some(Artifact {
                    prefix,
                    suffix,
                    base: target.name.clone(),
                    path: BString::default(),
                    output_dir: BString::default(),
                }));
            }
            return Ok(imported_artifact(
                target, kind, config, import, prefix, suffix,
            ));
        }

        self.local_artifact(
            id,
            (kind, part),
            config,
            target,
            platform,
            ArtifactNaming {
                import,
                category,
                prefix,
                suffix,
            },
        )
    }

    fn local_artifact(
        &self,
        id: TargetId,
        (kind, part): (ArtifactKind, ArtifactPart),
        config: &ConfigId,
        target: &Target,
        platform: ArtifactPlatform,
        naming: ArtifactNaming,
    ) -> Result<Option<Artifact>, ResolveError> {
        use ArtifactKind::*;
        use TargetKind::*;
        let ArtifactPlatform {
            apple,
            windows,
            desktop_apple,
            ..
        } = platform;
        let ArtifactNaming {
            import,
            category,
            prefix,
            suffix,
        } = naming;
        let config_upper = BString::from(config.0.to_ascii_uppercase());
        let bundle = kind == BundleDir || kind == BundleContentDir;
        let layout = if apple {
            crate::ide::bundle_kind(target)
        } else {
            BundleKind::None
        };
        let app = matches!(layout, BundleKind::App { .. });
        let framework = matches!(layout, BundleKind::Framework { .. });
        let module_bundle = matches!(layout, BundleKind::Bundle { .. });
        if (bundle && !(app || framework || module_bundle))
            || (kind == PdbFile
                && (!windows || !matches!(target.kind, Executable | SharedLibrary | ModuleLibrary)))
            || (kind == SonameFile && (windows || on(target, b"NO_SONAME")))
        {
            return Ok(None);
        }
        if kind == PdbFile {
            let Some(runtime) = self.output_artifact(id, File, ArtifactPart::Full, config)? else {
                return Ok(None);
            };
            let base = self
                .artifact_property(
                    id,
                    config,
                    "PDB_NAME",
                    &[format!("PDB_NAME_{config_upper}"), "PDB_NAME".into()],
                )?
                .filter(|value| !value.is_empty())
                .unwrap_or(runtime.base);
            let dir = if target.props.get(b"PDB_OUTPUT_DIRECTORY").is_some()
                || target
                    .props
                    .get(format!("PDB_OUTPUT_DIRECTORY_{config_upper}").as_bytes())
                    .is_some()
            {
                self.artifact_output_directory(id, config, target, "PDB", &config_upper)?
            } else {
                runtime.output_dir
            };
            let file = [runtime.prefix.as_slice(), &base, b".pdb"].concat();
            return Ok(Some(Artifact::from_path(
                lower::join(&dir, &file),
                &runtime.prefix,
                b".pdb",
            )));
        }
        let nested = app || framework || module_bundle;
        let category = if framework && target.kind == TargetKind::SharedLibrary {
            "LIBRARY"
        } else {
            category
        };
        let dir = if matches!(part, ArtifactPart::Dir | ArtifactPart::Full) {
            self.artifact_output_directory(id, config, target, category, &config_upper)?
        } else {
            BString::default()
        };
        let read_name = matches!(
            part,
            ArtifactPart::Full | ArtifactPart::Name | ArtifactPart::BaseName
        ) || (part == ArtifactPart::Dir && nested);
        let name = if read_name {
            self.artifact_property(
                id,
                config,
                "OUTPUT_NAME",
                &[
                    format!("{category}_OUTPUT_NAME_{config_upper}"),
                    format!("{category}_OUTPUT_NAME"),
                    format!("OUTPUT_NAME_{config_upper}"),
                    format!("{config_upper}_OUTPUT_NAME"),
                    "OUTPUT_NAME".into(),
                ],
            )?
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| target.name.clone())
        } else {
            target.name.clone()
        };
        let postfix = target
            .props
            .get(format!("{config_upper}_POSTFIX").as_bytes())
            .filter(|_| !app);
        let base: BString = if let Some(postfix) = postfix {
            [name.as_slice(), postfix.as_slice()].concat().into()
        } else {
            name
        };
        let (prefix, suffix) = if framework || module_bundle {
            (
                BString::default(),
                BString::from(if framework && import { ".tbd" } else { "" }),
            )
        } else {
            (prefix, suffix)
        };
        let version = target.props.get(b"VERSION");
        let soversion = target.props.get(b"SOVERSION");
        let version = if target.kind == SharedLibrary
            && !framework
            && !module_bundle
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
        let exe_version = (target.kind == Executable && kind == File && !app && !windows)
            .then_some(version.or_else(|| target.props.get(b"VERSION")))
            .flatten()
            .filter(|version| !version.is_empty());
        let file = match version {
            _ if let Some(version) = exe_version => [p, n, s, b"-", version.as_slice()].concat(),
            Some(version) if apple => [p, n, b".", version.as_slice(), s].concat(),
            Some(version) => [p, n, s, b".", version.as_slice()].concat(),
            None => [p, n, s].concat(),
        };
        let subdir = layout.extension().map_or_else(Vec::new, |extension| {
            [n, b".", extension.as_slice()].concat()
        });
        let mut path = Lexical::new(&dir).append(&subdir);
        if kind == BundleDir {
            return Ok(Some(Artifact::from_path(path, b"", b"")));
        }
        if (app || module_bundle) && desktop_apple {
            path = Lexical::new(&path).append(b"Contents");
        }
        if kind == BundleContentDir {
            return Ok(Some(Artifact::from_path(path, b"", b"")));
        }
        if (app || module_bundle) && desktop_apple {
            path = Lexical::new(&path).append(b"MacOS");
        } else if desktop_apple
            && !matches!(kind, LinkerFile | LinkerLibraryFile | LinkerImportFile)
            && let BundleKind::Framework { version, .. } = layout
        {
            path = Lexical::new(&Lexical::new(&path).append(b"Versions")).append(&version);
        }
        path = Lexical::new(&path).append(&file);
        Ok(Some(Artifact {
            output_dir: dir,
            path,
            prefix,
            base,
            suffix,
        }))
    }

    fn artifact_output_directory(
        &self,
        id: TargetId,
        config: &ConfigId,
        target: &Target,
        category: &str,
        config_upper: &BString,
    ) -> Result<BString, ResolveError> {
        let binary = lower::artifacts::model_path(&self.model.dir(target.dir).binary_dir);
        let config_dir = format!("{category}_OUTPUT_DIRECTORY_{config_upper}");
        let general_dir = format!("{category}_OUTPUT_DIRECTORY");
        let configured = !config.0.is_empty() && target.props.get(config_dir.as_bytes()).is_some();
        let general = target.props.get(general_dir.as_bytes());
        let mut dir = self
            .artifact_property(id, config, "OUTPUT_DIRECTORY", &[config_dir, general_dir])?
            .or_else(|| {
                matches!(category, "LIBRARY" | "ARCHIVE" | "RUNTIME")
                    .then(|| {
                        lower::artifacts::legacy_library_output_directory(self.model, target)
                            .cloned()
                    })
                    .flatten()
            })
            .map_or_else(
                || self.model.dir(target.dir).binary_dir.clone(),
                |path| binary.append(&path),
            );
        if (self.ide.is_some()
            || self
                .model
                .generator
                .is_some_and(|generator| generator.multi_config()))
            && !config.0.is_empty()
            && !configured
            && !general.is_some_and(|value| value.windows(2).any(|part| part == b"$<"))
        {
            dir = Lexical::new(&dir).append(&config.0);
        }
        Ok(lower::artifacts::model_path(&dir).normalize())
    }

    pub(crate) fn ide_artifacts(
        &self,
        id: TargetId,
        config: &ConfigId,
    ) -> Result<(ArtifactSet, OutputNaming), ResolveError> {
        use ArtifactKind::*;
        let artifact = |kind| self.output_artifact(id, kind, ArtifactPart::Full, config);
        let main = artifact(File)?;
        let target = self.model.target(id);
        let naming = main
            .as_ref()
            .map_or_else(OutputNaming::default, |file| OutputNaming {
                dir: file.output_dir.clone(),
                prefix: file.prefix.clone(),
                base_name: file.base.clone(),
                suffix: file.suffix.clone(),
                version: target
                    .and_then(|target| target.props.get(b"VERSION"))
                    .cloned(),
                soversion: target
                    .and_then(|target| target.props.get(b"SOVERSION"))
                    .cloned(),
            });
        let path = |kind| Ok::<_, ResolveError>(artifact(kind)?.map(|file| file.path));
        let artifacts = ArtifactSet {
            file: main.map(|file| file.path),
            linker_file: if target.is_some_and(|target| {
                !target.is_imported()
                    && target.kind == TargetKind::SharedLibrary
                    && self.ide != Some(IdeFlavor::VisualStudio)
                    && lower::is_darwin(self.model, target.dir)
            }) {
                path(LinkerLibraryFile)?
            } else {
                path(LinkerFile)?
            },
            soname_file: path(SonameFile)?,
            import_file: path(ImportFile)?,
            pdb_file: path(PdbFile)?,
            object_dir: target
                .filter(|target| target.kind.is_library() || target.kind == TargetKind::Executable)
                .and_then(|_| self.object_directory(id, config))
                .map(|path| lower::build_relative(self.model, &path)),
        };
        Ok((artifacts, naming))
    }

    fn artifact_property(
        &self,
        id: TargetId,
        config: &ConfigId,
        class: &'static str,
        names: &[String],
    ) -> Result<Option<BString>, ResolveError> {
        let target = self.target(id)?;
        let Some((name, value)) = names
            .iter()
            .find_map(|name| target.props.get(name.as_bytes()).map(|value| (name, value)))
        else {
            return Ok(None);
        };
        if !value.windows(2).any(|window| window == b"$<") {
            return Ok(Some(value.clone()));
        }
        let key = (id, config.clone(), BString::from(name.as_str()));
        if self.artifact_properties.contains(&key) {
            return Err(GenexError::ArtifactCycle {
                target: target.name.clone(),
                property: class,
            }
            .into());
        }
        if self.artifact_properties.len() >= super::MAX_WALK_DEPTH {
            return Err(GenexError::TooDeep.into());
        }
        let ctx = EvalContext {
            head: Some(handle(id)),
            config: config.clone(),
            ..Default::default()
        };
        let entry = BT::new(value.clone(), target.backtrace.clone());
        let mut resolver = self.fork_for_artifacts();
        resolver.artifact_properties.push(key);
        let (value, _) =
            resolver.eval_entry(id, target.dir, name.as_bytes(), &entry, &ctx, false)?;
        Ok(Some(value))
    }

    /// Compute the object directory used by the selected generator.
    #[must_use]
    pub fn object_directory(&self, id: TargetId, config: &ConfigId) -> Option<BString> {
        let target = self.model.target(id)?;
        if let Some(flavor) = self.ide {
            return Some(crate::ide::object_directory(
                self.model, target, config, flavor,
            ));
        }
        Some(absolute_path(
            &self.model.dir(DirId::ROOT).binary_dir,
            &lower::object_dir(self.model, target, config),
        ))
    }

    /// Compute the compiled object file list, with an optional source filter.
    #[must_use]
    pub fn target_objects(
        &self,
        id: TargetId,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        let target = self.model.target(id)?;
        if !target.kind.compiles_objects() {
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
        let mut resolver = self.fork_for_artifacts();
        resolver.source_paths = self.source_paths;
        resolver.active.clone_from(&self.active);
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
        let unity = self
            .source_paths
            .and_then(|paths| paths.unity.as_ref())
            .and_then(|plan| plan.get(&id));
        let mut file_set_sources = Vec::new();
        let mut file_set_headers = IndexSet::new();
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
                if set.kind == FileSetKind::Headers {
                    file_set_headers.insert(source);
                    continue;
                }
                file_set_sources.push((
                    source,
                    set.props
                        .get(b"LANGUAGE")
                        .is_some_and(|value| !value.is_empty()),
                ));
            }
        }
        let files = unity
            .into_iter()
            .flat_map(|plan| plan.files.keys())
            .map(|path| (path.clone(), true))
            .chain(
                value
                    .entries
                    .into_iter()
                    .map(|entry| (path(&entry.value), false))
                    .chain(file_set_sources),
            )
            .filter(|(source, _)| {
                source_filter
                    .as_ref()
                    .is_none_or(|filter| filter.contains(source))
            })
            .filter(|(source, explicit_language)| {
                unity
                    .and_then(|plan| plan.file_for_source(source, config))
                    .is_none()
                    && !external_object(dir, source)
                    && !file_set_headers.contains(source)
                    && !properties(dir, source)
                        .get(b"HEADER_FILE_ONLY")
                        .is_some_and(|value| truth::is_on(value))
                    && (*explicit_language
                        || lower::source_language(self.model, target, source).is_some())
            })
            .map(|(source, _)| {
                if let Some(flavor) = self.ide {
                    return crate::ide::object_path(self.model, target, config, &source, flavor);
                }
                let object = unity.and_then(|plan| plan.files.get(&source)).map_or_else(
                    || lower::object_path(self.model, target, config, &source),
                    |file| file.object_path(self.model, target, config, &source),
                );
                absolute_path(binary_root, &object)
            })
            .collect::<IndexSet<_>>();
        Some(list::join(files))
    }
}

#[derive(Clone, Copy)]
struct ArtifactPlatform {
    visual_studio: bool,
    apple: bool,
    windows: bool,
    desktop_apple: bool,
}

impl ArtifactPlatform {
    fn new(system: &[u8], ide: Option<IdeFlavor>, variant: Option<lower::apple::Variant>) -> Self {
        let xcode = ide == Some(IdeFlavor::Xcode);
        let vs = ide == Some(IdeFlavor::VisualStudio);
        let apple = xcode
            || (!vs
                && (matches!(
                    system,
                    b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
                ) || (system.is_empty() && cfg!(target_os = "macos"))));
        let windows = vs || (!xcode && (system.starts_with(b"Windows") || system == b"CYGWIN"));
        let desktop_apple = variant == Some(lower::apple::Variant::Catalyst)
            || system == b"Darwin"
            || system.is_empty()
            || (xcode && !matches!(system, b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"));
        Self {
            visual_studio: vs,
            apple,
            windows,
            desktop_apple,
        }
    }
}

struct ArtifactNaming {
    import: bool,
    category: &'static str,
    prefix: BString,
    suffix: BString,
}

fn artifact_defaults(
    target: &Target,
    import: bool,
    apple: bool,
    windows: bool,
    visual_studio: bool,
) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    use TargetKind::*;
    let (category, stem, prefix, suffix) = match (target.kind, import) {
        (StaticLibrary, false) if visual_studio => ("ARCHIVE", "STATIC_LIBRARY", "", ".lib"),
        (StaticLibrary, false) => ("ARCHIVE", "STATIC_LIBRARY", "lib", ".a"),
        (SharedLibrary, false) if windows => ("RUNTIME", "SHARED_LIBRARY", "", ".dll"),
        (SharedLibrary, false) if apple => ("LIBRARY", "SHARED_LIBRARY", "lib", ".dylib"),
        (SharedLibrary, false) => ("LIBRARY", "SHARED_LIBRARY", "lib", ".so"),
        (ModuleLibrary, false) if windows => ("LIBRARY", "SHARED_MODULE", "", ".dll"),
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
        (UnknownLibrary, false) if target.is_imported() => ("LIBRARY", "SHARED_LIBRARY", "", ""),
        _ => return None,
    };
    Some((category, stem, prefix, suffix))
}

fn imported_artifact(
    target: &Target,
    kind: ArtifactKind,
    config: &ConfigId,
    import: bool,
    prefix: BString,
    suffix: BString,
) -> Option<Artifact> {
    use ArtifactKind::*;
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
        let base = crate::ide::bundle_kind(target)
            .root(&path)
            .filter(|root| {
                name.strip_prefix(b"@rpath/")
                    .unwrap_or(&name)
                    .starts_with(Lexical::new(root).filename())
            })
            .map_or_else(
                || Lexical::new(&path).parent(),
                |root| Lexical::new(root).parent(),
            );
        Lexical::new(base).append(name.strip_prefix(b"@rpath/").unwrap_or(&name))
    } else {
        path
    };
    Some(Artifact {
        output_dir: Lexical::new(&path).parent().into(),
        path,
        prefix,
        suffix,
        base: target.name.clone(),
    })
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
    if advertised_imports(target) {
        let selected = selected_imported_config(target, config)?;
        let path = imported_for_config(target, property, &selected)
            .filter(|path| !path.is_empty())
            .or_else(|| {
                imported_for_config(target, property, b"").filter(|path| !path.is_empty())
            })?;
        return Some((path, selected));
    }
    let select = |config: BString| {
        imported_for_config(target, property, &config)
            .filter(|value| !value.is_empty())
            .map(|value| (value, config))
    };
    if let Some(mapped) = imported_config_map(target, config) {
        return imported_configs(mapped).into_iter().find_map(select);
    }
    select(imported_consumer_config(config))
        .or_else(|| select(BString::default()))
        .or_else(|| {
            let available = target.props.get(b"IMPORTED_CONFIGURATIONS")?;
            list::split(available, list::Empty::Drop)
                .into_iter()
                .map(|config| config.to_ascii_uppercase().into())
                .find_map(select)
        })
}

fn imported_consumer_config(config: &ConfigId) -> BString {
    if config.0.is_empty() {
        "NOCONFIG".into()
    } else {
        config.0.to_ascii_uppercase().into()
    }
}

pub(super) fn imported_config_map<'a>(
    target: &'a Target,
    config: &ConfigId,
) -> Option<&'a BString> {
    target.props.get(
        &[
            b"MAP_IMPORTED_CONFIG_",
            imported_consumer_config(config).as_slice(),
        ]
        .concat(),
    )
}

pub(super) fn imported_from_cps(target: &Target) -> bool {
    target.props.get(b"_RCMAKE_CPS_SOURCE").is_some()
}

fn advertised_imports(target: &Target) -> bool {
    imported_from_cps(target)
        || target.policies.policies.get(PolicyId::new(200)) == PolicyStatus::New
}

fn imported_configs(value: &[u8]) -> Vec<BString> {
    if value.is_empty() {
        return vec![BString::default()];
    }
    list::split(value, list::Empty::Keep)
        .into_iter()
        .map(|config| config.to_ascii_uppercase().into())
        .collect()
}

pub(super) fn selected_imported_config(target: &Target, config: &ConfigId) -> Option<BString> {
    let consumer = imported_consumer_config(config);
    let mapping = imported_config_map(target, config).map(|value| imported_configs(value));
    let advertised = target
        .props
        .get(b"IMPORTED_CONFIGURATIONS")
        .map(|value| imported_configs(value));
    let location = |config: &[u8]| {
        [b"IMPORTED_LOCATION".as_slice(), b"IMPORTED_IMPLIB"]
            .iter()
            .any(|property| {
                imported_for_config(target, property, config).is_some_and(|path| !path.is_empty())
            })
    };
    let interface = target.kind == TargetKind::InterfaceLibrary;
    if advertised_imports(target) {
        let usable = |config: &[u8]| interface || location(config) || location(b"");
        return match (mapping, advertised) {
            (Some(mapped), Some(advertised)) => mapped
                .into_iter()
                .find(|config| advertised.contains(config)),
            (Some(mapped), None) => mapped.into_iter().find(|config| usable(config)),
            (None, Some(advertised)) if advertised.contains(&consumer) => Some(consumer),
            (None, Some(advertised)) => advertised.into_iter().next(),
            (None, None) => usable(&consumer).then_some(consumer),
        };
    }
    if let Some(mapped) = mapping {
        return mapped
            .into_iter()
            .find(|config| location(config))
            .or_else(|| interface.then_some(consumer));
    }
    let advertised = advertised.unwrap_or_default();
    if location(&consumer) {
        return Some(consumer);
    }
    if location(b"") {
        return Some(BString::default());
    }
    advertised
        .iter()
        .find(|config| location(config))
        .cloned()
        .or_else(|| interface.then(|| advertised.into_iter().next_back().unwrap_or(consumer)))
}
