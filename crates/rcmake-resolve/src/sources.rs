//! Source paths and their build-time producers.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, FrameKind, Location, MessageKind, PolicyId, PolicyStatus,
    RenderOptions, render,
};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{FileKind, Follow, Host, Stream, path::to_native};
use rcmake_model::{BT, Directory, ProjectModel, PropertyMap, TargetId};

use crate::{
    CompileGroup, InterfaceInclude, ResolveError,
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
            Self::Definitions => group
                .defines
                .extend(source_values.into_iter().chain(set_values)),
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

pub(crate) fn has_source_compile_properties(props: SourceProperties<'_>) -> bool {
    props.get(b"COMPILE_FLAGS").is_some()
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
        let source_values = match source_props.get(name) {
            Some(value) => usage.file_set_paths(
                target,
                name,
                &[BT::new(value.clone(), source.backtrace.clone())],
                &ctx,
            )?,
            None => Vec::new(),
        };
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
    pub(crate) sources: IndexSet<BString>,
    pub(crate) objects: IndexSet<BString>,
    pub(crate) position_independent: Option<bool>,
    pub(crate) standard_requirements: IndexMap<BString, &'static [u8]>,
    pub(crate) unity_groups: IndexMap<BString, Option<BString>>,
}

pub(crate) struct SourcePaths<'a> {
    pub(crate) host: &'a dyn Host,
    pub(crate) ide: Option<crate::ide::IdeFlavor>,
    generated: IndexMap<BString, IndexSet<BString>>,
    objects: IndexMap<BString, TargetId>,
    pub(crate) unity: Option<crate::unity::UnityPlan>,
}

impl<'a> SourcePaths<'a> {
    pub(crate) fn new(host: &'a dyn Host) -> Self {
        Self {
            host,
            ide: None,
            generated: IndexMap::new(),
            objects: IndexMap::new(),
            unity: None,
        }
    }

    pub(crate) fn index_generated(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
    ) -> Result<(), ResolveError> {
        self.generated.clear();
        let mut usage = UsageResolver::new(model).with_ide(self.ide);
        for (_, command) in model.commands() {
            let (outputs, byproducts) =
                crate::lower::command_outputs(model, command, config, &mut usage)?;
            let dir = model.dir(command.dir);
            for path in outputs.into_iter().chain(byproducts) {
                self.generated
                    .entry(path)
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
            .filter(|(_, target)| target.kind.compiles_objects() && !target.is_imported())
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
                            .iter()
                            .any(|&policy| policies.get(policy) == PolicyStatus::New)
                    })
            })
    }

    pub(crate) fn is_source_dependency(
        &self,
        dir: &Directory,
        source: &BString,
        binary: &BString,
    ) -> bool {
        self.host.metadata(&to_native(source), Follow::Yes).is_ok()
            || (!self.generated.contains_key(binary) && dir.source_props.contains_key(source))
    }

    pub(crate) fn find(&self, dir: &Directory, source: &[u8]) -> Option<BString> {
        let paths = [
            absolute_path(&dir.source_dir, source),
            absolute_path(&dir.binary_dir, source),
        ];
        if let Some(path) = paths
            .iter()
            .find(|path| self.generated_in(dir, path) || self.objects.contains_key(*path))
        {
            return Some(path.clone());
        }
        paths.into_iter().find(|path| {
            self.host
                .metadata(&to_native(path), Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
        })
    }

    pub(crate) fn resolve(&self, dir: &Directory, source: &[u8]) -> Result<BString, ResolveError> {
        self.find(dir, source).ok_or_else(|| {
            ResolveError::Message([b"Cannot find source file:\n\n  ", source].concat().into())
        })
    }

    pub(crate) fn resolve_at(
        &self,
        model: &ProjectModel,
        dir: &Directory,
        source: &BT<BString>,
    ) -> Result<BString, ResolveError> {
        self.resolve(dir, &source.value).inspect_err(|error| {
            if !source.backtrace.is_empty() {
                let frames: Vec<_> = source
                    .backtrace
                    .frames()
                    .enumerate()
                    .filter(|(index, frame)| *index == 0 || frame.kind != FrameKind::ModuleInclude)
                    .map(|(_, frame)| frame)
                    .collect();
                let backtrace = frames
                    .into_iter()
                    .rev()
                    .fold(Backtrace::default(), |stack, frame| {
                        stack.push(frame.clone())
                    });
                self.emit(
                    model,
                    MessageKind::Error,
                    Location::Backtrace(backtrace),
                    error.to_string().into(),
                );
            }
        })
    }

    pub(crate) fn emit(
        &self,
        model: &ProjectModel,
        kind: MessageKind,
        location: Location,
        body: BString,
    ) {
        let options = RenderOptions {
            relative_base: Some(model.dir(rcmake_model::DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        };
        let diagnostic = Diagnostic {
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
}

pub(crate) fn external_object(dir: &Directory, path: &BString) -> bool {
    source_flag(dir, path, b"EXTERNAL_OBJECT")
        || matches!(Lexical::new(path).stem_extension(true).1, b".o" | b".obj")
}

#[derive(Clone, Copy)]
pub(crate) struct SourceProperties<'a> {
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

pub(crate) fn absolute_path(base: &[u8], path: &[u8]) -> BString {
    Lexical::new(&Lexical::new(base).append(path)).normalize()
}
