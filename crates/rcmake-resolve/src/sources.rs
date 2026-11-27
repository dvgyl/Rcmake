//! Source paths and their build-time producers.

use std::cell::RefCell;
use std::ops::Deref;
use std::sync::{Mutex, OnceLock};

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
pub(crate) use generated::traced_sources;

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
                let mut includes = Vec::new();
                for path in set_values.into_iter().chain(source_values) {
                    if seen.insert(path.value.clone()) {
                        includes.push(InterfaceInclude {
                            path,
                            system: false,
                        });
                    }
                }
                group.source_include_count = includes.len();
                includes.extend(
                    std::mem::take(&mut group.includes)
                        .into_iter()
                        .filter(|entry| seen.insert(entry.path.value.clone())),
                );
                group.includes = includes;
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
    if props.is_empty() {
        return false;
    }
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
    let flag_start = group.flags.len();
    if let Some(value) = source_props.get(b"COMPILE_FLAGS") {
        let entry = BT::new(value.clone(), Backtrace::default());
        let flags = usage.eval_source_compile_flags(target, &entry, &ctx)?;
        append_compile_flags(&mut group.flags, &flags, Backtrace::default());
    }
    for field in SourceUsage::ALL {
        let name = field.property();
        if matches!(field, SourceUsage::Options) {
            for entry in source_props.tracked_entries(name) {
                let items =
                    usage.file_set_paths(target, name, std::slice::from_ref(&entry), &ctx)?;
                let start = group.flags.len();
                if items.is_empty() {
                    // CMake records an empty `compileCommandFragments` entry with the
                    // `set_source_files_properties` backtrace even when the value or
                    // generator expression evaluates to empty, splitting the compile group.
                    group
                        .flags
                        .push(BT::new(BString::from("SHELL:"), entry.backtrace));
                } else {
                    group.flags.extend(items);
                }
                group
                    .source_option_groups
                    .push((start, group.flags.len() - start));
            }
            if let Some(member) = member {
                let owner = member.set.target;
                let key = if owner == target {
                    name.into()
                } else {
                    [b"INTERFACE_", name].concat()
                };
                for entry in property_entries(&member.set.props, &key) {
                    let items =
                        usage.file_set_paths(owner, &key, std::slice::from_ref(&entry), &ctx)?;
                    if !items.is_empty() {
                        let start = group.flags.len();
                        group.flags.extend(items);
                        group
                            .source_option_groups
                            .push((start, group.flags.len() - start));
                    }
                }
            }
            continue;
        }
        let source_entries = source_props.tracked_entries(name);
        let mut source_values = if source_entries.is_empty() {
            Vec::new()
        } else {
            usage.file_set_paths(target, name, &source_entries, &ctx)?
        };
        let xcode_per_config_include = matches!(field, SourceUsage::Includes)
            && usage.ide() == Some(crate::ide::IdeFlavor::Xcode)
            && source_entries.iter().any(|entry| {
                let v = entry.value.as_slice();
                v.windows(9).any(|w| w == b"$<CONFIG>" || w == b"$<CONFIG:")
                    || v.windows(16).any(|w| w == b"$<CONFIGURATION>")
            });
        if matches!(field, SourceUsage::Includes)
            && !xcode_per_config_include
            && let Some(include) = source_values
                .iter()
                .find(|entry| !crate::file_is_full_path(&entry.value))
        {
            let file = BString::from(Lexical::new(&source.value).filename());
            let body = BString::from(format!(
                "Found relative path while evaluating include directories of \"{file}\":\n  \"{}\"",
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
                .map(|value| BT::new(value.clone(), Backtrace::default()))
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
            let entries = property_entries(&member.set.props, &key);
            if entries.is_empty() {
                Vec::new()
            } else {
                usage.file_set_paths(owner, &key, &entries, &ctx)?
            }
        } else {
            Vec::new()
        };
        field.apply(group, source_values, set_values);
    }
    if group.flags.len() > flag_start {
        group.source_flag_start = Some(flag_start);
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
    let defaults = [
        "C", "CXX", "OBJC", "OBJCXX", "ASM", "Fortran", "CUDA", "HIP", "Swift", "RC",
    ];
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
                "HIP" => extension == b"hip",
                "Swift" => extension == b"swift",
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
    pub(crate) file_set_order: Option<crate::FileSetOrder>,
}

pub(crate) enum DiagEvent {
    WriteOut(Stream, Vec<u8>),
    ExtensionWarning {
        key: (BString, BString),
        bytes: BString,
    },
    Missing {
        key: (BString, BString),
        bytes: BString,
    },
    WarningOnce(BString),
    FileSetWarning {
        key: (DirId, BString, BString, usize),
        bytes: BString,
    },
}

#[derive(Default)]
pub(crate) struct DiagBuffer(Mutex<Vec<DiagEvent>>);

impl DiagBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn push(&self, event: DiagEvent) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }

    pub(crate) fn take_events(&self) -> Vec<DiagEvent> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Host adapter that forwards all read-only operations and buffers `write_out`
/// calls into a per-target [`DiagBuffer`] so parallel workers never interleave stderr.
pub(crate) struct BufferedHost<'a> {
    inner: &'a dyn Host,
    events: &'a DiagBuffer,
}

impl<'a> BufferedHost<'a> {
    pub(crate) fn new(inner: &'a dyn Host, events: &'a DiagBuffer) -> Self {
        Self { inner, events }
    }
}

impl Host for BufferedHost<'_> {
    rcmake_host::forward_host!(inner, fs, env, process);

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> rcmake_host::HostResult<Option<BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.events
            .push(DiagEvent::WriteOut(stream, bytes.to_vec()));
    }

    fn read_stdin(&self, limit: u64) -> rcmake_host::HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn terminal_width(&self, stream: Stream) -> Option<usize> {
        self.inner.terminal_width(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> core::time::Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: core::time::Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> rcmake_host::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> rcmake_host::HostResult<std::path::PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> std::path::PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.inner.system()
    }

    fn effective_uid(&self) -> Option<u32> {
        self.inner.effective_uid()
    }
}

const KNOWN_EXTENSIONS: &[&[u8]] = &[
    b"c", b"C", b"c++", b"cc", b"cpp", b"cxx", b"cu", b"mpp", b"m", b"M", b"mm", b"ixx", b"cppm",
    b"ccm", b"cxxm", b"c++m", b"h", b"hh", b"h++", b"hm", b"hpp", b"hxx", b"in", b"txx", b"f",
    b"F", b"for", b"f77", b"f90", b"f95", b"f03", b"hip", b"ispc",
];

fn ambiguous_source_names(dir: &Directory) -> bool {
    dir.final_policies.is_none_or(|policies| {
        policies.get(PolicyId::new(115)) != PolicyStatus::New
            || policies.get(PolicyId::new(187)) != PolicyStatus::New
    })
}

fn known_extension_offset(name: &[u8]) -> Option<usize> {
    let dot = name.iter().rposition(|&byte| byte == b'.')?;
    KNOWN_EXTENSIONS
        .contains(&name.get(dot + 1..)?)
        .then_some(dot)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SourceRecord {
    Absolute(BString),
    Relative(BString),
}

impl SourceRecord {
    fn path(&self) -> &[u8] {
        match self {
            Self::Absolute(path) | Self::Relative(path) => path,
        }
    }
}

enum AmbiguousMatch {
    None,
    Found(BString),
    Missing { identity: BString, display: BString },
}

fn index_source_record(
    records: &mut IndexMap<BString, IndexSet<SourceRecord>>,
    dir: &Directory,
    record: SourceRecord,
) {
    let path = record.path();
    let name = Lexical::new(path).filename();
    let stem = match known_extension_offset(name) {
        Some(dot) => name.get(..dot).unwrap_or(name),
        None => {
            let is_generated = match &record {
                SourceRecord::Absolute(abs) => dir.generated_sources.contains(abs),
                SourceRecord::Relative(rel) => {
                    dir.generated_sources
                        .contains(&absolute_path(&dir.binary_dir, rel))
                        || dir
                            .generated_sources
                            .contains(&absolute_path(&dir.source_dir, rel))
                }
            };
            if !is_generated {
                return;
            }
            name
        }
    };
    records.entry(stem.into()).or_default().insert(record);
}

fn matches_source_name(path: &[u8], base: &[u8]) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .and_then(|suffix| suffix.strip_prefix(b"."))
            .is_some_and(|ext| KNOWN_EXTENSIONS.contains(&ext))
}

/// Read-only pre-indexed source and target facts shared across parallel workers.
pub(crate) struct SourceIndex<'a> {
    pub(crate) xcframeworks: crate::xcframework::Plists,
    pub(crate) ide: Option<crate::ide::IdeFlavor>,
    pub(crate) collect_source_errors: bool,
    pub(crate) ninja_cxx_dyndep: OnceLock<Option<bool>>,
    pub(crate) generator_facts: Option<&'a dyn crate::GeneratorFacts>,
    root_source_dir: Option<BString>,
    source_records: IndexMap<BString, IndexMap<BString, IndexSet<SourceRecord>>>,
    absolute_sources: IndexMap<BString, IndexSet<BString>>,
    pub(super) generated: IndexMap<BString, IndexSet<BString>>,
    pub(crate) output_source_reads: IndexMap<BString, IndexSet<TargetId>>,
    objects: IndexMap<BString, TargetId>,
    header_only: IndexMap<DirId, IndexSet<BString>>,
    pub(crate) unity: Option<crate::unity::UnityPlan>,
}

enum SourceIndexStorage<'a> {
    Owned(Box<SourceIndex<'a>>),
    Shared(&'a SourceIndex<'a>),
}

pub(crate) struct SourcePaths<'a> {
    pub(crate) host: &'a dyn Host,
    index: SourceIndexStorage<'a>,
    extension_warnings: RefCell<IndexSet<(BString, BString)>>,
    missing: RefCell<IndexSet<(BString, BString)>>,
    /// Rendered generation warnings, so that a repeated evaluation warns once.
    warnings: RefCell<IndexSet<BString>>,
    file_set_warnings: RefCell<IndexSet<(DirId, BString, BString, usize)>>,
    buffered_events: Option<&'a DiagBuffer>,
}

impl<'a> Deref for SourcePaths<'a> {
    type Target = SourceIndex<'a>;

    fn deref(&self) -> &Self::Target {
        match &self.index {
            SourceIndexStorage::Owned(index) => index,
            SourceIndexStorage::Shared(index) => index,
        }
    }
}

impl<'a> SourcePaths<'a> {
    pub(crate) fn new(host: &'a dyn Host) -> Self {
        Self {
            host,
            index: SourceIndexStorage::Owned(Box::new(SourceIndex {
                xcframeworks: crate::xcframework::Plists::default(),
                ide: None,
                collect_source_errors: true,
                ninja_cxx_dyndep: OnceLock::new(),
                generator_facts: None,
                root_source_dir: None,
                source_records: IndexMap::new(),
                absolute_sources: IndexMap::new(),
                generated: IndexMap::new(),
                output_source_reads: IndexMap::new(),
                objects: IndexMap::new(),
                header_only: IndexMap::new(),
                unity: None,
            })),
            extension_warnings: RefCell::default(),
            missing: RefCell::default(),
            warnings: RefCell::default(),
            file_set_warnings: RefCell::default(),
            buffered_events: None,
        }
    }

    pub(crate) fn shared_index(&self) -> &SourceIndex<'a> {
        self
    }

    pub(crate) fn for_worker<'w>(
        host: &'w dyn Host,
        index: &'w SourceIndex<'w>,
        events: &'w DiagBuffer,
    ) -> SourcePaths<'w> {
        SourcePaths {
            host,
            index: SourceIndexStorage::Shared(index),
            extension_warnings: RefCell::default(),
            missing: RefCell::default(),
            warnings: RefCell::default(),
            file_set_warnings: RefCell::default(),
            buffered_events: Some(events),
        }
    }

    pub(crate) fn reset_local_dedup(&self) {
        self.extension_warnings.borrow_mut().clear();
        self.missing.borrow_mut().clear();
        self.warnings.borrow_mut().clear();
        self.file_set_warnings.borrow_mut().clear();
    }

    pub(crate) fn replay_events(&self, events: Vec<DiagEvent>) {
        for event in events {
            match event {
                DiagEvent::WriteOut(stream, bytes) => {
                    self.host.write_out(stream, &bytes);
                }
                DiagEvent::ExtensionWarning { key, bytes } => {
                    if self.extension_warnings.borrow_mut().insert(key) {
                        self.host.write_out(Stream::Stderr, &bytes);
                    }
                }
                DiagEvent::Missing { key, bytes } => {
                    if self.missing.borrow_mut().insert(key) {
                        self.host.write_out(Stream::Stderr, &bytes);
                    }
                }
                DiagEvent::WarningOnce(text) => {
                    if self.warnings.borrow_mut().insert(text.clone()) {
                        self.host.write_out(Stream::Stderr, &text);
                    }
                }
                DiagEvent::FileSetWarning { key, bytes } => {
                    if self.file_set_warnings.borrow_mut().insert(key) {
                        self.host.write_out(Stream::Stderr, &bytes);
                    }
                }
            }
        }
    }

    pub(super) fn index_mut(&mut self) -> Option<&mut SourceIndex<'a>> {
        match &mut self.index {
            SourceIndexStorage::Owned(index) => Some(index),
            SourceIndexStorage::Shared(_) => None,
        }
    }

    pub(crate) fn set_ide(&mut self, ide: Option<crate::ide::IdeFlavor>) {
        if let Some(index) = self.index_mut() {
            index.ide = ide;
        }
    }

    pub(crate) fn set_generator_facts(&mut self, facts: Option<&'a dyn crate::GeneratorFacts>) {
        if let Some(index) = self.index_mut() {
            index.generator_facts = facts;
        }
    }

    pub(crate) fn set_collect_source_errors(&mut self, collect: bool) {
        if let Some(index) = self.index_mut() {
            index.collect_source_errors = collect;
        }
    }

    pub(crate) fn set_unity(&mut self, unity: Option<crate::unity::UnityPlan>) {
        if let Some(index) = self.index_mut() {
            index.unity = unity;
        }
    }

    pub(crate) fn mark_header_only(&mut self, dir: DirId, path: BString) {
        if let Some(index) = self.index_mut() {
            index.header_only.entry(dir).or_default().insert(path);
        }
    }

    pub(crate) fn is_header_only(&self, dir: DirId, path: &[u8]) -> bool {
        self.header_only
            .get(&dir)
            .is_some_and(|paths| paths.contains(path))
    }

    pub(crate) fn index_source_records(&mut self, model: &ProjectModel) {
        let Some(index) = self.index_mut() else {
            return;
        };
        index.root_source_dir = Some(model.dir(DirId::ROOT).source_dir.clone());
        for (_, dir) in model.dirs().filter(|(_, dir)| ambiguous_source_names(dir)) {
            let records = index
                .source_records
                .entry(dir.binary_dir.clone())
                .or_default();
            if let Some(order) = dir.props.get(b"\0rcmake-source-order") {
                for entry in order.split(|&byte| byte == 0) {
                    let Some((&tag, path)) = entry.split_first() else {
                        continue;
                    };
                    if path.is_empty() {
                        continue;
                    }
                    let record = match tag {
                        b'R' => SourceRecord::Relative(path.into()),
                        b'A' => SourceRecord::Absolute(path.into()),
                        _ => continue,
                    };
                    index_source_record(records, dir, record);
                }
            }
            for (path, props) in &dir.source_props {
                let record = if let Some(resolved) = props.get(b"\0rcmake-source-location") {
                    SourceRecord::Absolute(resolved.clone())
                } else if props.get(b"\0rcmake-source-binary-alternative").is_some() {
                    if let Some(relative) =
                        Lexical::new(path).relative(Lexical::new(&dir.source_dir))
                    {
                        SourceRecord::Relative(relative)
                    } else {
                        SourceRecord::Absolute(path.clone())
                    }
                } else {
                    SourceRecord::Absolute(path.clone())
                };
                index_source_record(records, dir, record);
            }
        }
        for (_, target) in model.targets() {
            let dir = model.dir(target.dir);
            let absolute = index
                .absolute_sources
                .entry(dir.binary_dir.clone())
                .or_default();
            let mut records = index.source_records.get_mut(&dir.binary_dir);
            for path in target
                .sources
                .iter()
                .flat_map(|entry| list::iter(&entry.value, list::Empty::Drop))
                .filter(|path| !path.windows(2).any(|part| part == b"$<"))
            {
                let is_absolute = Lexical::new(&path).absolute();
                if !is_absolute && records.is_none() {
                    continue;
                }
                let resolved = absolute_path(&dir.source_dir, &path);
                if let Some(records) = &mut records {
                    let record = if is_absolute {
                        SourceRecord::Absolute(resolved.clone())
                    } else {
                        SourceRecord::Relative(Lexical::new(&path).normalize())
                    };
                    index_source_record(records, dir, record);
                }
                if is_absolute {
                    absolute.insert(resolved);
                }
            }
        }
    }

    pub(crate) fn index_generated(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
    ) -> Result<(), ResolveError> {
        if let Some(index) = self.index_mut() {
            index.generated.clear();
            index.output_source_reads.clear();
        }
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
            if let Some(index) = self.index_mut() {
                for path in outputs.into_iter().chain(byproducts) {
                    index
                        .generated
                        .entry(path)
                        .or_default()
                        .insert(dir.binary_dir.clone());
                }
            }
        }
        crate::lower::custom::combine_errors(errors)?;
        if let Some(index) = self.index_mut() {
            for (_, dir) in model.dirs() {
                for path in &dir.generated_sources {
                    index
                        .generated
                        .entry(path.clone())
                        .or_default()
                        .insert(dir.binary_dir.clone());
                }
            }
        }
        self.index_file_generations(model, config, &mut usage)?;
        Ok(())
    }

    pub(crate) fn index_objects(&mut self, model: &ProjectModel, config: &ConfigId) {
        if let Some(index) = self.index_mut() {
            index.objects.clear();
        }
        let objects = {
            let usage = UsageResolver::with_source_paths(model, self);
            model
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
                .collect()
        };
        if let Some(index) = self.index_mut() {
            index.objects = objects;
        }
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
        match self.lookup_ambiguous_record(dir, source, &source_path, &binary_path) {
            AmbiguousMatch::Found(found) => return Some(found),
            AmbiguousMatch::Missing { .. } => return None,
            AmbiguousMatch::None => {}
        }
        self.find_non_ambiguous(dir, source, &source_path, &binary_path)
    }

    fn find_non_ambiguous(
        &self,
        dir: &Directory,
        source: &[u8],
        source_path: &BString,
        binary_path: &BString,
    ) -> Option<BString> {
        // In CMake's cmMakefile::SourceFileSearchIndex, an explicit full-path
        // source file recorded before a same-named generated output in the build
        // tree wins over the later generated output for relative lookups.
        if !Lexical::new(source).absolute()
            && source_path != binary_path
            && !self.generated_in(dir, source_path)
            && let Some((src_idx, _, src_props)) = dir.source_props.get_full(source_path)
            && src_props
                .get(b"\0rcmake-source-binary-alternative")
                .is_none()
            && dir
                .source_props
                .get_index_of(binary_path)
                .is_none_or(|bin_idx| src_idx < bin_idx)
            && self
                .host
                .metadata(&to_native(source_path), Follow::Yes)
                .is_ok_and(|metadata| matches!(metadata.kind, FileKind::File | FileKind::Dir))
        {
            return Some(source_path.clone());
        }
        if self.generated_in(dir, binary_path)
            || self.objects.contains_key(binary_path)
            || generated_at_binary_alternative(dir, source_path, binary_path)
        {
            return Some(binary_path.clone());
        }
        if let Some(path) = dir
            .source_props
            .get(source_path)
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
        if self.generated_in(dir, source_path) || self.objects.contains_key(source_path) {
            return Some(source_path.clone());
        }
        [source_path, binary_path]
            .into_iter()
            .find(|path| {
                self.host
                    .metadata(&to_native(path), Follow::Yes)
                    .is_ok_and(|metadata| matches!(metadata.kind, FileKind::File | FileKind::Dir))
            })
            .cloned()
    }

    /// Look up the first registered source matching an extensionless spelling.
    ///
    /// Mirrors CMake's `cmMakefile::GetSource`: searches registered `cmSourceFile`
    /// records in directory creation order and binds to the first match, even if
    /// that record does not exist on disk.
    fn lookup_ambiguous_record(
        &self,
        dir: &Directory,
        source: &[u8],
        source_path: &BString,
        binary_path: &BString,
    ) -> AmbiguousMatch {
        let file_name = Lexical::new(source).filename();
        if !ambiguous_source_names(dir) || known_extension_offset(file_name).is_some() {
            return AmbiguousMatch::None;
        }
        // A relative extensionless name is exact if it exists in the source tree.
        if self
            .host
            .metadata(&to_native(source_path), Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
        {
            return AmbiguousMatch::None;
        }
        let is_abs_lookup = Lexical::new(source).absolute();
        let matches = |candidate: &[u8]| {
            matches_source_name(candidate, source_path)
                || matches_source_name(candidate, binary_path)
        };
        let exists = |path: &[u8]| {
            self.host
                .metadata(&to_native(path), Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
        };
        let first = self
            .source_records
            .get(&dir.binary_dir)
            .and_then(|records| records.get(file_name))
            .into_iter()
            .flatten()
            .find(|record| match record {
                SourceRecord::Absolute(abs) => matches(abs),
                SourceRecord::Relative(rel) => {
                    matches(&absolute_path(&dir.source_dir, rel))
                        || matches(&absolute_path(&dir.binary_dir, rel))
                }
            });
        if let Some(record) = first {
            return match record {
                SourceRecord::Absolute(abs) => {
                    if self.generated_in(dir, abs) || self.objects.contains_key(abs) || exists(abs)
                    {
                        AmbiguousMatch::Found(abs.clone())
                    } else {
                        AmbiguousMatch::Missing {
                            identity: abs.clone(),
                            display: abs.clone(),
                        }
                    }
                }
                SourceRecord::Relative(rel) => {
                    let rec_src = absolute_path(&dir.source_dir, rel);
                    let rec_bin = absolute_path(&dir.binary_dir, rel);
                    if is_abs_lookup {
                        let target = if matches_source_name(&rec_src, source_path) {
                            rec_src
                        } else {
                            rec_bin
                        };
                        if self.generated_in(dir, &target)
                            || self.objects.contains_key(&target)
                            || exists(&target)
                        {
                            AmbiguousMatch::Found(target)
                        } else {
                            AmbiguousMatch::Missing {
                                identity: target.clone(),
                                display: target,
                            }
                        }
                    } else if self.generated_in(dir, &rec_bin)
                        || self.objects.contains_key(&rec_bin)
                        || generated_at_binary_alternative(dir, &rec_src, &rec_bin)
                    {
                        AmbiguousMatch::Found(rec_bin)
                    } else if self.generated_in(dir, &rec_src)
                        || self.objects.contains_key(&rec_src)
                        || exists(&rec_src)
                    {
                        AmbiguousMatch::Found(rec_src)
                    } else if exists(&rec_bin) {
                        AmbiguousMatch::Found(rec_bin)
                    } else {
                        AmbiguousMatch::Missing {
                            identity: rec_src,
                            display: rel.clone(),
                        }
                    }
                }
            };
        }
        if let Some(found) = self
            .generated
            .keys()
            .find(|path| matches(path) && self.generated_in(dir, path))
        {
            return AmbiguousMatch::Found(found.clone());
        }
        AmbiguousMatch::None
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
        let source_path = absolute_path(&dir.source_dir, &source.value);
        let binary_path = absolute_path(&dir.binary_dir, &source.value);
        let ambiguous_missing =
            match self.lookup_ambiguous_record(dir, &source.value, &source_path, &binary_path) {
                AmbiguousMatch::Found(found) => return Ok(Some(found)),
                AmbiguousMatch::Missing { identity, display } => Some((identity, display)),
                AmbiguousMatch::None => {
                    if let Some(found) =
                        self.find_non_ambiguous(dir, &source.value, &source_path, &binary_path)
                    {
                        return Ok(Some(found));
                    }
                    None
                }
            };
        const EXTENSIONS: &str = "c C c++ cc cpp cxx cu mpp m M mm ixx cppm ccm cxxm c++m h hh h++ hm hpp hxx in txx f F for f77 f90 f95 f03 hip ispc";
        let policy = PolicyId::new(115);
        let status = dir
            .final_policies
            .map_or(PolicyStatus::New, |policies| policies.get(policy));
        let (identity, display_path) = ambiguous_missing
            .clone()
            .unwrap_or_else(|| self.missing_identity(dir, &source.value));
        let mut body: BString = [b"Cannot find source file:\n  ", display_path.as_slice()]
            .concat()
            .into();
        if status != PolicyStatus::New {
            let extensions = EXTENSIONS.split_ascii_whitespace();
            if ambiguous_missing.is_none() {
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
                            let warning_key = (dir.binary_dir.clone(), identity);
                            if status == PolicyStatus::Warn
                                && self
                                    .extension_warnings
                                    .borrow_mut()
                                    .insert(warning_key.clone())
                            {
                                let warning = Diagnostic {
                                    annotations: None,
                                    kind: MessageKind::AuthorWarning,
                                    effect: Effect::None,
                                    category: Some(DiagCategory::Policy),
                                    policy: Some(policy),
                                    location: generation_source_location(Location::Backtrace(
                                        source.backtrace.clone(),
                                    )),
                                    body: format!("Policy CMP0115 is not set: Source file extensions must be explicit.  Run \"cmake --help-policy CMP0115\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\nFile:\n  {candidate}").into(),
                                };
                                let rendered = render(
                                    &warning,
                                    &RenderOptions {
                                        relative_base: Some(
                                            model.dir(DirId::ROOT).source_dir.clone(),
                                        ),
                                        ..RenderOptions::default()
                                    },
                                );
                                if let Some(events) = self.buffered_events {
                                    events.push(DiagEvent::ExtensionWarning {
                                        key: warning_key,
                                        bytes: rendered,
                                    });
                                } else {
                                    self.host.write_out(Stream::Stderr, &rendered);
                                }
                            }
                            return Ok(Some(candidate));
                        }
                    }
                }
            }
            body.extend_from_slice(b"\nTried extensions");
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
        let missing_key = (dir.binary_dir.clone(), identity);
        if self.missing.borrow_mut().insert(missing_key.clone()) {
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
            let rendered = render_diagnostic(
                Some(model.dir(DirId::ROOT).source_dir.clone()),
                MessageKind::Error,
                Location::Backtrace(trace),
                body,
            );
            if let Some(events) = self.buffered_events {
                events.push(DiagEvent::Missing {
                    key: missing_key,
                    bytes: rendered,
                });
            } else {
                self.host.write_out(Stream::Stderr, &rendered);
            }
        }
        Ok(None)
    }

    pub(crate) fn emit_file_set_warning(
        &self,
        model: &ProjectModel,
        policy: PolicyId,
        target: &rcmake_model::Target,
        body: &BString,
        occurrence: usize,
    ) {
        let key = (target.dir, target.name.clone(), body.clone(), occurrence);
        if !self.file_set_warnings.borrow_mut().insert(key.clone()) {
            return;
        }
        let Some(rendered) =
            crate::pipeline::render_policy_warning(model, policy, target, &body.to_string())
        else {
            return;
        };
        if let Some(events) = self.buffered_events {
            events.push(DiagEvent::FileSetWarning {
                key,
                bytes: rendered,
            });
        } else {
            self.host.write_out(Stream::Stderr, &rendered);
        }
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
            self.emit_rendered_warning_once(text);
        }
    }

    pub(crate) fn emit_rendered_warning_once(&self, text: BString) {
        if let Some(events) = self.buffered_events {
            events.push(DiagEvent::WarningOnce(text));
        } else {
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
        let rendered = render_diagnostic(relative_base, kind, location, body);
        self.host.write_out(Stream::Stderr, &rendered);
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
    for (_, set, _) in usage.source_file_sets(facts.id, ctx)? {
        if set.kind == FileSetKind::Headers
            && let Some(owner) = model.target(set.target)
        {
            let owner_dir = model.dir(owner.dir);
            for file in usage.file_set_target_sources(set, ctx)? {
                if let Some(path) = paths.find(owner_dir, &file.value) {
                    invalid.shift_remove(&path);
                }
            }
        }
    }
    if invalid.is_empty() {
        return Ok(());
    }
    let mut body = BString::from(format!("OBJECT library \"{}\" contains:", target.name));
    for source in invalid {
        body.extend_from_slice(b"\n  ");
        body.extend_from_slice(Lexical::new(source).filename());
    }
    body.extend_from_slice(b"\nbut may contain only sources that compile, header files, and other files that would not affect linking of a normal library.");
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
    pub(crate) fn is_empty(self) -> bool {
        self.path.is_none() && self.relative.is_none()
    }

    pub(crate) fn get(self, name: &[u8]) -> Option<&'a BString> {
        self.path
            .and_then(|props| props.get(name))
            .or_else(|| self.relative.and_then(|props| props.get(name)))
    }

    pub(crate) fn tracked_entries(self, name: &[u8]) -> Vec<BT<BString>> {
        let props = self
            .path
            .filter(|props| props.get(name).is_some())
            .or_else(|| self.relative.filter(|props| props.get(name).is_some()));
        props.map_or_else(Vec::new, |props| property_entries(props, name))
    }
}

fn property_entries(props: &PropertyMap, name: &[u8]) -> Vec<BT<BString>> {
    if let Some(entries) = props.tracked_entries(name) {
        entries.to_vec()
    } else if let Some(value) = props.get(name) {
        vec![BT::new(value.clone(), Backtrace::default())]
    } else {
        Vec::new()
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
            "contains a context-sensitive condition which is not supported ({}).",
            raw
        )
    } else if path.absolute() || !path.root_name().is_empty() {
        format!("resolves to an absolute path which is not supported:\n  {name}")
    } else if path.components().any(|part| part == b"..") {
        format!("contains an invalid component ({name}).")
    } else {
        return Ok(name);
    };
    Err(crate::ResolveError::Message(
        format!(
            "The \"{}\" property for\n  {}\n{reason}",
            BString::from(property),
            BString::from(source)
        )
        .into(),
    ))
}

fn render_diagnostic(
    relative_base: Option<BString>,
    kind: MessageKind,
    location: Location,
    body: BString,
) -> BString {
    let options = RenderOptions {
        relative_base,
        ..RenderOptions::default()
    };
    let diagnostic = Diagnostic {
        annotations: None,
        kind,
        effect: Effect::None,
        category: None,
        policy: None,
        location: generation_source_location(location),
        body,
    };
    render(&diagnostic, &options)
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

#[cfg(test)]
#[test]
fn source_record_matches_stem_only_under_compatible_policies()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::TargetOrigin;

    let host = rcmake_host::PureHost::new(to_native(b"/src"), to_native(b"/bin/cmake"));
    for path in [
        b"/build/ordered.cxx".as_slice(),
        b"/src/ordered.cxx",
        b"/src/registered.cxx",
        b"/src/unregistered.cxx",
        b"/src/exact",
        b"/src/exact.cxx",
    ] {
        host.seed_file(&to_native(path), b"")?;
    }
    let mut model = ProjectModel::default();
    let dir = model.dir_mut(DirId::ROOT);
    dir.source_dir = "/src".into();
    dir.binary_dir = "/build".into();
    let mut policies = rcmake_diag::PolicyMap::default();
    policies.set(PolicyId::new(115), PolicyStatus::New)?;
    dir.source_props
        .entry("/build/ordered.cxx".into())
        .or_default();
    dir.source_props
        .entry("/src/ordered.cxx".into())
        .or_default();
    dir.final_policies = Some(policies);
    let mut target = Target::new(
        "target".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target.sources.push(BT::new(
        "registered.cxx;registered;exact.cxx".into(),
        Default::default(),
    ));
    model.add_target(target)?;
    let mut paths = SourcePaths::new(&host);
    paths.index_source_records(&model);
    let dir = model.dir(DirId::ROOT);
    assert_eq!(
        paths.find(dir, b"registered"),
        paths.find(dir, b"registered.cxx")
    );
    assert_eq!(
        paths.find(dir, b"ordered"),
        Some("/build/ordered.cxx".into())
    );
    assert_eq!(paths.find(dir, b"unregistered"), None);
    assert_eq!(paths.find(dir, b"exact"), Some("/src/exact".into()));
    let mut strict = dir.clone();
    policies.set(PolicyId::new(187), PolicyStatus::New)?;
    strict.final_policies = Some(policies);
    assert_eq!(paths.find(&strict, b"registered"), None);
    assert_eq!(
        paths.find(&strict, b"registered.cxx"),
        Some("/src/registered.cxx".into())
    );
    Ok(())
}

#[cfg(test)]
#[test]
fn extensionless_lookup_uses_directory_registration_order_and_binds_missing_first_record()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::TargetOrigin;

    let host = rcmake_host::PureHost::new(to_native(b"/src"), to_native(b"/bin/cmake"));
    for path in [
        b"/src/foo.cxx".as_slice(),
        b"/src/foo.c",
        b"/src/missing.cxx",
        b"/build/gen.cxx",
        b"/src/gen.c",
    ] {
        host.seed_file(&to_native(path), b"")?;
    }

    let mut model = ProjectModel::default();
    let dir = model.dir_mut(DirId::ROOT);
    dir.source_dir = "/src".into();
    dir.binary_dir = "/build".into();
    let mut policies = rcmake_diag::PolicyMap::default();
    policies.set(PolicyId::new(115), PolicyStatus::New)?;
    dir.final_policies = Some(policies);

    // Registration order mirrors:
    //   add_executable(a foo.cxx)
    //   set_source_files_properties(foo.c PROPERTIES ...)
    //   set_source_files_properties(missing.c PROPERTIES ...)
    //   add_executable(b missing.cxx)
    //   add_executable(c gen.cxx)
    //   set_source_files_properties(gen.c PROPERTIES ...)
    dir.props.set(
        b"\0rcmake-source-order".as_slice().into(),
        b"Rfoo.cxx\0Rfoo.c\0Rmissing.c\0Rmissing.cxx\0Rgen.cxx\0Rgen.c\0"
            .as_slice()
            .into(),
    );
    dir.source_props
        .entry("/src/foo.c".into())
        .or_default()
        .set(
            b"\0rcmake-source-binary-alternative".as_slice().into(),
            "/build/foo.c".into(),
        );
    dir.source_props
        .entry("/src/missing.c".into())
        .or_default()
        .set(
            b"\0rcmake-source-binary-alternative".as_slice().into(),
            "/build/missing.c".into(),
        );
    dir.source_props
        .entry("/src/gen.c".into())
        .or_default()
        .set(
            b"\0rcmake-source-binary-alternative".as_slice().into(),
            "/build/gen.c".into(),
        );

    let mut target = Target::new(
        "target".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target.sources.push(BT::new(
        "foo.cxx;missing.cxx;gen.cxx".into(),
        Default::default(),
    ));
    model.add_target(target)?;

    let mut paths = SourcePaths::new(&host);
    paths.set_collect_source_errors(false);
    paths.index_source_records(&model);
    let dir = model.dir(DirId::ROOT);

    // 1. Target source registered before source_props wins over later source_props.
    assert_eq!(paths.find(dir, b"foo"), Some("/src/foo.cxx".into()));

    // 2. Relative target source existing only in binary dir wins over later source_props.
    assert_eq!(paths.find(dir, b"gen"), Some("/build/gen.cxx".into()));

    // 3. First matching registered record (`missing.c`) is NOT skipped even when absent on disk,
    //    and resolve_at reports `missing.c` rather than falling through to `missing.cxx`.
    assert_eq!(paths.find(dir, b"missing"), None);
    let err = paths
        .resolve_at(&model, dir, &BT::new("missing".into(), Default::default()))
        .unwrap_err();
    assert!(
        err.to_string().contains("missing.c"),
        "expected missing.c diagnostic, got: {err}"
    );

    // Even under CMP0115 OLD, the bound missing record is reported instead of resolving missing.cxx.
    let mut old_dir = dir.clone();
    let mut old_policies = rcmake_diag::PolicyMap::default();
    old_policies.set(PolicyId::new(115), PolicyStatus::Old)?;
    old_dir.final_policies = Some(old_policies);
    let err = paths
        .resolve_at(
            &model,
            &old_dir,
            &BT::new("missing".into(), Default::default()),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("missing.c"),
        "expected missing.c diagnostic under CMP0115 OLD, got: {err}"
    );
    Ok(())
}
