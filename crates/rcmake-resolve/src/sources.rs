//! Source paths and their build-time producers.

use std::cell::RefCell;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{
    Backtrace, DiagCategory, Diagnostic, Effect, FrameKind, Location, MessageKind, PolicyId,
    PolicyStatus, RenderOptions, render,
};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_host::{FileKind, Follow, Host, Stream, path::to_native};
use rcmake_model::{
    BT, DirId, Directory, FileSetKind, ProjectModel, PropertyMap, Target, TargetId, TargetKind,
};

use crate::{
    CompileGroup, InterfaceInclude, ResolveError, ResolvedTarget,
    target::FileSetMember,
    usage::{self, UsageResolver},
};

mod generated;

#[derive(Clone, Copy)]
enum SourceUsage {
    Options,
    Definitions,
    Includes,
}

impl SourceUsage {
    const ALL: [Self; 3] = [Self::Options, Self::Definitions, Self::Includes];

    fn property(self) -> &'static [u8] {
        match self {
            Self::Options => b"COMPILE_OPTIONS",
            Self::Definitions => b"COMPILE_DEFINITIONS",
            Self::Includes => b"INCLUDE_DIRECTORIES",
        }
    }

    fn apply(
        self,
        group: &mut CompileGroup,
        source_values: Vec<BT<BString>>,
        set_values: Vec<BT<BString>>,
    ) {
        match self {
            Self::Options => group
                .flags
                .extend(source_values.into_iter().chain(set_values)),
            Self::Definitions => {
                if !source_values.is_empty() || !set_values.is_empty() {
                    group.source_define_start = Some(group.defines.len());
                    group
                        .defines
                        .extend(source_values.into_iter().chain(set_values).filter_map(
                            |mut entry| {
                                if let Some(value) = entry.value.strip_prefix(b"-D") {
                                    entry.value = value.into();
                                }
                                (!entry.value.is_empty()).then_some(entry)
                            },
                        ));
                }
            }
            Self::Includes => {
                let mut seen = IndexSet::new();
                group.includes = set_values
                    .into_iter()
                    .chain(source_values)
                    .map(|path| InterfaceInclude {
                        path,
                        system: false,
                    })
                    .chain(std::mem::take(&mut group.includes))
                    .filter(|entry| seen.insert(entry.path.value.clone()))
                    .collect();
            }
        }
    }
}

fn config_definition_property(config: &ConfigId) -> Vec<u8> {
    [
        b"COMPILE_DEFINITIONS_".as_slice(),
        &config.0.to_ascii_uppercase(),
    ]
    .concat()
}

pub(crate) fn has_source_compile_properties(
    props: SourceProperties<'_>,
    config: &ConfigId,
) -> bool {
    props.get(b"COMPILE_FLAGS").is_some()
        || (!config.0.is_empty() && props.get(&config_definition_property(config)).is_some())
        || SourceUsage::ALL
            .into_iter()
            .any(|usage| props.get(usage.property()).is_some())
}

pub(crate) fn apply_compile_properties(
    usage: &mut UsageResolver<'_>,
    target: TargetId,
    ctx: &EvalContext,
    source: &BT<BString>,
    member: Option<&FileSetMember<'_>>,
    source_props: SourceProperties<'_>,
    group: &mut CompileGroup,
) -> Result<(), ResolveError> {
    let mut ctx = ctx.clone();
    ctx.head = Some(usage::handle(target));
    if let Some(value) = source_props.get(b"COMPILE_FLAGS") {
        let entry = BT::new(value.clone(), source.backtrace.clone());
        let flags = usage.eval_source_compile_flags(target, &entry, &ctx)?;
        append_compile_flags(&mut group.flags, &flags, entry.backtrace);
    }
    for field in SourceUsage::ALL {
        let name = field.property();
        let mut source_values = match source_props.get(name) {
            Some(value) => usage.file_set_paths(
                target,
                name,
                &[BT::new(value.clone(), source.backtrace.clone())],
                &ctx,
            )?,
            None => Vec::new(),
        };
        if matches!(field, SourceUsage::Includes)
            && let Some(include) = source_values
                .iter()
                .find(|entry| !crate::file_is_full_path(&entry.value))
        {
            let file = BString::from(Lexical::new(&source.value).filename());
            let body = BString::from(format!(
                "Found relative path while evaluating include directories of \"{file}\":\n\n  \"{}\"",
                include.value
            ));
            if let Some(paths) = usage.source_paths() {
                paths.emit_with_base(
                    paths.root_source_dir.clone(),
                    MessageKind::Error,
                    Location::FileLine {
                        file: absolute_path(source_props.source_dir, b"CMakeLists.txt")
                            .as_slice()
                            .into(),
                        line: 0,
                    },
                    body.clone(),
                );
                return Err(ResolveError::Reported(body));
            }
            return Err(ResolveError::Message(body));
        }
        if matches!(field, SourceUsage::Definitions) && !ctx.config.0.is_empty() {
            let key = config_definition_property(&ctx.config);
            let configured = source_props
                .get(&key)
                .into_iter()
                .map(|value| BT::new(value.clone(), source.backtrace.clone()))
                .collect::<Vec<_>>();
            source_values.extend(usage.file_set_paths(target, &key, &configured, &ctx)?);
        }
        let set_values = if let Some(member) = member {
            let owner = member.set.target;
            let key = if owner == target {
                name.into()
            } else {
                [b"INTERFACE_", name].concat()
            };
            match member.set.props.get(&key) {
                Some(value) => usage.file_set_paths(
                    owner,
                    &key,
                    &[BT::new(value.clone(), member.source.backtrace.clone())],
                    &ctx,
                )?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        field.apply(group, source_values, set_values);
    }
    Ok(())
}

pub(crate) fn language_for_extension(
    model: &ProjectModel,
    dir: DirId,
    extension: &[u8],
) -> Option<BString> {
    let extension = extension.strip_prefix(b".").unwrap_or(extension);
    let registry = model.languages();
    if let Some(language) = registry.language_for_extension(extension) {
        return Some(language.clone());
    }
    let definition = |key: &[u8]| crate::lower::definition(model, dir, key);
    let defaults = ["C", "CXX", "OBJC", "OBJCXX", "ASM", "Fortran", "CUDA", "RC"];
    let enabled = model.global.props.get(b"ENABLED_LANGUAGES");
    let languages = enabled.map_or_else(
        || defaults.iter().map(|lang| BString::from(*lang)).collect(),
        |value| list::split(value, list::Empty::Drop),
    );
    let mut ignored = registry.is_ignored(extension);
    for language in languages.iter().rev() {
        let key = [b"CMAKE_", language.as_slice(), b"_SOURCE_FILE_EXTENSIONS"].concat();
        if definition(&key)
            .is_some_and(|value| list::iter(value, list::Empty::Drop).any(|item| item == extension))
        {
            return Some(language.clone());
        }
        let key = [b"CMAKE_", language.as_slice(), b"_IGNORE_EXTENSIONS"].concat();
        ignored |= definition(&key).is_some_and(|value| {
            list::iter(value, list::Empty::Drop).any(|item| item == extension)
        });
    }
    if ignored {
        return None;
    }
    // CMake maps an extension only to an enabled language. A source with no
    // enabled language is not compiled, for example AUTOMOC's
    // mocs_compilation.cpp in a C-only project.
    defaults
        .into_iter()
        .filter(|language| languages.iter().any(|enabled| enabled == language))
        .find(|language| {
            if *language == "RC" {
                return extension.eq_ignore_ascii_case(b"rc")
                    && definition(b"CMAKE_RC_COMPILER")
                        .is_some_and(|compiler| !truth::is_off(compiler));
            }
            let key = format!("CMAKE_{language}_SOURCE_FILE_EXTENSIONS");
            if definition(key.as_bytes()).is_some() {
                return false;
            }
            match *language {
                "C" => extension == b"c",
                "CXX" => matches!(
                    extension,
                    b"C" | b"cc" | b"cpp" | b"CPP" | b"cxx" | b"c++" | b"ixx" | b"cppm"
                ),
                "OBJC" => extension == b"m",
                "OBJCXX" => extension == b"mm",
                "ASM" => matches!(extension, b"s" | b"S" | b"asm"),
                "Fortran" => matches!(extension, b"f" | b"F" | b"f90" | b"F90"),
                "CUDA" => extension == b"cu",
                _ => false,
            }
        })
        .map(BString::from)
}

pub(crate) fn append_compile_flags(
    flags: &mut Vec<BT<BString>>,
    value: &[u8],
    backtrace: Backtrace,
) {
    if !value.is_empty() {
        flags.push(BT::new([b"SHELL:", value].concat().into(), backtrace));
    }
}

#[derive(Default)]
pub(crate) struct TargetInputs {
    pub(crate) link_languages: IndexSet<BString>,
    pub(crate) xcframework_inputs: IndexSet<BString>,
    pub(crate) sources: IndexSet<BString>,
    pub(crate) objects: IndexSet<BString>,
    pub(crate) sources_without_pch: IndexSet<BString>,
    pub(crate) position_independent: Option<bool>,
    pub(crate) standard_requirements: IndexMap<BString, &'static [u8]>,
    pub(crate) unity_groups: IndexMap<BString, Option<BString>>,
}

pub(crate) struct SourcePaths<'a> {
    pub(crate) host: &'a dyn Host,
    pub(crate) xcframeworks: crate::xcframework::Plists,
    pub(crate) ide: Option<crate::ide::IdeFlavor>,
    pub(crate) collect_source_errors: bool,
    extension_warnings: RefCell<IndexSet<(BString, BString)>>,
    missing: RefCell<IndexSet<(BString, BString)>>,
    /// Rendered generation warnings, so that a repeated evaluation warns once.
    warnings: RefCell<IndexSet<BString>>,
    file_set_warnings: RefCell<IndexSet<(DirId, BString, BString, usize)>>,
    root_source_dir: Option<BString>,
    absolute_sources: IndexMap<BString, IndexSet<BString>>,
    generated: IndexMap<BString, IndexSet<BString>>,
    objects: IndexMap<BString, TargetId>,
    pub(crate) unity: Option<crate::unity::UnityPlan>,
}

impl<'a> SourcePaths<'a> {
    pub(crate) fn new(host: &'a dyn Host) -> Self {
        Self {
            host,
            xcframeworks: crate::xcframework::Plists::default(),
            ide: None,
            collect_source_errors: true,
            extension_warnings: RefCell::default(),
            missing: RefCell::default(),
            warnings: RefCell::default(),
            file_set_warnings: RefCell::default(),
            root_source_dir: None,
            absolute_sources: IndexMap::new(),
            generated: IndexMap::new(),
            objects: IndexMap::new(),
            unity: None,
        }
    }

    pub(crate) fn index_source_records(&mut self, model: &ProjectModel) {
        self.root_source_dir = Some(model.dir(DirId::ROOT).source_dir.clone());
        for (_, target) in model.targets() {
            let paths = self
                .absolute_sources
                .entry(model.dir(target.dir).binary_dir.clone())
                .or_default();
            paths.extend(
                target
                    .sources
                    .iter()
                    .flat_map(|entry| list::iter(&entry.value, list::Empty::Drop))
                    .filter(|path| {
                        Lexical::new(path).absolute() && !path.windows(2).any(|part| part == b"$<")
                    })
                    .map(|path| Lexical::new(&path).normalize()),
            );
        }
    }

    pub(crate) fn index_generated(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
    ) -> Result<(), ResolveError> {
        self.generated.clear();
        let mut usage = UsageResolver::new(model).with_ide(self.ide);
        let mut errors = Vec::new();
        for (_, command) in model.commands() {
            let (outputs, byproducts) = match crate::lower::checked_command_outputs(
                model, command, config, &mut usage, self.host,
            ) {
                Ok(paths) => paths,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let dir = model.dir(command.dir);
            for path in outputs.into_iter().chain(byproducts) {
                self.generated
                    .entry(path)
                    .or_default()
                    .insert(dir.binary_dir.clone());
            }
        }
        crate::lower::custom::combine_errors(errors)?;
        for (_, dir) in model.dirs() {
            for path in &dir.generated_sources {
                self.generated
                    .entry(path.clone())
                    .or_default()
                    .insert(dir.binary_dir.clone());
            }
        }
        self.index_file_generations(model, config, &mut usage)?;
        Ok(())
    }

    pub(crate) fn index_objects(&mut self, model: &ProjectModel, config: &ConfigId) {
        self.objects.clear();
        let usage = UsageResolver::with_source_paths(model, self);
        self.objects = model
            .targets()
            .filter(|(_, target)| {
                target.kind.compiles_objects()
                    && (!target.is_imported() || target.kind == TargetKind::ObjectLibrary)
            })
            .filter_map(|(id, _)| Some((id, usage.target_objects(id, config, None)?)))
            .flat_map(|(id, objects)| {
                list::split(&objects, list::Empty::Drop)
                    .into_iter()
                    .map(move |path| (Lexical::new(&path).normalize(), id))
            })
            .collect();
    }

    pub(crate) fn object_owner(&self, path: &[u8]) -> Option<TargetId> {
        self.objects.get(&Lexical::new(path).normalize()).copied()
    }

    pub(crate) fn is_generated(&self, dir: &Directory, source: &[u8]) -> bool {
        [&dir.source_dir, &dir.binary_dir]
            .into_iter()
            .map(|base| absolute_path(base, source))
            .any(|path| self.generated_in(dir, &path))
    }

    fn generated_in(&self, dir: &Directory, path: &[u8]) -> bool {
        source_flag(dir, path, b"GENERATED")
            || self.generated.get(path).is_some_and(|producers| {
                producers.contains(&dir.binary_dir)
                    || dir.final_policies.is_some_and(|policies| {
                        [PolicyId::new(118), PolicyId::new(163)]
                            .into_iter()
                            .any(|policy| policies.get(policy) == PolicyStatus::New)
                    })
            })
    }

    pub(crate) fn is_source_dependency(
        &self,
        dir: &Directory,
        source: &BString,
        binary: &BString,
    ) -> bool {
        !self.generated_in(dir, binary)
            && !generated_at_binary_alternative(dir, source, binary)
            && (self.host.metadata(&to_native(source), Follow::Yes).is_ok()
                || dir.source_props.contains_key(source))
    }

    pub(crate) fn find(&self, dir: &Directory, source: &[u8]) -> Option<BString> {
        let source_path = absolute_path(&dir.source_dir, source);
        let binary_path = absolute_path(&dir.binary_dir, source);
        if self.generated_in(dir, &binary_path)
            || self.objects.contains_key(&binary_path)
            || generated_at_binary_alternative(dir, &source_path, &binary_path)
        {
            return Some(binary_path);
        }
        if let Some(path) = dir
            .source_props
            .get(&source_path)
            .and_then(|props| props.get(b"\0rcmake-source-location"))
            && (self.generated_in(dir, path)
                || self.objects.contains_key(path)
                || self
                    .host
                    .metadata(&to_native(path), Follow::Yes)
                    .is_ok_and(|metadata| matches!(metadata.kind, FileKind::File | FileKind::Dir)))
        {
            return Some(path.clone());
        }
        if self.generated_in(dir, &source_path) || self.objects.contains_key(&source_path) {
            return Some(source_path);
        }
        [source_path, binary_path].into_iter().find(|path| {
            self.host
                .metadata(&to_native(path), Follow::Yes)
                .is_ok_and(|metadata| matches!(metadata.kind, FileKind::File | FileKind::Dir))
        })
    }

    fn missing_identity(&self, dir: &Directory, source: &[u8]) -> (BString, BString) {
        let source_path = absolute_path(&dir.source_dir, source);
        if Lexical::new(source).absolute() {
            return (source_path.clone(), source_path);
        }
        let binary_path = absolute_path(&dir.binary_dir, source);
        let props = |path: &BString| {
            dir.source_props
                .get_full(path)
                .filter(|(_, _, props)| props.get(b"\0rcmake-source-absolute").is_some())
        };
        let recorded = [props(&source_path), props(&binary_path)]
            .into_iter()
            .flatten()
            .min_by_key(|(index, _, _)| *index)
            .map(|(_, path, _)| path)
            .or_else(|| {
                let paths = self.absolute_sources.get(&dir.binary_dir)?;
                [&source_path, &binary_path]
                    .into_iter()
                    .filter_map(|path| Some((paths.get_index_of(path)?, path)))
                    .min_by_key(|(index, _)| *index)
                    .map(|(_, path)| path)
            });
        match recorded {
            Some(path) => (path.clone(), path.clone()),
            None => (source_path, source.into()),
        }
    }

    pub(crate) fn is_directory(&self, path: &[u8]) -> bool {
        self.host
            .metadata(&to_native(path), Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::Dir)
    }

    pub(crate) fn missing_sources(&self) -> bool {
        !self.missing.borrow().is_empty()
    }

    pub(crate) fn resolve_at(
        &self,
        model: &ProjectModel,
        dir: &Directory,
        source: &BT<BString>,
    ) -> Result<Option<BString>, ResolveError> {
        if let Some(found) = self.find(dir, &source.value) {
            return Ok(Some(found));
        }
        const EXTENSIONS: &str = "c C c++ cc cpp cxx cu mpp m M mm ixx cppm ccm cxxm c++m h hh h++ hm hpp hxx in txx f F for f77 f90 f95 f03 hip ispc";
        let policy = PolicyId::new(115);
        let status = dir
            .final_policies
            .map_or(PolicyStatus::New, |policies| policies.get(policy));
        let (identity, display_path) = self.missing_identity(dir, &source.value);
        let mut body: BString = [b"Cannot find source file:\n\n  ", display_path.as_slice()]
            .concat()
            .into();
        if status != PolicyStatus::New {
            let extensions = EXTENSIONS.split_ascii_whitespace();
            for base in [&dir.source_dir, &dir.binary_dir] {
                let path = absolute_path(base, &source.value);
                for extension in extensions.clone() {
                    let candidate: BString = [path.as_slice(), b".", extension.as_bytes()]
                        .concat()
                        .into();
                    if self.generated_in(dir, &candidate)
                        || self
                            .host
                            .metadata(&to_native(&candidate), Follow::Yes)
                            .is_ok_and(|metadata| metadata.kind == FileKind::File)
                    {
                        if status == PolicyStatus::Warn
                            && self
                                .extension_warnings
                                .borrow_mut()
                                .insert((dir.binary_dir.clone(), identity))
                        {
                            let warning = Diagnostic { annotations: None,
                                kind: MessageKind::AuthorWarning,
                                effect: Effect::None,
                                category: Some(DiagCategory::Policy),
                                policy: Some(policy),
                                location: generation_source_location(Location::Backtrace(source.backtrace.clone())),
                                body: format!("Policy CMP0115 is not set: Source file extensions must be explicit.  Run \"cmake --help-policy CMP0115\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n\nFile:\n\n  {candidate}").into(),
                            };
                            self.host.write_out(
                                Stream::Stderr,
                                &render(
                                    &warning,
                                    &RenderOptions {
                                        relative_base: Some(
                                            model.dir(DirId::ROOT).source_dir.clone(),
                                        ),
                                        ..RenderOptions::default()
                                    },
                                ),
                            );
                        }
                        return Ok(Some(candidate));
                    }
                }
            }
            body.extend_from_slice(b"\n\nTried extensions");
            for extension in extensions {
                body.extend_from_slice(b" .");
                body.extend_from_slice(extension.as_bytes());
            }
        }
        if source.value == "FILE_SET" {
            body.extend_from_slice(
                b"\nHint: the FILE_SET keyword may only appear after a visibility specifier or another FILE_SET within the target_sources() command.",
            );
        }
        if !self.collect_source_errors {
            return Err(ResolveError::Message(body));
        }
        if self
            .missing
            .borrow_mut()
            .insert((dir.binary_dir.clone(), identity))
        {
            let frames: Vec<_> = source
                .backtrace
                .frames()
                .take_while(|frame| Some(*frame) != dir.backtrace.top())
                .collect();
            let trace = frames
                .into_iter()
                .rev()
                .fold(Backtrace::default(), |trace, frame| {
                    trace.push(frame.clone())
                });
            self.emit(model, MessageKind::Error, Location::Backtrace(trace), body);
        }
        Ok(None)
    }

    pub(crate) fn first_file_set_warning(
        &self,
        target: &rcmake_model::Target,
        body: &BString,
        occurrence: usize,
    ) -> bool {
        self.file_set_warnings.borrow_mut().insert((
            target.dir,
            target.name.clone(),
            body.clone(),
            occurrence,
        ))
    }

    pub(crate) fn emit(
        &self,
        model: &ProjectModel,
        kind: MessageKind,
        location: Location,
        body: BString,
    ) {
        self.emit_with_base(
            Some(model.dir(DirId::ROOT).source_dir.clone()),
            kind,
            location,
            body,
        );
    }

    /// Write a warning once, even when evaluation repeats for each configuration.
    pub(crate) fn warn_once(
        &self,
        model: &ProjectModel,
        kind: MessageKind,
        policy: Option<rcmake_diag::PolicyId>,
        location: Location,
        body: BString,
    ) {
        if let Some(text) = self.render_once(model, kind, policy, location, body) {
            self.host.write_out(Stream::Stderr, &text);
        }
    }

    /// Render a warning, or return `None` when this run already reported the same text.
    pub(crate) fn render_once(
        &self,
        model: &ProjectModel,
        kind: MessageKind,
        policy: Option<rcmake_diag::PolicyId>,
        location: Location,
        body: BString,
    ) -> Option<BString> {
        let options = RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        };
        let diagnostic = Diagnostic {
            annotations: None,
            kind,
            effect: Effect::None,
            category: policy.map(|_| rcmake_diag::DiagCategory::Policy),
            policy,
            location: generation_source_location(location),
            body,
        };
        let text = render(&diagnostic, &options);
        self.warnings
            .borrow_mut()
            .insert(text.clone())
            .then_some(text)
    }

    fn emit_with_base(
        &self,
        relative_base: Option<BString>,
        kind: MessageKind,
        location: Location,
        body: BString,
    ) {
        let options = RenderOptions {
            relative_base,
            ..RenderOptions::default()
        };
        let location = generation_source_location(location);
        let diagnostic = Diagnostic {
            annotations: None,
            kind,
            effect: Effect::None,
            category: None,
            policy: None,
            location,
            body,
        };
        self.host
            .write_out(Stream::Stderr, &render(&diagnostic, &options));
    }

    pub(crate) fn target_error(
        &self,
        model: &ProjectModel,
        target: &Target,
        body: BString,
    ) -> ResolveError {
        self.emit(
            model,
            MessageKind::Error,
            Location::Backtrace(target.backtrace.clone()),
            body.clone(),
        );
        ResolveError::Reported(body)
    }
}

pub(crate) fn assign_file_set_source_groups(
    model: &ProjectModel,
    target: &Target,
    facts: &mut ResolvedTarget,
) {
    let Some(ide) = &mut facts.ide else {
        return;
    };
    for source in &mut ide.sources {
        let Some(name) = &source.file_set else {
            continue;
        };
        if let Some(group) = target
            .file_sets
            .iter()
            .filter_map(|&id| model.file_set(id))
            .find(|set| &set.name == name)
            .and_then(|set| {
                set.props
                    .get(b"SOURCE_GROUP")
                    .or_else(|| set.props.get(b"\0source_group_file_set"))
            })
        {
            source.group.clone_from(group);
        }
    }
}

pub(crate) fn validate_object_sources(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    facts: &ResolvedTarget,
    inputs: &TargetInputs,
    ctx: &EvalContext,
) -> Result<(), ResolveError> {
    if target.kind != TargetKind::ObjectLibrary {
        return Ok(());
    }
    let dir = model.dir(target.dir);
    let mut invalid = inputs
        .sources
        .iter()
        .filter(|source| {
            let extension = Lexical::new(source).stem_extension(true).1;
            (extension.eq_ignore_ascii_case(b".def") || extension.eq_ignore_ascii_case(b".idl"))
                && !external_object(dir, source)
                && !properties(dir, source)
                    .get(b"HEADER_FILE_ONLY")
                    .is_some_and(|value| truth::is_on(value))
                && !facts
                    .compile_units
                    .iter()
                    .any(|unit| unit.source.value == **source)
        })
        .collect::<IndexSet<_>>();
    if invalid.is_empty() {
        return Ok(());
    }
    for (_, set) in usage.source_file_sets(facts.id, ctx)? {
        if set.kind == FileSetKind::Headers
            && let Some(owner) = model.target(set.target)
        {
            let owner_dir = model.dir(owner.dir);
            for file in usage.file_set_paths(set.target, b"SOURCES", &set.files, ctx)? {
                if let Some(path) = paths.find(owner_dir, &file.value) {
                    invalid.shift_remove(&path);
                }
            }
        }
    }
    if invalid.is_empty() {
        return Ok(());
    }
    let mut body = BString::from(format!("OBJECT library \"{}\" contains:\n", target.name));
    for source in invalid {
        body.extend_from_slice(b"\n  ");
        body.extend_from_slice(Lexical::new(source).filename());
    }
    body.extend_from_slice(b"\n\nbut may contain only sources that compile, header files, and other files that would not affect linking of a normal library.");
    Err(paths.target_error(model, target, body))
}

pub(crate) fn external_object(dir: &Directory, path: &BString) -> bool {
    source_flag(dir, path, b"EXTERNAL_OBJECT")
        || matches!(Lexical::new(path).stem_extension(true).1, b".o" | b".obj")
}

#[derive(Clone, Copy)]
pub(crate) struct SourceProperties<'a> {
    source_dir: &'a BString,
    path: Option<&'a PropertyMap>,
    relative: Option<&'a PropertyMap>,
}

impl<'a> SourceProperties<'a> {
    pub(crate) fn get(self, name: &[u8]) -> Option<&'a BString> {
        self.path
            .and_then(|props| props.get(name))
            .or_else(|| self.relative.and_then(|props| props.get(name)))
    }
}

pub(crate) fn properties<'a>(dir: &'a Directory, source: &BString) -> SourceProperties<'a> {
    let relative = || {
        let binary = Lexical::new(&dir.binary_dir);
        let source = Lexical::new(source);
        if !binary.prefix(source) {
            return None;
        }
        // A property may use the relative name of a source in the build tree.
        let relative = source.relative(binary)?;
        dir.source_props
            .get(&absolute_path(&dir.source_dir, &relative))
    };
    SourceProperties {
        source_dir: &dir.source_dir,
        path: dir.source_props.get(source),
        relative: relative(),
    }
}

fn source_flag(dir: &Directory, source: &[u8], property: &[u8]) -> bool {
    dir.source_props
        .get(source)
        .and_then(|props| props.get(property))
        .is_some_and(|value| truth::is_on(value))
}

fn generated_at_binary_alternative(dir: &Directory, source: &[u8], binary: &BString) -> bool {
    dir.source_props.get(source).is_some_and(|props| {
        props.get(b"\0rcmake-source-binary-alternative") == Some(binary)
            && props
                .get(b"GENERATED")
                .is_some_and(|value| truth::is_on(value))
    })
}

pub(crate) fn absolute_path(base: &[u8], path: &[u8]) -> BString {
    Lexical::new(&Lexical::new(base).append(path)).normalize()
}

pub(crate) fn custom_object_name(
    model: &ProjectModel,
    dir: DirId,
    source: &[u8],
) -> Result<BString, crate::ResolveError> {
    object_property(model, dir, source, b"OBJECT_NAME", None)
}

pub(crate) fn install_object_name(
    model: &ProjectModel,
    dir: DirId,
    source: &[u8],
    config: &ConfigId,
) -> Result<BString, ResolveError> {
    object_property(model, dir, source, b"INSTALL_OBJECT_NAME", Some(config))
}

fn object_property(
    model: &ProjectModel,
    dir: DirId,
    source: &[u8],
    property: &[u8],
    config: Option<&ConfigId>,
) -> Result<BString, ResolveError> {
    let properties = properties(model.dir(dir), &source.into());
    let Some(raw) = properties.get(property) else {
        return Ok(BString::default());
    };
    let mut usage = crate::usage::UsageResolver::new(model);
    let ctx = EvalContext {
        config: config.cloned().unwrap_or_default(),
        site: EvalSite {
            property: Some(property.into()),
            ..EvalSite::default()
        },
        ..EvalContext::default()
    };
    let ast = parse(raw).map_err(rcmake_genex::GenexError::without_location)?;
    let (name, flags) = evaluate(&ast, &ctx, &mut usage.host(dir))
        .map_err(rcmake_genex::GenexError::without_location)?;
    let path = Lexical::new(&name);
    let reason = if flags.head_sensitive
        || flags.link_language_sensitive
        || (config.is_none() && flags.context_sensitive)
    {
        format!(
            "contains a context-sensitive condition which is not supported\n  ({}).",
            raw
        )
    } else if path.absolute() || !path.root_name().is_empty() {
        format!("resolves to an absolute path which is not supported:\n\n  {name}")
    } else if path.components().any(|part| part == b"..") {
        format!("contains an invalid component ({name}).")
    } else {
        return Ok(name);
    };
    Err(crate::ResolveError::Message(
        format!(
            "The \"{}\" property for\n\n  {}\n\n{reason}",
            BString::from(property),
            BString::from(source)
        )
        .into(),
    ))
}

pub(crate) fn generation_source_location(location: Location) -> Location {
    match location {
        Location::Backtrace(trace) => {
            let frames: Vec<_> = trace
                .frames()
                .enumerate()
                .filter(|(index, frame)| *index == 0 || frame.kind != FrameKind::ModuleInclude)
                .map(|(_, frame)| frame)
                .collect();
            Location::Backtrace(
                frames
                    .into_iter()
                    .rev()
                    .fold(Backtrace::default(), |trace, frame| {
                        trace.push(frame.clone())
                    }),
            )
        }
        other => other,
    }
}
