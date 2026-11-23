//! Plan unity source files across build configurations.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, hash::HashAlgo, path_lex::Lexical, truth};
use rcmake_diag::{Backtrace, MessageKind};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, ReadRange, WriteMode, path::to_native};
use rcmake_model::{BT, DirId, ProjectModel, Target, TargetId, TargetKind};

use crate::{
    CompileGroup, FileSetMember, InterfaceInclude, ResolveError, SourcePaths, absolute_path,
    generation_location, lower, resolve_target, sources::SourceProperties, usage::UsageResolver,
};

pub(crate) type UnityPlan = IndexMap<TargetId, TargetUnity>;

#[derive(Default)]
pub(crate) struct TargetUnity {
    pub(crate) files: IndexMap<BString, UnityFile>,
    pub(crate) batched: IndexSet<BString>,
}

pub(crate) struct UnityFile {
    pub(crate) language: BString,
    pub(crate) relocatable: bool,
    pub(crate) config_guard: bool,
    object_name: BString,
}

impl UnityFile {
    pub(crate) fn object_path(
        &self,
        model: &ProjectModel,
        target: &Target,
        config: &ConfigId,
        source: &[u8],
    ) -> BString {
        if lower::short_object_paths(model) {
            return lower::object_path(model, target, config, source);
        }
        Lexical::new(&lower::object_dir(model, target, config)).append(&self.object_name)
    }

    pub(crate) fn apply_compile_properties(
        &self,
        model: &ProjectModel,
        config: &ConfigId,
        group: &mut CompileGroup,
    ) {
        if self.relocatable {
            let source_root = &model.dir(DirId::ROOT).source_dir;
            if !group
                .includes
                .iter()
                .any(|entry| entry.path.value == *source_root)
            {
                group.includes.insert(
                    0,
                    InterfaceInclude {
                        path: BT::new(source_root.clone(), Backtrace::default()),
                        system: false,
                    },
                );
            }
        }
        if self.config_guard {
            group
                .defines
                .push(BT::new(config_macro(config), Backtrace::default()));
        }
    }
}

pub(crate) fn enabled(target: &Target) -> bool {
    matches!(
        target.kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
            | TargetKind::ObjectLibrary
    ) && !target.is_imported()
        && target
            .props
            .get(b"UNITY_BUILD")
            .is_some_and(|value| truth::is_on(value))
}

enum Mode {
    Batch(usize),
    Group,
}

struct Options<'a> {
    mode: Mode,
    prefix: &'a [u8],
    relocatable: bool,
    unique_id: Option<&'a BString>,
    before: Option<&'a BString>,
    after: Option<&'a BString>,
}

impl<'a> Options<'a> {
    fn read(
        model: &'a ProjectModel,
        paths: &SourcePaths<'_>,
        target: &'a Target,
    ) -> Result<Self, ResolveError> {
        let mode = match target
            .props
            .get(b"UNITY_BUILD_MODE")
            .or_else(|| lower::definition(model, target.dir, b"CMAKE_UNITY_BUILD_MODE"))
            .map(|value| value.as_slice())
        {
            None | Some(b"BATCH") => Mode::Batch(
                target
                    .props
                    .get(b"UNITY_BUILD_BATCH_SIZE")
                    .map_or(8, |size| {
                        let size = size.trim_ascii_start();
                        let size = size.strip_prefix(b"+").unwrap_or(size);
                        size.split(|byte| !byte.is_ascii_digit())
                            .next()
                            .and_then(|digits| {
                                std::str::from_utf8(digits).ok()?.parse::<usize>().ok()
                            })
                            .filter(|size| *size != 0)
                            .unwrap_or(usize::MAX)
                    }),
            ),
            Some(b"GROUP") => Mode::Group,
            Some(mode) => {
                let body: BString = format!(
                    "Invalid UNITY_BUILD_MODE value of {} assigned to target {}.\n  Acceptable values are BATCH and GROUP.",
                    BString::from(mode), target.name
                ).into();
                paths.emit(
                    model,
                    MessageKind::Error,
                    generation_location(model, target),
                    body.clone(),
                );
                return Err(ResolveError::Message(body));
            }
        };
        let code = |key: &[u8]| target.props.get(key).filter(|value| !value.is_empty());
        Ok(Self {
            mode,
            prefix: target
                .props
                .get(b"UNITY_BUILD_FILENAME_PREFIX")
                .map_or(b"unity_", |value| value.as_slice()),
            relocatable: target
                .props
                .get(b"UNITY_BUILD_RELOCATABLE")
                .or_else(|| lower::definition(model, target.dir, b"CMAKE_UNITY_BUILD_RELOCATABLE"))
                .is_some_and(|value| truth::is_on(value)),
            unique_id: code(b"UNITY_BUILD_UNIQUE_ID"),
            before: code(b"UNITY_BUILD_CODE_BEFORE_INCLUDE"),
            after: code(b"UNITY_BUILD_CODE_AFTER_INCLUDE"),
        })
    }
}

pub(crate) fn source_group(
    id: TargetId,
    props: SourceProperties<'_>,
    member: Option<&FileSetMember<'_>>,
) -> Option<Option<BString>> {
    const SEPARATE_PROPERTIES: &[&[u8]] = &[
        b"COMPILE_OPTIONS",
        b"COMPILE_DEFINITIONS",
        b"COMPILE_FLAGS",
        b"INCLUDE_DIRECTORIES",
    ];
    let skip = |value: Option<&BString>| value.is_some_and(|value| truth::is_on(value));
    if skip(props.get(b"SKIP_UNITY_BUILD_INCLUSION"))
        || SEPARATE_PROPERTIES
            .iter()
            .any(|key| props.get(key).is_some())
        || member.is_some_and(|member| {
            let set = member.set;
            set.kind == rcmake_model::FileSetKind::CxxModules
                || skip(set.props.get(b"SKIP_UNITY_BUILD_INCLUSION"))
                || SEPARATE_PROPERTIES.iter().any(|key| {
                    let key = if set.target == id {
                        BString::from(*key)
                    } else {
                        [b"INTERFACE_", *key].concat().into()
                    };
                    set.props.get(&key).is_some()
                })
        })
    {
        return None;
    }
    Some(props.get(b"UNITY_GROUP").cloned())
}

const LANGUAGES: &[(&[u8], &[u8])] = &[
    (b"CUDA", b"cu"),
    (b"OBJCXX", b"mm"),
    (b"OBJC", b"m"),
    (b"CXX", b"cxx"),
    (b"C", b"c"),
];

#[derive(Default)]
struct Candidate {
    group: Option<BString>,
    configs: IndexSet<ConfigId>,
}

type Candidates = IndexMap<BString, IndexMap<BString, Candidate>>;

pub(crate) fn prepare(
    model: &ProjectModel,
    host: &dyn Host,
    paths: &mut SourcePaths<'_>,
    configs: &[ConfigId],
) -> Result<UnityPlan, ResolveError> {
    let mut candidates = model
        .targets()
        .filter(|(_, target)| enabled(target))
        .map(|(id, target)| {
            Ok((
                id,
                (
                    target,
                    Options::read(model, paths, target)?,
                    Candidates::new(),
                ),
            ))
        })
        .collect::<Result<IndexMap<_, _>, ResolveError>>()?;
    if candidates.is_empty() {
        return Ok(UnityPlan::new());
    }
    for config in configs {
        paths.index_generated(model, config)?;
        paths.index_objects(model, config);
        let mut usage = UsageResolver::with_source_paths(model, paths);
        let ctx = EvalContext {
            config: config.clone(),
            ..EvalContext::default()
        };
        for (&id, (target, options, candidates)) in &mut candidates {
            let (facts, inputs) = resolve_target(model, paths, &mut usage, id, target, &ctx)?;
            for unit in facts.compile_units.iter().filter(|unit| !unit.scan) {
                let Some(group) = inputs.unity_groups.get(&unit.source.value) else {
                    continue;
                };
                if matches!(options.mode, Mode::Group) && group.is_none() {
                    continue;
                }
                let Some(compile_group) = facts.compile_groups.get(unit.group) else {
                    continue;
                };
                let language = &compile_group.language;
                let candidate = candidates
                    .entry(language.clone())
                    .or_default()
                    .entry(unit.source.value.clone())
                    .or_default();
                candidate.group.clone_from(group);
                candidate.configs.insert(config.clone());
            }
        }
    }
    candidates
        .into_iter()
        .map(|(id, (target, options, candidates))| {
            let generator = UnityWriter {
                model,
                host,
                target,
                options,
                config_count: configs.len(),
            };
            Ok((id, generator.write_files(candidates)?))
        })
        .collect()
}

struct UnityWriter<'a> {
    model: &'a ProjectModel,
    host: &'a dyn Host,
    target: &'a Target,
    options: Options<'a>,
    config_count: usize,
}

impl UnityWriter<'_> {
    fn write_files(&self, candidates: Candidates) -> Result<TargetUnity, ResolveError> {
        let mut plan = TargetUnity::default();
        let root = self.model.dir(DirId::ROOT);
        let target_dir = lower::object_dir(self.model, self.target, &ConfigId::default());
        let unity_dir = absolute_path(
            &root.binary_dir,
            &Lexical::new(&target_dir).append(b"Unity"),
        );
        for &(language, extension) in LANGUAGES {
            let Some(sources) = candidates.get(language) else {
                continue;
            };
            let groups = match self.options.mode {
                Mode::Batch(size) => {
                    let sources: Vec<_> = sources.iter().collect();
                    sources
                        .chunks(size)
                        .enumerate()
                        .rev()
                        .map(|(index, chunk)| (BString::from(index.to_string()), chunk.to_vec()))
                        .collect::<IndexMap<_, _>>()
                }
                Mode::Group => {
                    let mut groups = IndexMap::<_, Vec<_>>::new();
                    for (path, candidate) in sources {
                        if let Some(group) = &candidate.group {
                            groups
                                .entry(group.clone())
                                .or_default()
                                .push((path, candidate));
                        }
                    }
                    groups
                }
            };
            for (group, members) in groups {
                let name = [
                    self.options.prefix,
                    group.as_slice(),
                    b"_",
                    extension,
                    b".",
                    extension,
                ]
                .concat();
                let path = Lexical::new(&unity_dir).append(&name);
                let mut content = BString::from("/* generated by CMake */\n\n");
                let mut config_guard = false;
                for (source, candidate) in members {
                    config_guard |= self.append_member(&mut content, &path, source, candidate);
                    plan.batched.insert(source.clone());
                }
                write_if_changed(self.host, &path, &content)?;
                let suffix_key = [b"CMAKE_", language, b"_OUTPUT_EXTENSION"].concat();
                let suffix = lower::definition(self.model, self.target.dir, &suffix_key)
                    .map_or(b".o".as_slice(), |value| value.as_slice());
                plan.files.insert(
                    path,
                    UnityFile {
                        language: language.into(),
                        relocatable: self.options.relocatable,
                        config_guard,
                        object_name: [b"Unity/", name.as_slice(), suffix].concat().into(),
                    },
                );
            }
        }
        Ok(plan)
    }

    fn append_member(
        &self,
        content: &mut BString,
        unity: &[u8],
        source: &BString,
        candidate: &Candidate,
    ) -> bool {
        let guard = candidate.configs.len() < self.config_count;
        if guard {
            let mut sep = &b"#if "[..];
            for config in &candidate.configs {
                content.extend_from_slice(sep);
                content.extend_from_slice(b"defined(");
                content.extend_from_slice(&config_macro(config));
                content.push(b')');
                sep = b" || ";
            }
            content.push(b'\n');
        }
        let root = self.model.dir(DirId::ROOT);
        let source_path = Lexical::new(source);
        let binary_root = Lexical::new(&root.binary_dir);
        let source_root = Lexical::new(&root.source_dir);
        let (tag, relative, include) = if binary_root.prefix(source_path) {
            let relative = source_path
                .relative(binary_root)
                .unwrap_or_else(|| source.clone());
            let include = source_path
                .relative(Lexical::new(Lexical::new(unity).parent()))
                .unwrap_or_else(|| source.clone());
            (&b"BLD_"[..], relative, include)
        } else if source_root.prefix(source_path) {
            let relative = source_path
                .relative(source_root)
                .unwrap_or_else(|| source.clone());
            (&b"SRC_"[..], relative.clone(), relative)
        } else {
            (&b"ABS_"[..], source.clone(), source.clone())
        };
        if let Some(unique_id) = self.options.unique_id {
            let name = [tag, relative.as_slice()].concat();
            let digest = HashAlgo::Md5.hex_digest(&name);
            for part in [
                b"/* ".as_slice(),
                name.as_slice(),
                b" */\n#undef ",
                unique_id,
                b"\n#define ",
                unique_id,
                b" unity_",
                digest.as_bytes(),
                b"\n",
            ] {
                content.extend_from_slice(part);
            }
        }
        if let Some(before) = self.options.before {
            content.extend_from_slice(before);
            content.push(b'\n');
        }
        content.extend_from_slice(
            b"/* NOLINTNEXTLINE(bugprone-suspicious-include,misc-include-cleaner) */\n#include \"",
        );
        content.extend_from_slice(if self.options.relocatable {
            &include
        } else {
            source
        });
        content.extend_from_slice(b"\"\n");
        if let Some(after) = self.options.after {
            content.extend_from_slice(after);
            content.push(b'\n');
        }
        if guard {
            content.extend_from_slice(b"#endif\n");
        }
        content.push(b'\n');
        guard
    }
}

fn config_macro(config: &ConfigId) -> BString {
    [
        b"CMAKE_UNITY_CONFIG_".as_slice(),
        &config.0.to_ascii_uppercase(),
    ]
    .concat()
    .into()
}

fn write_if_changed(host: &dyn Host, path: &BString, content: &[u8]) -> Result<(), ResolveError> {
    let file = to_native(path);
    let limit = u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if host
        .read(&file, ReadRange::All { limit })
        .is_ok_and(|old| old == content)
    {
        return Ok(());
    }
    let write = || {
        if let Some(parent) = file.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(&file, content, WriteMode::Truncate)
    };
    write().map_err(|error| {
        ResolveError::Message(format!("Cannot write unity source {path}. {error}").into())
    })
}
