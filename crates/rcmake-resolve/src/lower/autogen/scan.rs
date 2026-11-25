//! Qt version and tool discovery, and the configure-time source inventory.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth, version::Version};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, parse};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, path::to_native,
};
use rcmake_model::{ProjectModel, Target, TargetId};

use super::{ConfiguredSources, checksum::PathChecksum};
use crate::ResolveError;
use crate::lower::{compute_artifacts, join};
use crate::sources::{SourcePaths, absolute_path, properties};
use crate::usage::{ModelGenexHost, UsageResolver, handle};

/// Header extensions in companion-header search order.
pub(super) const HEADER_EXTENSIONS: &[&[u8]] =
    &[b"h", b"hh", b"h++", b"hm", b"hpp", b"hxx", b"in", b"txx"];

const C_EXTENSIONS: &[&[u8]] = &[
    b"c", b"cc", b"cpp", b"c++", b"cxx", b"cxxm", b"ccm", b"c++m", b"cppm", b"ixx", b"m", b"mm",
    b"mpp", b"cu",
];
const RESOURCE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Qt tools that can generate source and header files.
pub(super) enum Tool {
    /// Generate C++ meta-object code.
    Moc,
    /// Generate C++ headers from UI files.
    Uic,
    /// Generate C++ source from resource manifests.
    Rcc,
}

impl Tool {
    /// Tools in the order of diagnostic reporting.
    pub(super) const ALL: [Self; 3] = [Self::Moc, Self::Uic, Self::Rcc];

    /// Return the executable name.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Moc => "moc",
            Self::Uic => "uic",
            Self::Rcc => "rcc",
        }
    }

    /// Return the target property that enables the tool.
    pub(super) fn property(self) -> &'static [u8] {
        match self {
            Self::Moc => b"AUTOMOC",
            Self::Uic => b"AUTOUIC",
            Self::Rcc => b"AUTORCC",
        }
    }

    fn property_key(self, suffix: &[u8]) -> BString {
        [self.property(), suffix].concat().into()
    }

    /// Check the tool property that target creation initialized.
    ///
    /// Directory variables set after target creation cannot enable a tool for this target.
    pub(super) fn requested(self, target: &Target) -> bool {
        target
            .props
            .get(self.property())
            .is_some_and(|value| truth::is_on(value))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
/// Qt major and minor version.
pub(super) struct QtVersion(pub(super) u32, pub(super) u32);

#[derive(Clone, Debug, Default)]
/// A default setting and its configuration-specific values.
pub(super) struct ConfigValues<T> {
    /// Setting for the default configuration.
    pub(super) default: T,
    /// Whether each configuration selects its own executable.
    pub(super) per_config: IndexMap<ConfigId, T>,
}

#[derive(Clone, Debug, Default)]
/// Selected executable and dependencies for one tool.
pub(super) struct ToolPlan {
    /// Whether to enable the tool for the target.
    pub(super) enabled: bool,
    /// Executable paths to record in the metadata.
    pub(super) executable: ConfigValues<BString>,
    /// Options that ask rcc to list resource inputs.
    pub(super) list_options: ConfigValues<Vec<BString>>,
    /// Target that supplies the executable, if one exists.
    pub(super) dependency: Option<TargetId>,
    /// Targets and configurations referenced by an executable override.
    pub(super) artifact_deps: IndexSet<(TargetId, ConfigId)>,
}

/// Find the Qt version from project settings or a registered moc executable.
pub(super) fn qt_version(
    model: &ProjectModel,
    host: &dyn Host,
    id: TargetId,
    target: &Target,
    sources: &[ConfiguredSources],
) -> Result<QtVersion, ResolveError> {
    let major = target
        .props
        .get(b"QT_MAJOR_VERSION")
        .and_then(|value| uint(value))
        .or_else(|| {
            sources
                .iter()
                .flat_map(|config| &config.link_targets)
                .filter_map(|id| model.target(*id))
                .filter_map(|link| link.props.get(b"INTERFACE_QT_MAJOR_VERSION"))
                .find_map(|value| uint(value))
        })
        .filter(|major| *major != 0);

    let dir = model.dir(target.dir);
    for variables in [true, false] {
        for prefix in [
            b"Qt6Core_VERSION_".as_slice(),
            b"Qt5Core_VERSION_",
            b"QT_VERSION_",
        ] {
            let get = |suffix: &[u8]| {
                let key = [prefix, suffix].concat();
                if variables {
                    model.definition(target.dir, &key)
                } else {
                    dir.props.get(&key)
                }
                .and_then(|value| uint(value))
            };
            if let Some(found) = get(b"MAJOR")
                .filter(|found| *found != 0 && major.is_none_or(|major| major == *found))
            {
                return Ok(QtVersion(found, get(b"MINOR").unwrap_or_default()));
            }
        }
    }

    let mut usage = UsageResolver::new(model);
    let mut deps = IndexSet::new();
    let mut program = BString::default();
    if let Some(value) = target
        .props
        .get(b"AUTOMOC_EXECUTABLE")
        .filter(|value| !truth::is_off(value))
    {
        program = eval_property(
            &mut usage,
            id,
            target,
            &ConfigId::default(),
            b"AUTOMOC_EXECUTABLE",
            value,
            &mut deps,
        )?;
    }
    if program.is_empty() {
        for candidate in [6, 5, 4]
            .into_iter()
            .filter(|candidate| major.is_none_or(|major| major == *candidate))
        {
            let name = format!("Qt{candidate}::moc");
            if let Some(tool) = model
                .find_target(target.dir, name.as_bytes())
                .and_then(|id| model.target(id))
                && let Some(file) = compute_artifacts(model, tool, &ConfigId::default()).file
            {
                program = file;
                break;
            }
        }
    }
    if program.is_empty() {
        return Ok(QtVersion::default());
    }
    let found = run_tool(host, &program, &dir.binary_dir, vec!["--version".into()])
        .ok()
        .filter(|out| out.results == [ProcessResult::Exited(0)])
        .and_then(|out| {
            version_from_output(&out.stdout).or_else(|| version_from_output(&out.stderr))
        });
    Ok(found.unwrap_or_default())
}

fn version_from_output(output: &[u8]) -> Option<QtVersion> {
    output
        .split(|byte| !byte.is_ascii_digit() && *byte != b'.')
        .filter(|token| token.contains(&b'.'))
        .filter_map(Version::parse_strict)
        .map(|version| QtVersion(version.major(), version.minor()))
        .next()
}

/// Parse a non-negative decimal value that fits in a 32-bit integer.
pub(super) fn uint(value: &[u8]) -> Option<u32> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Evaluate a target property and collect its artifact dependencies.
pub(super) fn eval_property(
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    config: &ConfigId,
    property: &[u8],
    value: &[u8],
    deps: &mut IndexSet<(TargetId, ConfigId)>,
) -> Result<BString, ResolveError> {
    let ast = parse(value)?;
    let dependencies = std::cell::RefCell::new(IndexSet::new());
    let mut adapter = ModelGenexHost::new(usage, target.dir);
    adapter.current = Some(handle(id));
    adapter.artifact_dependencies = Some(&dependencies);
    let (value, _) = evaluate(
        &ast,
        &EvalContext {
            config: config.clone(),
            current: Some(handle(id)),
            head: Some(handle(id)),
            site: EvalSite {
                backtrace: target.backtrace.clone(),
                property: Some(property.into()),
            },
            ..EvalContext::default()
        },
        &mut adapter,
    )?;
    deps.extend(dependencies.into_inner());
    Ok(value)
}

/// Inputs used to select the executable for a target.
pub(super) struct ToolDiscovery<'a> {
    /// Configured project model.
    pub(super) model: &'a ProjectModel,
    /// Host used to read files and query executable capabilities.
    pub(super) host: &'a dyn Host,
    /// Target that requests the tool.
    pub(super) id: TargetId,
    /// Target data.
    pub(super) target: &'a Target,
    /// Selected Qt version.
    pub(super) version: QtVersion,
    /// Configurations in generation order.
    pub(super) configurations: &'a [ConfigId],
    /// Whether each configuration selects its own executable.
    pub(super) per_config: bool,
}

impl ToolDiscovery<'_> {
    /// Resolve the executable and its capabilities for an enabled tool.
    pub(super) fn find(
        &self,
        kind: Tool,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<ToolPlan, ResolveError> {
        let model = self.model;
        let target = self.target;
        if !kind.requested(target) {
            return Ok(ToolPlan::default());
        }
        let key = kind.property_key(b"_EXECUTABLE");
        let explicit = target.props.get(&key).filter(|value| !truth::is_off(value));
        if explicit.is_none() && self.version.0 == 0 {
            diagnostics.push(warning(
                target,
                None,
                format!(
                    "AUTOGEN: No valid Qt version found for target {}. {} disabled.",
                    target.name,
                    String::from_utf8_lossy(kind.property()),
                )
                .into(),
            ));
            return Ok(ToolPlan::default());
        }
        let name = format!("Qt{}::{}", self.version.0, kind.name());
        let dependency = explicit
            .is_none()
            .then(|| model.find_target(target.dir, name.as_bytes()))
            .flatten();
        let tool_target = dependency.and_then(|id| model.target(id));
        if explicit.is_none() && tool_target.is_none() && kind != Tool::Uic {
            return Err(ResolveError::Message(
                format!(
                    "AUTOGEN: Could not find {} executable target {name}",
                    kind.name(),
                )
                .into(),
            ));
        }
        let mut plan = ToolPlan {
            enabled: true,
            dependency,
            ..ToolPlan::default()
        };
        let mut usage = UsageResolver::new(model);
        let default = ConfigId::default();
        let selected = if self.per_config {
            self.configurations
        } else {
            std::slice::from_ref(self.configurations.first().unwrap_or(&default))
        };
        let cwd = &model.dir(target.dir).binary_dir;
        let imported = tool_target.is_some_and(Target::is_imported);
        let mut capabilities = BTreeMap::<BString, Vec<BString>>::new();
        for config in selected {
            let program = if let Some(raw) = explicit {
                let evaluation_config = if self.per_config { config } else { &default };
                eval_property(
                    &mut usage,
                    self.id,
                    target,
                    evaluation_config,
                    &key,
                    raw,
                    &mut plan.artifact_deps,
                )?
            } else {
                tool_target
                    .and_then(|tool| compute_artifacts(model, tool, config).file)
                    .unwrap_or_default()
            };
            if imported && !is_file(self.host, &program) {
                return Err(ResolveError::Message(
                    format!(
                        "AUTOGEN for target \"{}\": Imported executable \"{name}\" has no file at \"{program}\".",
                        target.name,
                    )
                    .into(),
                ));
            }
            if program.is_empty() && kind != Tool::Uic {
                return Err(ResolveError::Message(
                    format!("AUTOGEN: {key} evaluates to an empty value").into(),
                ));
            }
            let options = if kind == Tool::Rcc
                && explicit.is_none()
                && matches!(self.version.0, 5 | 6)
                && (imported || is_file(self.host, &program))
            {
                match capabilities.entry(program.clone()) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let options = rcc_list_options(self.host, &program, cwd)?;
                        entry.insert(options).clone()
                    }
                }
            } else {
                Vec::new()
            };
            if self.per_config {
                plan.executable.per_config.insert(config.clone(), program);
                if kind == Tool::Rcc {
                    plan.list_options.per_config.insert(config.clone(), options);
                }
            } else {
                plan.executable.default = program;
                plan.list_options.default = options;
            }
        }
        Ok(plan)
    }
}

fn rcc_list_options(
    host: &dyn Host,
    program: &[u8],
    cwd: &[u8],
) -> Result<Vec<BString>, ResolveError> {
    let error = |reason: &dyn std::fmt::Display| {
        ResolveError::Message(
            format!(
                "Cannot run the Qt resource compiler \"{}\" with -h. {reason}",
                BString::from(program)
            )
            .into(),
        )
    };
    let output =
        run_tool(host, program, cwd, vec!["-h".into()]).map_err(|reason| error(&reason))?;
    if !matches!(output.results.as_slice(), [ProcessResult::Exited(_)]) {
        return Err(error(&format!("{:?}", output.results)));
    }
    Ok([b"--list".as_slice(), b"-list"]
        .into_iter()
        .find(|option| {
            output
                .stdout
                .windows(option.len())
                .any(|part| part == *option)
        })
        .map_or_else(Vec::new, |option| vec![option.into()]))
}

fn run_tool(
    host: &dyn Host,
    program: &[u8],
    cwd: &[u8],
    args: Vec<BString>,
) -> rcmake_host::HostResult<RunOutput> {
    host.run_bounded(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: to_native(cwd),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(15)),
            tee: false,
        },
        RESOURCE_LIMIT,
    )
}

fn is_file(host: &dyn Host, path: &[u8]) -> bool {
    !path.is_empty()
        && host
            .metadata(&to_native(path), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
}

/// Create an author warning at the target declaration.
pub(super) fn warning(target: &Target, policy: Option<PolicyId>, body: BString) -> Diagnostic {
    Diagnostic {
        annotations: None,
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: None,
        policy,
        location: Location::Backtrace(target.backtrace.clone()),
        body,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Kinds of inputs recorded in the autogen scan lists.
pub(super) enum ScanKind {
    /// C or C++ header.
    Header,
    /// C-like source file.
    Source,
    /// C++ module unit.
    Module,
}

#[derive(Clone, Debug)]
/// Tool flags and configuration membership of a scan input.
pub(super) struct ScanFile {
    /// Kind of scan input.
    pub(super) kind: ScanKind,
    /// Configurations that use this file.
    pub(super) configs: IndexSet<ConfigId>,
    /// Whether moc processing is enabled.
    pub(super) moc: bool,
    /// Whether uic processing is enabled.
    pub(super) uic: bool,
    /// Whether a build rule generates the file.
    pub(super) generated: bool,
    /// Moc output path relative to the autogen output directory.
    pub(super) moc_output: BString,
}

#[derive(Clone, Debug)]
/// A resource manifest selected for rcc.
pub(super) struct ResourceFile {
    /// Whether a build rule generates the file.
    pub(super) generated: bool,
    /// Options from the source file property.
    pub(super) options: Vec<BString>,
}

#[derive(Default)]
/// Files and properties collected for the enabled tools.
pub(super) struct Scan {
    /// C++ inputs indexed by absolute path.
    pub(super) files: BTreeMap<BString, ScanFile>,
    /// Resource manifests indexed by absolute path.
    pub(super) qrc: BTreeMap<BString, ResourceFile>,
    /// Absolute paths that moc must ignore.
    pub(super) moc_skip: BTreeSet<BString>,
    /// Absolute paths that uic must ignore.
    pub(super) uic_skip: BTreeSet<BString>,
    /// Registered UI files and their options.
    pub(super) ui: BTreeMap<BString, Vec<BString>>,
}

/// Inputs used to select and classify source files.
pub(super) struct SourceDiscovery<'a, 'host> {
    /// Configured project model.
    pub(super) model: &'a ProjectModel,
    /// Original source and generated-output index.
    pub(super) paths: &'a SourcePaths<'host>,
    /// Target data.
    pub(super) target: &'a Target,
    /// Whether moc processing is enabled.
    pub(super) moc: bool,
    /// Whether uic processing is enabled.
    pub(super) uic: bool,
    /// Whether rcc processing is enabled.
    pub(super) rcc: bool,
}

impl SourceDiscovery<'_, '_> {
    /// Collect tool inputs, skip lists, and generated output names.
    pub(super) fn scan(
        &self,
        configs: &[ConfiguredSources],
        checksums: &PathChecksum<'_>,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Scan, ResolveError> {
        let dir = self.model.dir(self.target.dir);
        let policies = dir.final_policies.unwrap_or(self.target.policies.policies);
        let generated = policies.get(PolicyId::new(71));
        let hh = policies.get(PolicyId::new(100));
        let mut scan = Scan::default();
        let mut ignored_generated = BTreeSet::new();
        let mut ignored_hh = BTreeSet::new();

        for config in configs {
            for source in &config.sources {
                let ext = extension(&source.path);
                let props = properties(dir, &source.path);
                let has = |key: &[u8]| props.get(key).is_some_and(|value| truth::is_on(value));
                let is_generated = source.generated || self.paths.is_generated(dir, &source.path);
                if ext == b"qrc" && self.rcc && !has(b"SKIP_AUTOGEN") && !has(b"SKIP_AUTORCC") {
                    scan.qrc
                        .entry(source.path.clone())
                        .or_insert_with(|| ResourceFile {
                            generated: is_generated,
                            options: props
                                .get(b"AUTORCC_OPTIONS")
                                .map_or_else(Vec::new, |value| {
                                    list::split(value, list::Empty::Drop)
                                }),
                        });
                }
                let Some(kind) = source_kind(&ext, source.cxx_module) else {
                    continue;
                };
                let (moc, uic) = self.flags(&mut scan, &source.path);
                let uic = uic && kind != ScanKind::Module;
                if !(moc || uic) {
                    continue;
                }
                if ext == b"hh" && hh != PolicyStatus::New {
                    ignored_hh.insert(source.path.clone());
                    continue;
                }
                if is_generated && generated != PolicyStatus::New && kind != ScanKind::Module {
                    ignored_generated.insert(source.path.clone());
                    continue;
                }
                let entry = scan
                    .files
                    .entry(source.path.clone())
                    .or_insert_with(|| ScanFile {
                        kind,
                        moc,
                        uic,
                        generated: is_generated,
                        configs: IndexSet::new(),
                        moc_output: BString::default(),
                    });
                entry.configs.insert(config.config.clone());
            }
        }

        let sources: Vec<_> = scan
            .files
            .iter()
            .filter(|(_, file)| file.kind == ScanKind::Source)
            .map(|(path, file)| (path.clone(), file.clone()))
            .collect();
        for (source, flags) in sources {
            let parsed = Lexical::new(&source);
            let stem = parsed.stem_extension(true).0;
            for suffix in [b"".as_slice(), b"_p"] {
                for extension in HEADER_EXTENSIONS {
                    let name = [stem, suffix, b".", extension].concat();
                    let header = join(parsed.parent(), &name);
                    let is_generated = self.paths.is_generated(dir, &header);
                    if scan
                        .files
                        .get(&header)
                        .is_some_and(|file| !file.configs.is_disjoint(&flags.configs))
                        || (!is_generated && !is_file(self.paths.host, &header))
                    {
                        continue;
                    }
                    let (moc, uic) = self.flags(&mut scan, &header);
                    let (moc, uic) = (moc && flags.moc, uic && flags.uic);
                    if !(moc || uic) {
                        continue;
                    }
                    if *extension == b"hh" && hh != PolicyStatus::New {
                        ignored_hh.insert(header);
                        continue;
                    }
                    if is_generated && generated != PolicyStatus::New {
                        ignored_generated.insert(header);
                        continue;
                    }
                    let entry = scan.files.entry(header).or_insert_with(|| ScanFile {
                        kind: ScanKind::Header,
                        moc,
                        uic,
                        generated: is_generated,
                        configs: IndexSet::new(),
                        moc_output: BString::default(),
                    });
                    entry.configs.extend(flags.configs.iter().cloned());
                }
            }
        }

        if self.moc || self.uic {
            for path in dir.source_props.keys().chain(
                configs
                    .iter()
                    .flat_map(|config| config.sources.iter().map(|source| &source.path)),
            ) {
                let ext = extension(path);
                if ext == b"ui" && self.uic {
                    if self.flags(&mut scan, path).1 {
                        let opts = properties(dir, path)
                            .get(b"AUTOUIC_OPTIONS")
                            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
                        scan.ui.entry(path.clone()).or_insert(opts);
                    }
                } else if source_kind(&ext, false).is_some() {
                    self.flags(&mut scan, path);
                }
            }
        }

        for (status, policy, files, reason) in [
            (generated, 71, ignored_generated, "GENERATED files"),
            (hh, 100, ignored_hh, ".hh files"),
        ] {
            if status == PolicyStatus::Warn && !files.is_empty() {
                let files = BString::from(
                    files
                        .iter()
                        .map(|path| path.as_slice())
                        .collect::<Vec<_>>()
                        .join(b"\n  ".as_slice()),
                );
                diagnostics.push(warning(self.target, Some(PolicyId::new(policy)), format!(
                    "Policy CMP{policy:04} is not set. For compatibility, AUTOGEN for target {} excludes {reason}:\n  {files}",
                    self.target.name,
                ).into()));
            }
        }

        let mut outputs = BTreeSet::new();
        // Module units claim names first because their outputs are declared in the graph.
        for kind in [ScanKind::Module, ScanKind::Header] {
            for (path, file) in scan
                .files
                .iter_mut()
                .filter(|(_, file)| file.kind == kind && file.moc)
            {
                file.moc_output = moc_output(path, checksums, &mut outputs)?;
            }
        }
        Ok(scan)
    }

    fn flags(&self, scan: &mut Scan, path: &BString) -> (bool, bool) {
        let props = properties(self.model.dir(self.target.dir), path);
        let on = |name: &[u8]| props.get(name).is_some_and(|value| truth::is_on(value));
        let skip_all = on(b"SKIP_AUTOGEN");
        let skip_moc = skip_all || on(b"SKIP_AUTOMOC");
        let skip_uic = skip_all || on(b"SKIP_AUTOUIC");
        if self.moc && skip_moc && extension(path) != b"ui" {
            scan.moc_skip.insert(path.clone());
        }
        if self.uic && skip_uic {
            scan.uic_skip.insert(path.clone());
        }
        (self.moc && !skip_moc, self.uic && !skip_uic)
    }
}

fn extension(path: &[u8]) -> Vec<u8> {
    let ext = Lexical::new(path).stem_extension(true).1;
    ext.strip_prefix(b".").unwrap_or(ext).to_ascii_lowercase()
}

fn source_kind(ext: &[u8], module: bool) -> Option<ScanKind> {
    if module {
        Some(ScanKind::Module)
    } else if HEADER_EXTENSIONS.contains(&ext) {
        Some(ScanKind::Header)
    } else if C_EXTENSIONS.contains(&ext) {
        Some(ScanKind::Source)
    } else {
        None
    }
}

fn moc_output(
    path: &[u8],
    checksums: &PathChecksum<'_>,
    emitted: &mut BTreeSet<BString>,
) -> Result<BString, ResolveError> {
    let (stem, ext) = Lexical::new(path).stem_extension(true);
    let checksum = checksums.for_file(path);
    let base = [checksum.as_slice(), b"/moc_", stem].concat();
    let ext = ext.strip_prefix(b".").unwrap_or(ext);
    let output = BString::from([base.as_slice(), b".cpp".as_slice()].concat());
    if emitted.insert(output.clone()) {
        return Ok(output);
    }
    let base = [base.as_slice(), b"_", ext].concat();
    for number in 1..=255 {
        let suffix = if number == 1 {
            String::new()
        } else {
            format!("_{number}")
        };
        let output = BString::from([base.as_slice(), suffix.as_bytes(), b".cpp"].concat());
        if emitted.insert(output.clone()) {
            return Ok(output);
        }
    }
    Err(ResolveError::Message(
        format!("moc output file name conflict for {}", BString::from(path)).into(),
    ))
}

/// List manifest inputs in the order used by the resource compiler.
pub(super) fn resource_inputs(
    host: &dyn Host,
    manifest: &[u8],
    program: &[u8],
    options: &[BString],
) -> Result<Vec<BString>, ResolveError> {
    let path = Lexical::new(manifest);
    let dir = path.parent();
    let error = |error: &dyn std::fmt::Display| {
        ResolveError::Message(
            format!(
                "Cannot list resources from {}. {error}",
                BString::from(manifest),
            )
            .into(),
        )
    };
    let names = if options.is_empty() || !is_file(host, program) {
        let data = host
            .read(
                &to_native(manifest),
                ReadRange::All {
                    limit: RESOURCE_LIMIT as u64,
                },
            )
            .map_err(|err| error(&err))?;
        parse_resource_entries(&data).map_err(|reason| error(&reason))?
    } else {
        let mut args = options.to_vec();
        args.push(path.filename().into());
        let result = run_tool(host, program, dir, args).map_err(|err| error(&err))?;
        if result.results != [ProcessResult::Exited(0)] {
            return Err(error(&BString::from(
                [result.stdout, result.stderr].concat(),
            )));
        }
        let mut entries: Vec<BString> = lines(&result.stdout).map(BString::from).collect();
        for line in lines(&result.stderr).filter(|line| line.starts_with(b"RCC: Error in")) {
            let message = b"Cannot find file '";
            let Some(start) = line.windows(message.len()).position(|part| part == message) else {
                return Err(error(&BString::from(line)));
            };
            let Some(file) = line
                .get(start + message.len()..)
                .and_then(|tail| tail.strip_suffix(b"'"))
            else {
                return Err(error(&BString::from(line)));
            };
            entries.push(file.into());
        }
        entries
    };
    Ok(names.iter().map(|name| absolute_path(dir, name)).collect())
}

fn lines(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input
        .split(|&byte| byte == b'\n')
        .filter_map(|line| line.split(|&byte| byte == b'\r').next())
        .filter(|line| !line.is_empty())
}

fn parse_resource_entries(mut input: &[u8]) -> Result<Vec<BString>, &'static str> {
    const ENTRY_LIMIT: usize = 100_000;
    if let Some(end) = input.iter().position(|&byte| byte == 0) {
        input = input.get(..end).unwrap_or_default();
    }
    let mut entries = Vec::new();
    while let Some(start) = input
        .windows(6)
        .position(|bytes| bytes.starts_with(b"<file") && bytes.last() != Some(&b'<'))
    {
        let after_name = input.get(start + 5..).unwrap_or_default();
        let length = 5 + after_name
            .iter()
            .position(|&byte| byte == b'<')
            .unwrap_or(after_name.len());
        let entry = input.get(start..start + length).unwrap_or_default();
        let value = entry
            .iter()
            .position(|&byte| byte == b'>')
            .and_then(|end| entry.get(end + 1..))
            .unwrap_or(entry);
        // The reference advances by the match length from the search start.
        // A long gap before a match can make it report the same entry again.
        let repeats = start / length + 1;
        if entries.len() + repeats > ENTRY_LIMIT {
            return Err("The resource list exceeds the entry limit.");
        }
        entries.extend(std::iter::repeat_with(|| BString::from(value)).take(repeats));
        input = input.get(repeats * length..).unwrap_or_default();
    }
    Ok(entries)
}

/// Merge tool options and replace values for options that take an argument.
pub(super) fn merge_options(
    options: &mut Vec<BString>,
    incoming: Vec<BString>,
    value_options: &[&[u8]],
    modern: bool,
) {
    let mut incoming = incoming.into_iter();
    while let Some(option) = incoming.next() {
        let key = option_key(&option, modern);
        let Some(index) = options
            .iter()
            .position(|old| old == &option || (key.is_some() && option_key(old, modern) == key))
        else {
            options.push(option);
            continue;
        };
        if key.is_some_and(|key| value_options.contains(&key))
            && let Some(value) = incoming.next()
            && let Some(slot) = options.get_mut(index + 1)
        {
            *slot = value;
        }
    }
}

fn option_key(value: &[u8], modern: bool) -> Option<&[u8]> {
    let key = value.strip_prefix(b"-")?;
    Some(if modern {
        key.strip_prefix(b"-").unwrap_or(key)
    } else {
        key
    })
}

#[cfg(test)]
mod tests;
