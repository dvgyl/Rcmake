//! Link and archive rules.

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal, evaluate};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId};

use super::artifacts::{build_relative, is_darwin, is_msvc};
use super::compile::language_flags;
use super::launcher;
use super::policy_flags::Flags;
use super::shell::{command as shell_command, direct, join_words, simple_args};
use super::{definition, join};
use crate::{
    ArtifactSet, BuildStep, LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget,
    sources::TargetInputs,
    usage::{UsageResolver, link_property_traversal},
};

pub(super) mod driver;
mod frameworks;
mod install_name;
mod linker_type;
pub(crate) use install_name::{install_name_dir, install_name_prefix, warn_install_name_policy};
mod response;
pub(super) mod rpath;
mod rpath_macho;
use driver::{
    LinkerDriver, archive_commands, deduplicate_darwin_flags, definition_file, library_file,
    shared_library, target_definition_files, without_empty_output_flags,
};
pub(crate) use frameworks::bundle_name as framework_bundle_name;
pub(crate) use rpath::ide_rpaths;
pub(crate) use rpath::{SharedDependencyMode, links_shared_dependency, shared_dependency_mode};

fn creation_flags<'a>(
    model: &'a ProjectModel,
    target: &Target,
    kind: LinkKind,
    language: &[u8],
) -> Option<&'a BString> {
    let key = if kind == LinkKind::Executable
        && target.policies.policies.get(PolicyId::new(210)) != PolicyStatus::New
    {
        BString::from([b"CMAKE_", language, b"_LINK_FLAGS"].concat())
    } else {
        kind.create_flags_key(language)
    };
    definition(model, target.dir, &key).or_else(|| {
        if kind == LinkKind::ModuleLibrary {
            definition(
                model,
                target.dir,
                &LinkKind::SharedLibrary.create_flags_key(language),
            )
        } else {
            None
        }
    })
}

/// Append a flag string using CMake's `cmLocalGenerator::AppendFlags` rules:
/// ignore whitespace-only strings, separate non-empty values with a single
/// space, and preserve any leading/trailing spaces on non-whitespace values.
fn append_flag(out: &mut BString, piece: &[u8]) {
    if piece.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    if !out.is_empty() {
        out.push(b' ');
    }
    out.extend_from_slice(piece);
}

fn join_link_flags(pieces: impl IntoIterator<Item = BString>) -> BString {
    let mut out = BString::default();
    for piece in pieces {
        append_flag(&mut out, &piece);
    }
    out
}

fn win32_executable(
    model: &ProjectModel,
    target_id: Option<TargetId>,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    usage: Option<&mut UsageResolver<'_>>,
) -> Result<bool, ResolveError> {
    let Some(raw) = target.props.get(b"WIN32_EXECUTABLE") else {
        return Ok(false);
    };
    if !raw.windows(2).any(|part| part == b"$<") {
        return Ok(truth::is_on(raw));
    }
    let handle = target_id
        .or_else(|| model.find_target(target.dir, &target.name))
        .map(crate::usage::handle);
    let ctx = EvalContext {
        config: config.clone(),
        head: handle,
        current: handle,
        link_language: Some(language.into()),
        language: Some(language.into()),
        traversal: UsageTraversal::Link,
        ..EvalContext::default()
    };
    let evaluate_prop = |usage: &mut UsageResolver<'_>| {
        let ast = usage.parse_genex(raw)?;
        evaluate(&ast, &ctx, &mut usage.host(target.dir))
            .map(|(value, _)| truth::is_on(&value))
            .map_err(ResolveError::from)
    };
    match usage {
        Some(usage) => evaluate_prop(usage),
        None => evaluate_prop(&mut UsageResolver::new(model)),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Executable extra-flag resolution evaluates WIN32_EXECUTABLE per target, config, and language."
)]
fn executable_extra_flags(
    model: &ProjectModel,
    target_id: Option<TargetId>,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    wrapper: &LinkerDriver<'_>,
    policy_181_new: bool,
    usage: Option<&mut UsageResolver<'_>>,
) -> Result<BString, ResolveError> {
    let mut extra = BString::default();
    let form = if win32_executable(model, target_id, target, config, language, usage)? {
        b"_CREATE_WIN32_EXE".as_slice()
    } else {
        b"_CREATE_CONSOLE_EXE"
    };
    if let Some(value) = definition(model, target.dir, &[b"CMAKE_", language, form].concat()) {
        let parsed = if policy_181_new {
            wrapper.flag_string(value)?
        } else {
            wrapper.driver_flags(value)
        };
        append_flag(&mut extra, &parsed);
    }
    let exports = target
        .props
        .get(b"ENABLE_EXPORTS")
        .is_some_and(|v| truth::is_on(v));
    if exports
        && let Some(value) = definition(
            model,
            target.dir,
            &[b"CMAKE_EXE_EXPORTS_", language, b"_FLAG"].concat(),
        )
    {
        append_flag(&mut extra, &wrapper.driver_flags(value));
    }
    let shared_build =
        definition(model, target.dir, b"BUILD_SHARED_LIBS").is_some_and(|v| truth::is_on(v));
    if shared_build {
        extra.push(b' ');
        if let Some(value) = definition(
            model,
            target.dir,
            &[b"CMAKE_SHARED_BUILD_", language, b"_FLAGS"].concat(),
        ) {
            extra.extend_from_slice(&wrapper.driver_flags(value));
        }
    }
    let shared_exports = exports
        && model
            .global
            .props
            .get(b"TARGET_SUPPORTS_SHARED_LIBS")
            .is_some_and(|v| truth::is_on(v))
        && definition(model, target.dir, b"CMAKE_SYSTEM_NAME").is_none_or(|v| v != b"AIX");
    if shared_exports {
        if let Some(value) = definition(
            model,
            target.dir,
            &[b"CMAKE_SHARED_LIBRARY_LINK_", language, b"_FLAGS"].concat(),
        ) {
            append_flag(&mut extra, &wrapper.driver_flags(value));
        }
        extra.push(b' ');
    }
    if let Some(value) = target.props.get(b"LINK_FLAGS") {
        append_flag(&mut extra, &wrapper.driver_flags(value));
    }
    if let Some(value) = target
        .props
        .get(&[b"LINK_FLAGS_", config.0.to_ascii_uppercase().as_slice()].concat())
    {
        append_flag(&mut extra, &wrapper.driver_flags(value));
    }
    Ok(extra)
}

/// Return effective `POSITION_INDEPENDENT_CODE` for `target`, checking the
/// target property and transitive compile-usage (`compatibility_targets`)
/// `INTERFACE_POSITION_INDEPENDENT_CODE` entries — excluding `$<LINK_ONLY:...>`.
fn effective_position_independent(
    model: &ProjectModel,
    target_id: TargetId,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    usage: Option<&mut UsageResolver<'_>>,
) -> Option<bool> {
    if let Some(value) = target.props.get(b"POSITION_INDEPENDENT_CODE") {
        return Some(truth::is_on(value));
    }
    if let Some(usage) = usage {
        let ctx = EvalContext {
            config: config.clone(),
            traversal: UsageTraversal::Compile,
            link_language: Some(language.into()),
            head: Some(crate::usage::handle(target_id)),
            ..EvalContext::default()
        };
        let deps = usage.compile_targets(target_id, &ctx).unwrap_or_default();
        let mut resolved = None;
        for dep_id in deps {
            if dep_id == target_id {
                continue;
            }
            let Some(dep) = model.target(dep_id) else {
                continue;
            };
            let Some(raw) = dep.props.get(b"INTERFACE_POSITION_INDEPENDENT_CODE") else {
                continue;
            };
            let value = if rcmake_genex::find_genex(raw).is_some() {
                let mut host = usage.host(dep.dir);
                rcmake_genex::parse(raw)
                    .and_then(|expr| evaluate(&expr, &ctx, &mut host))
                    .map(|(val, _)| val)
                    .unwrap_or_else(|_| raw.clone())
            } else {
                raw.clone()
            };
            if !value.is_empty() {
                resolved = Some(truth::is_on(&value));
            }
        }
        if resolved.is_some() {
            return resolved;
        }
    }
    None
}

/// Return the untagged flag fragments before link options, post-option flag
/// fragments (such as PIE flags), and IPO status of a codemodel link step.
pub fn codemodel_link_flags(
    model: &ProjectModel,
    target_id: TargetId,
    target: &Target,
    config: &ConfigId,
    link: &LinkStep,
    usage: Option<&mut UsageResolver<'_>>,
) -> Result<(Vec<BString>, Vec<BString>, bool), ResolveError> {
    let kind = link.kind;
    let language = link.language.as_slice();
    let mut flags = Vec::new();
    let mut post_flags = Vec::new();
    let policy = Flags::new(model, target, language);
    let ipo = policy.ipo(config);
    let wrapper = LinkerDriver::new(model, target, language);
    let policy_new =
        |policy| target.policies.policies.get(PolicyId::new(policy)) == PolicyStatus::New;
    let push_non_empty = |flags: &mut Vec<BString>, value: &[u8]| {
        let trimmed = value.trim_ascii();
        if !trimmed.is_empty() {
            flags.push(trimmed.into());
        }
    };
    if kind == LinkKind::StaticArchive {
        if language != b"Swift" {
            let base = b"CMAKE_STATIC_LINKER_FLAGS".to_vec();
            let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
            let mut linker_flags = BString::default();
            for key in [&base, &configured] {
                if let Some(value) = definition(model, target.dir, key) {
                    append_flag(&mut linker_flags, &wrapper.driver_flags(value));
                }
            }
            push_non_empty(&mut flags, &linker_flags);
        }
        let mut target_flags = BString::default();
        let raw_key = kind.target_flags_property();
        if let Some(value) = target.props.get(raw_key) {
            append_flag(&mut target_flags, &wrapper.driver_flags(value));
        }
        if let Some(value) = target
            .props
            .get(&[raw_key, b"_", config.0.to_ascii_uppercase().as_slice()].concat())
        {
            append_flag(&mut target_flags, &wrapper.driver_flags(value));
        }
        push_non_empty(&mut flags, &target_flags);
        return Ok((flags, post_flags, ipo));
    }
    let split_swift = super::swift::is_split_build(model, target);
    if kind == LinkKind::Executable {
        let mut language_flags = language_flags(model, target, config, language);
        if ipo {
            language_flags.extend(
                policy
                    .option(b"COMPILE_OPTIONS_IPO")
                    .into_iter()
                    .flat_map(|value| list::split(value, list::Empty::Drop)),
            );
        }
        if language == b"CUDA" {
            language_flags.extend(
                super::cuda::link_flags(model, target, config)
                    .iter()
                    .map(|flag| super::compile::codemodel_flag_quote(model, target, flag)),
            );
        } else if language == b"HIP" {
            language_flags.extend(
                super::hip::link_flags(model, target, config)
                    .iter()
                    .map(|flag| super::compile::codemodel_flag_quote(model, target, flag)),
            );
        } else if language == b"Swift" && split_swift {
            let compiler_version = definition(model, target.dir, b"CMAKE_Swift_COMPILER_VERSION")
                .map_or(b"".as_slice(), |value| value.as_slice());
            if compiler_version.is_empty()
                || rcmake_base::version::compare(compiler_version, b"5.8").is_ge()
            {
                let package = super::swift::swift_package_name(model, target);
                if !package.is_empty()
                    && let Some(flag) =
                        definition(model, target.dir, b"CMAKE_Swift_PACKAGE_NAME_FLAG")
                            .filter(|value| !value.is_empty())
                {
                    language_flags.push(flag.clone());
                    language_flags.push(super::compile::codemodel_flag_quote(
                        model, target, &package,
                    ));
                }
            }
        }
        // `DumpLinkCommandFragments` trims the language flags and drops them when empty.
        push_non_empty(&mut flags, &join_link_flags(language_flags));
    }
    if let Some(value) = creation_flags(model, target, kind, language) {
        let value = if policy_new(181) {
            wrapper.flag_string(value)?
        } else {
            wrapper.driver_flags(value)
        };
        push_non_empty(&mut flags, &value);
    }
    let include_linker_flags = match kind {
        LinkKind::Executable => language != b"Swift" || (split_swift && policy_new(214)),
        LinkKind::SharedLibrary | LinkKind::ModuleLibrary => language != b"Swift" || split_swift,
        LinkKind::StaticArchive => language != b"Swift",
    };
    let mut linker_flags = BString::default();
    if include_linker_flags {
        let base = [b"CMAKE_", kind.flag_class(), b"_LINKER_FLAGS"].concat();
        let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
        for key in [&base, &configured] {
            if let Some(value) = definition(model, target.dir, key) {
                append_flag(&mut linker_flags, &wrapper.driver_flags(value));
            }
        }
    }
    if policy_new(210) {
        let base = [b"CMAKE_", language, b"_LINK_FLAGS"].concat();
        let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
        for key in [&base, &configured] {
            if let Some(value) = definition(model, target.dir, key) {
                append_flag(&mut linker_flags, &wrapper.flag_string(value)?);
            }
        }
    }
    push_non_empty(&mut flags, &linker_flags);
    let mut usage = usage;
    if kind == LinkKind::Executable {
        let extra = executable_extra_flags(
            model,
            Some(target_id),
            target,
            config,
            language,
            &wrapper,
            policy_new(181),
            usage.as_deref_mut(),
        )?;
        // CMake checks `!extraFlags.empty()` before trimming, so `BUILD_SHARED_LIBS=ON`
        // with no other extra flags emits `{"fragment": "", "role": "flags"}`.
        if !extra.is_empty() {
            flags.push(extra.trim_ascii().into());
        }
        let pic = effective_position_independent(model, target_id, target, config, language, usage);
        let pie = policy.pie_link(pic);
        if !pie.is_empty() {
            push_non_empty(&mut post_flags, &join_link_flags(pie));
        }
    } else {
        let mut extra = BString::default();
        let raw_key = kind.target_flags_property();
        if let Some(value) = target.props.get(raw_key) {
            append_flag(&mut extra, &wrapper.driver_flags(value));
        }
        if let Some(value) = target
            .props
            .get(&[raw_key, b"_", config.0.to_ascii_uppercase().as_slice()].concat())
        {
            append_flag(&mut extra, &wrapper.driver_flags(value));
        }
        push_non_empty(&mut flags, &extra);
    }
    Ok((flags, post_flags, ipo))
}

pub(super) struct LoweredLink {
    pub(super) step: BuildStep,
    pub(super) install_name_dir: BString,
    pub(super) depfile: Option<BString>,
    pub(super) runtime_paths: Option<crate::LinkRuntimePaths>,
    pub(super) extra_outputs: Vec<BString>,
    pub(super) swift_output_file_map: Option<(BString, BString)>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Link lowering uses configured artifacts, objects, and usage requirements."
)]
pub(super) fn link(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    mut step: LinkStep,
    objects: Vec<BString>,
    (resolved, target_inputs, framework_dirs): (
        &ResolvedTarget,
        Option<&TargetInputs>,
        &[(BString, bool)],
    ),
    position_independent: Option<bool>,
    link_languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<LoweredLink, ResolveError> {
    let sources = target_inputs.map(|inputs| &inputs.sources);
    if step.output.is_empty()
        && let Some(file) = &artifacts.file
    {
        step.output = build_relative(model, file);
    }
    let language = step.language.as_slice();
    let wrapper = LinkerDriver::new(model, target, language);
    let policy_new =
        |policy| target.policies.policies.get(PolicyId::new(policy)) == PolicyStatus::New;
    let parse_flags = |flags: BString| {
        if policy_new(181) {
            wrapper.flag_string(&flags)
        } else {
            Ok(wrapper.driver_flags(&flags))
        }
    };
    let darwin = is_darwin(model, target.dir);
    let msvc = is_msvc(model, target.dir, language);
    let kind = step.kind;
    let policy = Flags::new(model, target, language);
    let ipo = if language == b"CUDA" {
        super::cuda::arch::ipo_enabled(model, target, config, language)
    } else {
        policy.ipo(config)
    };
    let ninja_posix = model
        .generator
        .is_some_and(rcmake_model::GeneratorKind::is_ninja)
        && !super::shell::windows_host(model, target.dir);
    let mut driver = language_flags(model, target, config, language);
    let linker_mode = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_LINK_MODE"].concat(),
    )
    .is_some_and(|mode| mode == b"LINKER");
    if kind != LinkKind::StaticArchive && !msvc && !linker_mode {
        driver.extend(policy.ipo_compile(ipo));
    }
    let arch = if kind == LinkKind::StaticArchive && language != b"Swift" {
        Vec::new()
    } else {
        super::apple::arch::flags(model, target, config, language, None)?
    };
    let language_compile_flags = (ninja_posix && kind != LinkKind::Executable).then(|| {
        let mut lang_flags = driver.clone();
        if language == b"CUDA" {
            lang_flags.extend(
                super::cuda::link_flags(model, target, config)
                    .iter()
                    .map(|flag| quote(flag)),
            );
        } else if language == b"HIP" {
            lang_flags.extend(
                super::hip::link_flags(model, target, config)
                    .iter()
                    .map(|flag| quote(flag)),
            );
        }
        [
            join_link_flags(lang_flags).trim_ascii(),
            b" ",
            join_words(arch.iter().cloned()).as_slice(),
        ]
        .concat()
        .into()
    });
    let split_swift = super::swift::is_split_build(model, target);
    let one_step_swift = language == b"Swift" && super::compile::is_one_step_swift(model, target);
    let swift_group = one_step_swift
        .then(|| {
            resolved
                .compile_groups
                .iter()
                .find(|group| group.language == b"Swift")
        })
        .flatten();
    let driver = if let Some(group) = swift_group {
        let required_standard = target_inputs.and_then(|inputs| {
            inputs
                .standard_requirements
                .get(b"Swift".as_slice())
                .copied()
        });
        join_words(super::compile::swift_compile_flags(
            model,
            target,
            config,
            group,
            required_standard,
            position_independent,
            usage,
        )?)
    } else {
        if language == b"CUDA" {
            driver.extend(
                super::cuda::link_flags(model, target, config)
                    .iter()
                    .map(|flag| quote(flag)),
            );
        } else if language == b"HIP" {
            driver.extend(
                super::hip::link_flags(model, target, config)
                    .iter()
                    .map(|flag| quote(flag)),
            );
        }
        driver.extend(arch);
        join_words(driver)
    };
    let driver = if ninja_posix {
        BString::from(driver.trim_ascii())
    } else {
        driver
    };
    let option = |key: &[u8]| policy.option(key);
    let cmake = || {
        definition(model, target.dir, b"CMAKE_COMMAND").map_or_else(|| "cmake".into(), Clone::clone)
    };
    let mut flags = Vec::new();
    let language_flags = [b"CMAKE_", language, b"_LINK_FLAGS"].concat();
    let create = creation_flags(model, target, step.kind, language)
        .map_or_else(|| kind.default_create_flag(darwin).into(), Clone::clone);
    flags.push(parse_flags(create)?);
    let include_linker_flags = match kind {
        LinkKind::Executable => language != b"Swift" || (split_swift && policy_new(214)),
        LinkKind::SharedLibrary | LinkKind::ModuleLibrary => language != b"Swift" || split_swift,
        LinkKind::StaticArchive => language != b"Swift",
    };
    if include_linker_flags {
        let base = if kind == LinkKind::SharedLibrary
            && darwin
            && target
                .props
                .get(b"FRAMEWORK")
                .is_some_and(|value| truth::is_on(value))
            && model.generator == Some(rcmake_model::GeneratorKind::UnixMakefiles)
        {
            b"CMAKE_MACOSX_FRAMEWORK_LINKER_FLAGS".to_vec()
        } else {
            [b"CMAKE_", kind.flag_class(), b"_LINKER_FLAGS"].concat()
        };
        let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
        for key in [&base, &configured] {
            if let Some(value) = definition(model, target.dir, key) {
                flags.push(parse_flags(value.clone())?);
            }
        }
    }
    if kind != LinkKind::StaticArchive && policy_new(210) {
        let configured = [
            language_flags.as_slice(),
            b"_",
            &config.0.to_ascii_uppercase(),
        ]
        .concat();
        for key in [&language_flags, &configured] {
            if let Some(value) = definition(model, target.dir, key) {
                flags.push(wrapper.flag_string(value)?);
            }
        }
    }
    if kind == LinkKind::Executable {
        flags.push(executable_extra_flags(
            model,
            Some(resolved.id),
            target,
            config,
            language,
            &wrapper,
            policy_new(181),
            Some(usage),
        )?);
    } else {
        let raw_flags = kind.target_flags_property();
        flags.extend(
            target
                .props
                .get(raw_flags)
                .map(|flags| wrapper.driver_flags(flags)),
        );
        flags.extend(
            target
                .props
                .get(&[raw_flags, b"_", &config.0.to_ascii_uppercase()].concat())
                .map(|flags| wrapper.driver_flags(flags)),
        );
    }
    for fragment in step
        .fragments
        .iter()
        .filter(|f| f.role == LinkFragmentRole::Flags)
    {
        let value = &fragment.text.value;
        flags.push(if value.starts_with(b"LINKER:") {
            wrapper.argument(value)?
        } else {
            value.clone()
        });
    }
    // CMake adds the linker type flags after the link options.
    let linker = linker_type::select(model, target, config, language, kind, usage)?;
    for flag in linker.flags {
        flags.push(wrapper.argument(&flag)?);
    }
    if kind != LinkKind::StaticArchive {
        let modes = crate::usage::link_options::warning_as_error_options(model, target)
            .map_err(ResolveError::Message)?;
        for mode in modes {
            let prefix = if mode == "DRIVER" {
                let key = [b"CMAKE_", language, b"_LINK_MODE"].concat();
                if definition(model, target.dir, &key).is_none_or(|mode| mode != "DRIVER") {
                    continue;
                }
                b"_COMPILE".as_slice()
            } else {
                b"_LINK"
            };
            let key = [b"CMAKE_", language, prefix, b"_OPTIONS_WARNING_AS_ERROR"].concat();
            for option in definition(model, target.dir, &key)
                .into_iter()
                .flat_map(|value| list::split(value, list::Empty::Drop))
            {
                flags.push(if mode == "DRIVER" {
                    quote(&option)
                } else {
                    wrapper.argument(&option)?
                });
            }
        }
        flags.extend(policy.pie_link(position_independent));
        flags.extend(policy.link(ipo));
    }
    let depfile = ninja_link_depfile(model, target, config, kind, language);
    if let Some((path, depfile_flags)) = &depfile {
        flags.extend(wrapper.depfile_flags(depfile_flags, path)?);
    }
    let lwyu = super::policy_flags::link_what_you_use(model, target, language);
    if let Some(flag) = lwyu {
        flags.push(wrapper.argument(flag)?);
    }
    if kind == LinkKind::SharedLibrary && darwin {
        flags.extend(darwin_versions(model, target, language));
    }
    if darwin {
        deduplicate_darwin_flags(&mut flags);
    }
    let mut inputs = objects.clone();
    let mut implicit_inputs = Vec::new();
    let mut before_link = Vec::new();
    if kind != LinkKind::StaticArchive {
        if super::artifacts::is_dll_platform(model, target.dir) {
            let definitions =
                definition_file(model, target, config, usage, sources, &objects, language)?;
            for path in definitions.inputs {
                if !implicit_inputs.contains(&path) {
                    implicit_inputs.push(path);
                }
            }
            flags.extend(definitions.flag);
            before_link.extend(definitions.invocation);
        } else {
            for path in target_definition_files(model, target, sources, false) {
                if !implicit_inputs.contains(&path) {
                    implicit_inputs.push(path);
                }
            }
        }
    }
    let flags = join_link_flags(flags);
    // Ninja trims each variable binding (`cmGlobalNinjaGenerator::WriteBuild`).
    let flags = if model.generator.is_some_and(GeneratorKind::is_ninja) {
        BString::from(flags.trim_ascii())
    } else {
        flags
    };
    let source_paths = || {
        sources.map_or_else(
            || {
                target
                    .sources
                    .iter()
                    .map(|source| join(&model.dir(target.dir).source_dir, &source.value))
                    .collect()
            },
            |sources| sources.iter().cloned().collect::<Vec<_>>(),
        )
    };
    let manifests = source_paths()
        .into_iter()
        .filter(|path| {
            path.last_chunk::<9>()
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".manifest"))
        })
        .map(|path| build_relative(model, &path))
        .collect::<Vec<_>>();
    inputs.extend(manifests.iter().cloned());
    let manifests = join_words(manifests.iter().map(|path| quote(path)));
    let runtime =
        super::cuda::runtime::host_args(model, target, config, &step, link_languages, usage)?;
    let (libraries, runtime_paths) = link_libraries(
        model,
        target,
        config,
        &step,
        darwin,
        &mut inputs,
        &wrapper,
        runtime,
        link_languages,
        usage,
        ninja_posix,
    )?;
    let mut explicit_link_depends = Vec::new();
    if kind != LinkKind::StaticArchive {
        for path in frameworks::imported_framework_files(model, target, config, &step, usage) {
            if !inputs.contains(&path) && !implicit_inputs.contains(&path) {
                implicit_inputs.push(path);
            }
        }
        if let Some(id) = model.find_target(target.dir, &target.name) {
            let context = EvalContext {
                config: config.clone(),
                link_language: Some(step.language.clone()),
                ..EvalContext::default()
            };
            for entry in usage
                .collect_property_with(
                    id,
                    b"LINK_DEPENDS",
                    &context,
                    Some(link_property_traversal(target)),
                )?
                .entries
            {
                let path = join(&model.dir(target.dir).source_dir, &entry.value);
                let path = build_relative(model, &path);
                if !implicit_inputs.contains(&path) {
                    implicit_inputs.push(path.clone());
                }
                explicit_link_depends.push(path);
            }
        }
    }
    let soname = if kind == LinkKind::SharedLibrary {
        artifacts.soname_file.as_ref()
    } else {
        None
    };
    let soname = soname.map(|name| Lexical::new(name).filename());
    let soname_flag = soname.map_or_else(BString::default, |_| {
        definition(
            model,
            target.dir,
            &[b"CMAKE_SHARED_LIBRARY_SONAME_", language, b"_FLAG"].concat(),
        )
        .or_else(|| definition(model, target.dir, b"CMAKE_SHARED_LIBRARY_SONAME_C_FLAG"))
        .cloned()
        .unwrap_or_else(|| {
            if darwin {
                "-install_name".into()
            } else {
                "-Wl,-soname,".into()
            }
        })
    });
    let has_install_name = definition(model, target.dir, b"CMAKE_PLATFORM_HAS_INSTALLNAME")
        .map_or(darwin, |value| truth::is_on(value));
    let install_name_dir = if has_install_name && soname.is_some() {
        install_name_prefix(model, target, artifacts, config, None)?
    } else {
        BString::default()
    };
    let shared_flags_key = [b"CMAKE_SHARED_LIBRARY_", language, b"_FLAGS"].concat();
    let ninja_posix = model.generator.is_some_and(GeneratorKind::is_ninja)
        && !super::shell::windows_host(model, target.dir);
    let quote_path = |path: &[u8]| {
        if ninja_posix {
            super::shell::cmake_posix_quote(path)
        } else {
            quote(path)
        }
    };
    let output = quote_path(&step.output);
    let output_ext = Lexical::new(&step.output).stem_extension(true).1;
    let output_base = quote_path(step.output.strip_suffix(output_ext).unwrap_or(&step.output));
    let object_dir = super::object_dir(model, target, config);
    let swift_defines = swift_group
        .map(|group| super::compile::formatted_defines(model, target, config, group))
        .unwrap_or_default();
    let swift_includes = swift_group
        .map(|group| super::compile::formatted_includes(model, target, group, framework_dirs))
        .unwrap_or_default();
    let mut extra_outputs = Vec::new();
    let mut swift_output_file_map = None;
    let mut driver = driver;
    let swift_sources = one_step_swift.then(|| {
        let source_dir = &model.dir(target.dir).source_dir;
        let mut swift_unit_objects = IndexSet::new();
        let mut emitted_objects = IndexSet::new();
        let mut entries = Vec::new();
        let mut map_entries = std::collections::BTreeMap::new();
        for unit in &resolved.compile_units {
            let is_swift = resolved
                .compile_groups
                .get(unit.group)
                .is_some_and(|group| group.language == b"Swift");
            if is_swift {
                let source_path = join(source_dir, &unit.source.value);
                swift_unit_objects.insert(unit.object.as_slice());
                entries.push(source_path.clone());
                if one_step_swift {
                    let input_path = build_relative(model, &source_path);
                    if !inputs.contains(&input_path) {
                        inputs.push(input_path);
                    }
                    if !unit.object.is_empty() && !extra_outputs.contains(&unit.object) {
                        extra_outputs.push(unit.object.clone());
                    }
                    if !unit.object.is_empty() {
                        let (stem, _) = Lexical::new(&source_path).stem_extension(true);
                        let partial_module = join(
                            &object_dir,
                            &[stem, b".swiftmodule~partial.swiftmodule"].concat(),
                        );
                        let mut per_source = std::collections::BTreeMap::new();
                        per_source.insert(
                            "dependencies".into(),
                            rcmake_json::Value::string([unit.object.as_slice(), b".d"].concat()),
                        );
                        per_source.insert(
                            "object".into(),
                            rcmake_json::Value::string(unit.object.clone()),
                        );
                        per_source.insert(
                            "swiftmodule".into(),
                            rcmake_json::Value::string(partial_module),
                        );
                        per_source.insert(
                            "swift-dependencies".into(),
                            rcmake_json::Value::string(
                                [unit.object.as_slice(), b".swiftdeps"].concat(),
                            ),
                        );
                        map_entries.insert(source_path, rcmake_json::Value::Object(per_source));
                    }
                }
            } else if emitted_objects.insert(unit.object.as_slice()) {
                entries.push(unit.object.clone());
            }
        }
        for object in &objects {
            if !swift_unit_objects.contains(object.as_slice())
                && emitted_objects.insert(object.as_slice())
            {
                entries.push(object.clone());
            }
        }
        if one_step_swift && !map_entries.is_empty() {
            let swift_deps = target
                .props
                .get(b"Swift_DEPENDENCIES_FILE")
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    join(
                        &object_dir,
                        &[target.name.as_slice(), b".swiftdeps"].concat(),
                    )
                });
            let mut global = std::collections::BTreeMap::new();
            global.insert(
                "swift-dependencies".into(),
                rcmake_json::Value::string(swift_deps),
            );
            let mut root = std::collections::BTreeMap::new();
            root.insert(BString::default(), rcmake_json::Value::Object(global));
            root.extend(map_entries);
            let map_path = join(&object_dir, b"output-file-map.json");
            let json = rcmake_json::write_styled(&rcmake_json::Value::Object(root), "  ");
            driver = join_words([driver.clone(), "-output-file-map".into(), quote(&map_path)]);
            swift_output_file_map = Some((map_path, json));
        }
        join_words(entries.iter().map(|path| quote(path)))
    });
    let swift_library_name = if one_step_swift {
        Some(super::swift::library_base_name(
            model, target, config, resolved, usage,
        )?)
    } else {
        None
    };
    let swift_module_name = one_step_swift.then(|| super::swift::module_name(target));
    let swift_module = if one_step_swift {
        Some(super::swift::module_path(
            model,
            target,
            config,
            swift_module_name
                .as_ref()
                .map_or(target.name.as_slice(), |value| value.as_slice()),
            usage,
        )?)
    } else {
        None
    };
    let with_exports = kind == LinkKind::Executable
        && target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|value| truth::is_on(value));
    let template = if with_exports {
        option(b"LINK_EXECUTABLE_WITH_EXPORTS")
            .filter(|v| !v.is_empty())
            .or_else(|| option(kind.rule_variable()).filter(|v| !v.is_empty()))
    } else {
        option(kind.rule_variable()).filter(|v| !v.is_empty())
    };
    if one_step_swift
        && let Some(module) = &swift_module
        && template.is_some_and(|tpl| {
            tpl.windows(b"<SWIFT_MODULE>".len())
                .any(|window| window == b"<SWIFT_MODULE>")
        })
        && !extra_outputs.contains(module)
    {
        extra_outputs.push(module.clone());
    }
    let lookup = |key: &[u8]| match key {
        b"CMAKE_LINKER" => linker.executable.as_ref().map(|path| quote(path)),
        b"TARGET" => Some(output.clone()),
        b"TARGET_BASE" => Some(output_base.clone()),
        b"OBJECT_DIR" | b"TARGET_SUPPORT_DIR" => Some(quote(&object_dir)),
        b"DEFINES" if language == b"Swift" => Some(swift_defines.clone()),
        b"INCLUDES" if language == b"Swift" => Some(swift_includes.clone()),
        b"SWIFT_SOURCES" => swift_sources.clone(),
        b"SWIFT_MODULE_NAME" => swift_module_name.as_ref().map(|v| quote(v)),
        b"SWIFT_LIBRARY_NAME" => swift_library_name.as_ref().map(|v| quote(v)),
        b"SWIFT_MODULE" => swift_module.as_ref().map(|v| quote(v)),
        b"MANIFESTS" => Some(manifests.clone()),
        b"TARGET_SONAME" => Some(soname.map_or_else(BString::default, quote)),
        b"SONAME_FLAG" => Some(soname_flag.clone()),
        b"TARGET_INSTALLNAME_DIR" => Some(if install_name_dir.is_empty() {
            BString::default()
        } else {
            quote(&install_name_dir)
        }),
        b"TARGET_PDB" | b"TARGET_IMPLIB" => Some({
            let file = if key == b"TARGET_PDB" {
                &artifacts.pdb_file
            } else {
                &artifacts.import_file
            };
            file.as_ref()
                .map_or_else(BString::default, |path| quote(path))
        }),
        b"TARGET_VERSION_MAJOR" | b"TARGET_VERSION_MINOR" => Some(
            target
                .props
                .get(b"VERSION")
                .and_then(|v| {
                    v.split(|&b| b == b'.')
                        .nth(usize::from(key == b"TARGET_VERSION_MINOR"))
                })
                .unwrap_or(b"0")
                .into(),
        ),
        b"FLAGS" => Some(driver.clone()),
        b"LANGUAGE_COMPILE_FLAGS" => {
            Some(language_compile_flags.as_ref().unwrap_or(&driver).clone())
        }
        b"LINK_FLAGS" => Some(flags.clone()),
        b"LINK_LIBRARIES" => Some(libraries.clone()),
        key if key == shared_flags_key => Some(
            definition(model, target.dir, key)
                .cloned()
                .unwrap_or_default(),
        ),
        _ => None,
    };
    let expand = |template: &[u8], objects: &[BString]| {
        let objects = join_words(objects.iter().map(|v| quote(v)));
        let template = without_empty_output_flags(template, artifacts);
        let lookup = |key: &[u8]| {
            if key == b"OBJECTS" {
                Some(objects.clone())
            } else {
                lookup(key)
            }
        };
        response::expand(model, target, &step, msvc, &template, lookup)
    };

    let compiler = [b"<CMAKE_", language, b"_COMPILER>"].concat();
    let soname_template: &[u8] = if darwin {
        b" <SONAME_FLAG> <TARGET_INSTALLNAME_DIR><TARGET_SONAME>"
    } else {
        b" <SONAME_FLAG><TARGET_SONAME>"
    };
    let fallback = [
        compiler.as_slice(),
        b" <FLAGS> <LINK_FLAGS>",
        if kind == LinkKind::SharedLibrary {
            soname_template
        } else {
            b""
        },
        b" <OBJECTS> -o <TARGET> <LINK_LIBRARIES>",
    ]
    .concat();

    let mut invocations = if kind == LinkKind::StaticArchive {
        archive_commands(
            &step.output,
            &objects,
            cmake(),
            &policy,
            (
                ipo,
                msvc,
                model.generator.is_some_and(GeneratorKind::is_ninja),
            ),
            template,
            expand,
        )
    } else {
        expand(
            template.map_or(fallback.as_slice(), |v| v.as_slice()),
            &objects,
        )
    };
    if kind == LinkKind::StaticArchive
        && darwin
        && model
            .generator
            .is_some_and(rcmake_model::GeneratorKind::is_ninja)
    {
        invocations.push(direct(
            vec![cmake(), "-E".into(), "touch".into(), step.output.clone()],
            None,
        ));
    }
    let rule_launcher = target
        .props
        .get(b"RULE_LAUNCH_LINK")
        .or_else(|| {
            model
                .ancestors(target.dir)
                .find_map(|dir| model.dir(dir).props.get(b"RULE_LAUNCH_LINK"))
        })
        .or_else(|| model.global.props.get(b"RULE_LAUNCH_LINK"))
        .map(|value| {
            if !value.windows(2).any(|part| part == b"$<") {
                return Ok(value.clone());
            }
            let handle = model
                .find_target(target.dir, &target.name)
                .map(crate::usage::handle);
            let ctx = EvalContext {
                config: config.clone(),
                current: handle,
                head: handle,
                link_language: Some(step.language.clone()),
                traversal: UsageTraversal::Link,
                ..EvalContext::default()
            };
            let ast = usage.parse_genex(value)?;
            evaluate(&ast, &ctx, &mut usage.host(target.dir))
                .map(|(text, _)| text)
                .map_err(ResolveError::from)
        })
        .transpose()?
        .filter(|value| !value.is_empty())
        .map(|prefix| {
            super::expand_rule(&prefix, |key| match key {
                b"LANGUAGE" => Some(step.language.clone()),
                b"TARGET_NAME" => Some(quote(&target.name)),
                b"TARGET_TYPE" => Some(target.kind.as_str().into()),
                b"CMAKE_CURRENT_BINARY_DIR" => Some(quote(&model.dir(target.dir).binary_dir)),
                b"CMAKE_BINARY_DIR" => Some(quote(&model.dir(DirId::ROOT).binary_dir)),
                b"TARGET_SUPPORT_DIR" => Some(quote(&super::object_dir(model, target, config))),
                _ => lookup(key),
            })
        });
    if let Some(program) = rule_launcher
        .as_ref()
        .and_then(|prefix| simple_args(prefix)?.into_iter().next())
    {
        for dep in &target.utility_deps {
            if let Some(dependency) = model
                .find_target(target.dir, &dep.value)
                .and_then(|id| model.target(id))
                && let Some(file) = super::compute_artifacts(model, dependency, config).file
                && (program == file || program == build_relative(model, &file))
            {
                inputs.push(build_relative(model, &file));
            }
        }
    }
    for invocation in invocations
        .iter_mut()
        .skip(usize::from(kind == LinkKind::StaticArchive && !msvc))
    {
        let command = launcher::Command {
            role: "link",
            config,
            output: &step.output,
            source: None,
            language: &step.language,
        };
        let mut link = std::mem::take(invocation);
        let annotated = link
            .link_response
            .take()
            .map(|response| (link.argv.clone(), response));
        if kind != LinkKind::StaticArchive {
            link = command.launch_tool(model, target, b"_LINKER_LAUNCHER", link, usage)?;
        }
        if let Some(prefix) = &rule_launcher {
            link = shell_command(join_words([prefix.clone(), link.shell]));
        }
        *invocation = command.wrap(model, target, link);
        if let Some((original, response)) = annotated {
            response::rebase(invocation, &original, response);
        }
    }
    if lwyu.is_some()
        && let Some(check) = definition(model, target.dir, b"CMAKE_LINK_WHAT_YOU_USE_CHECK")
            .filter(|check| !check.is_empty())
    {
        let program = cmake();
        let mut invocation = direct(
            vec![
                program.clone(),
                "-E".into(),
                "__run_co_compile".into(),
                [b"--lwyu=".as_slice(), check].concat().into(),
                [b"--source=".as_slice(), &step.output].concat().into(),
            ],
            None,
        );
        if !super::shell::windows_host(model, target.dir) {
            invocation.shell = [
                super::shell::cmake_posix_quote(&program).as_slice(),
                b" -E __run_co_compile --lwyu=",
                super::shell::cmake_posix_quote(check).as_slice(),
                b" --source=",
                super::shell::cmake_posix_quote(&step.output).as_slice(),
            ]
            .concat()
            .into();
        }
        invocations.push(invocation);
    }
    if kind == LinkKind::SharedLibrary
        && let (Some(file), Some(soname), Some(linker)) = (
            &artifacts.file,
            &artifacts.soname_file,
            &artifacts.linker_file,
        )
        && (file != soname || soname != linker)
    {
        invocations.push(direct(
            vec![
                cmake(),
                "-E".into(),
                "cmake_symlink_library".into(),
                step.output.clone(),
                build_relative(model, soname),
                build_relative(model, linker),
            ],
            None,
        ));
    }
    if kind == LinkKind::Executable
        && let (Some(file), Some(linker)) = (&artifacts.file, &artifacts.linker_file)
        && file != linker
        && artifacts.import_file.as_ref() != Some(linker)
    {
        invocations.push(direct(
            vec![
                cmake(),
                "-E".into(),
                "cmake_symlink_executable".into(),
                step.output.clone(),
                build_relative(model, linker),
            ],
            None,
        ));
    }
    before_link.append(&mut invocations);
    implicit_inputs.retain(|path| explicit_link_depends.contains(path) || !inputs.contains(path));
    Ok(LoweredLink {
        step: BuildStep::Link {
            implicit_inputs,
            inputs,
            step,
            invocations: before_link,
        },
        install_name_dir,
        depfile: depfile.map(|(path, _)| path),
        runtime_paths,
        extra_outputs,
        swift_output_file_map,
    })
}

fn ninja_link_depfile<'a>(
    model: &'a ProjectModel,
    target: &Target,
    config: &ConfigId,
    kind: LinkKind,
    language: &[u8],
) -> Option<(BString, &'a BString)> {
    let language_setting =
        |suffix: &[u8]| definition(model, target.dir, &[b"CMAKE_", language, suffix].concat());
    if !model
        .generator
        .is_some_and(rcmake_model::GeneratorKind::is_ninja)
        || kind == LinkKind::StaticArchive
        || target
            .props
            .get(b"LINK_DEPENDS_NO_SHARED")
            .is_some_and(|value| truth::is_on(value))
        || definition(model, target.dir, b"CMAKE_LINK_DEPENDS_USE_LINKER")
            .is_some_and(|value| !truth::is_on(value))
        || !language_setting(b"_LINK_DEPENDS_USE_LINKER").is_some_and(|value| truth::is_on(value))
        || !language_setting(b"_LINKER_DEPFILE_FORMAT").is_some_and(|value| value == b"gcc")
    {
        return None;
    }
    let flags = language_setting(b"_LINKER_DEPFILE_FLAGS").filter(|value| !value.is_empty())?;
    Some((
        join(&super::object_dir(model, target, config), b"link.d"),
        flags,
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Runtime libraries follow explicit link fragments."
)]
fn link_libraries(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    darwin: bool,
    inputs: &mut Vec<BString>,
    wrapper: &LinkerDriver<'_>,
    runtime_args: Vec<BString>,
    link_languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
    ninja_posix: bool,
) -> Result<(BString, Option<crate::LinkRuntimePaths>), ResolveError> {
    if step.kind == LinkKind::StaticArchive {
        return Ok((BString::default(), None));
    }
    let link_fragments = step
        .fragments
        .iter()
        .filter(|f| f.role != LinkFragmentRole::Flags);
    let windows = super::shell::windows_host(model, target.dir);
    let mut libraries = linked_libraries(step, windows);
    let feature_files =
        feature_wrapped_library_files(model, target, config, step, usage, &libraries, windows);
    libraries.extend(feature_files);
    let named = frameworks::named_framework_files(model, target, config, step, &libraries);
    libraries.extend(named);
    let runtime = rpath::runtime_libraries(model, target, config, step, &libraries, usage)?;
    let runtime_flags =
        rpath::link_flags(model, target, config, step, usage, &runtime, link_languages)?;
    let mut rpaths = runtime_flags.rpath;
    if ninja_posix && rpaths.len() > 1 {
        rpaths = vec![join_words(rpaths)];
    }
    let runtime_paths = crate::LinkRuntimePaths {
        rpath: (!rpaths.is_empty()).then(|| join_words(rpaths.iter().cloned())),
        rpath_link: runtime_flags.rpath_link.clone(),
    };
    inputs.extend(libraries);
    inputs.extend(runtime.link_stubs.iter().cloned());
    let (mut arguments, mut rpaths) = if darwin {
        (Vec::new(), Some(rpaths))
    } else {
        (rpaths, None)
    };
    let mut framework_paths = Vec::new();
    let mut library_paths = Vec::new();
    for fragment in link_fragments {
        if ninja_posix {
            let paths = match fragment.role {
                LinkFragmentRole::LibraryPath => Some(&mut library_paths),
                LinkFragmentRole::FrameworkPath => Some(&mut framework_paths),
                _ => None,
            };
            if let Some(paths) = paths {
                paths.push(wrapper.argument(&fragment.text.value)?);
                continue;
            }
        }
        if darwin && fragment.role == LinkFragmentRole::Libraries {
            arguments.extend(rpaths.take().into_iter().flatten());
        }
        arguments.push(wrapper.argument(&fragment.text.value)?);
    }
    arguments.extend(rpaths.into_iter().flatten());
    arguments.extend(runtime.link_stubs.iter().map(|file| quote(file)));
    arguments.extend(runtime_args.iter().map(|arg| quote(arg)));
    arguments.extend(runtime_flags.rpath_link);
    if let Some(standard) = definition(
        model,
        target.dir,
        &[b"CMAKE_", step.language.as_slice(), b"_STANDARD_LIBRARIES"].concat(),
    ) {
        arguments.push(wrapper.driver_flags(standard));
    }
    let runtime_paths = Some(runtime_paths);
    if !ninja_posix {
        return Ok((join_words(arguments), runtime_paths));
    }
    let frameworks = join_words(framework_paths);
    let directories = library_paths.join(b"   ".as_slice());
    let libraries = arguments
        .iter()
        .filter(|arg| !arg.is_empty())
        .map(|argument| argument.as_slice())
        .collect::<Vec<_>>()
        .join(b"  ".as_slice());
    let mut line = frameworks;
    if !line.is_empty() && !directories.is_empty() {
        line.extend_from_slice(b"  ");
    }
    line.extend_from_slice(&directories);
    line.push(b' ');
    line.extend_from_slice(&libraries);
    Ok((line, runtime_paths))
}

fn darwin_versions(model: &ProjectModel, target: &Target, language: &[u8]) -> Vec<BString> {
    let version = target.props.get(b"VERSION").filter(|v| !v.is_empty());
    let soversion = target.props.get(b"SOVERSION").filter(|v| !v.is_empty());
    let current = target
        .props
        .get(b"MACHO_CURRENT_VERSION")
        .or(version)
        .or(soversion);
    let compatibility = target
        .props
        .get(b"MACHO_COMPATIBILITY_VERSION")
        .or(soversion)
        .or(version);
    let mut flags = Vec::new();
    for (value, key, fallback) in [
        (
            compatibility,
            b"OSX_COMPATIBILITY_VERSION_FLAG".as_slice(),
            b"-compatibility_version ".as_slice(),
        ),
        (current, b"OSX_CURRENT_VERSION_FLAG", b"-current_version "),
    ] {
        if let Some(value) = value.and_then(|v| macho_version(v)) {
            let flag = Flags::new(model, target, language)
                .option(key)
                .map_or(fallback, |v| v.as_slice());
            if !flag.is_empty() {
                flags.push([flag, quote(&value).as_slice()].concat().into());
            }
        }
    }
    flags
}

fn macho_version(value: &[u8]) -> Option<BString> {
    let mut version = value.split(|&byte| byte == b'.');
    let mut component = || {
        version
            .next()
            .and_then(|part| std::str::from_utf8(part).ok())
            .and_then(|part| part.parse::<u32>().ok())
            .unwrap_or_default()
    };
    let (major, minor, patch) = (component(), component(), component());
    (major != 0 || minor != 0 || patch != 0).then(|| format!("{major}.{minor}.{patch}").into())
}

fn takes_separate_non_file_arg(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"-F"
            | b"-L"
            | b"-l"
            | b"-framework"
            | b"-weak_framework"
            | b"-needed_framework"
            | b"-reexport_framework"
            | b"-lazy_framework"
            | b"-rpath"
            | b"-rpath-link"
            | b"-install_name"
            | b"-syslibroot"
            | b"-isysroot"
            | b"-soname"
            | b"--soname"
            | b"-h"
            | b"-u"
            | b"-e"
            | b"-init"
            | b"-z"
            | b"-T"
            | b"--script"
            | b"-version-script"
            | b"--version-script"
            | b"--dynamic-list"
            | b"-Map"
            | b"-force_load"
            | b"-weak_library"
            | b"-reexport_library"
            | b"-upward_library"
            | b"-needed_library"
            | b"-lazy_library"
            | b"-load_hidden"
            | b"-compatibility_version"
            | b"-current_version"
            | b"-undefined"
            | b"-umbrella"
            | b"-allowable_client"
            | b"-exported_symbols_list"
            | b"-unexported_symbols_list"
            | b"-order_file"
            | b"-alias_list"
            | b"-bundle_loader"
            | b"-segaddr"
            | b"-pagezero_size"
            | b"-stack_size"
    )
}

fn feature_wrapped_library_files(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    usage: &mut UsageResolver<'_>,
    existing: &[BString],
    windows: bool,
) -> Vec<BString> {
    let has_wrapped_feature = step.fragments.iter().any(|fragment| {
        fragment.role == LinkFragmentRole::Libraries
            && (fragment.text.value.starts_with(b"-Wl,")
                || fragment.text.value.starts_with(b"LINKER:")
                || fragment.text.value.windows(14).any(|window| {
                    window.eq_ignore_ascii_case(b"/wholearchive:")
                        || window.eq_ignore_ascii_case(b"-wholearchive:")
                })
                || link_fragment_args(&fragment.text.value, windows).is_some_and(|args| {
                    args.iter().any(|arg| {
                        arg.starts_with(b"-Wl,")
                            || takes_separate_non_file_arg(arg)
                            || arg.get(..14).is_some_and(|prefix| {
                                prefix.eq_ignore_ascii_case(b"-wholearchive:")
                                    || prefix.eq_ignore_ascii_case(b"/wholearchive:")
                            })
                    })
                }))
    });
    if !has_wrapped_feature {
        return Vec::new();
    }
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Vec::new();
    };
    let ctx = EvalContext {
        config: config.clone(),
        link_language: Some(step.language.clone()),
        ..EvalContext::default()
    };
    let Ok(items) = usage.ordered_links(id, &ctx) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut collect = |item: &crate::usage::ResolvedLinkItem| {
        if item
            .feature
            .as_deref()
            .is_none_or(|feature| feature == b"DEFAULT")
        {
            return;
        }
        let path = match &item.kind {
            crate::usage::ResolvedLinkKind::Raw if library_file(&item.text.value) => {
                Some(build_relative(model, &item.text.value))
            }
            crate::usage::ResolvedLinkKind::Library(dep) => {
                let producer = model.target(*dep);
                let file = if producer.is_some_and(|producer| {
                    !producer.is_imported()
                        && is_darwin(model, producer.dir)
                        && matches!(
                            crate::ide::bundle_kind(producer),
                            crate::ide::BundleKind::Framework { .. }
                        )
                }) {
                    usage.artifact(
                        *dep,
                        rcmake_genex::ArtifactKind::File,
                        rcmake_genex::ArtifactPart::Full,
                        config,
                    )
                } else {
                    usage
                        .ide_artifacts(*dep, config)
                        .ok()
                        .and_then(|(artifacts, _)| artifacts.linker_file.or(artifacts.file))
                };
                file.map(|file| build_relative(model, &file))
            }
            _ => None,
        };
        if let Some(path) = path
            && !existing.contains(&path)
            && !files.contains(&path)
        {
            files.push(path);
        }
    };
    for item in &items {
        if let crate::usage::ResolvedLinkKind::Group { members, .. } = &item.kind {
            for member in members {
                collect(member);
            }
        } else {
            collect(item.as_ref());
        }
    }
    files
}

fn link_fragment_args(text: &[u8], windows: bool) -> Option<Vec<BString>> {
    simple_args(text).or_else(|| windows.then(|| super::shell::windows_arguments(text)))
}

fn linked_libraries(step: &LinkStep, windows: bool) -> Vec<BString> {
    let mut libraries = Vec::new();
    for fragment in step
        .fragments
        .iter()
        .filter(|f| f.role == LinkFragmentRole::Libraries)
    {
        let Some(args) = link_fragment_args(&fragment.text.value, windows) else {
            continue;
        };
        // A raw multi-word `-Wl,...` flag item passed via `target_link_libraries`
        // (for example `"-Wl,--whole-archive /path/libfoo.a"`) is a linker flag,
        // not a Ninja file dependency (`cmComputeLinkInformation::AddItem`).
        if args.len() > 1 && args.first().is_some_and(|arg| arg.starts_with(b"-Wl,")) {
            continue;
        }
        let mut skip_next = false;
        for arg in args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if takes_separate_non_file_arg(&arg) {
                skip_next = true;
                continue;
            }
            if library_file(&arg) {
                libraries.push(arg);
            }
        }
    }
    libraries
}

#[cfg(test)]
mod tests {
    use rcmake_model::{DirId, ProjectModel, Target, TargetKind, TargetOrigin};

    use super::*;

    #[test]
    fn append_flag_skips_whitespace_only_and_preserves_non_blank_spaces() {
        let mut out = BString::default();
        append_flag(&mut out, b"-Wl,--no-undefined");
        append_flag(&mut out, b"   ");
        append_flag(&mut out, b"-Wl,--dependency-file=foo.d");
        assert_eq!(out, "-Wl,--no-undefined -Wl,--dependency-file=foo.d");

        let mut leading = BString::from("-Wl,--export-dynamic ");
        append_flag(&mut leading, b" -Wl,-rpath-link,/foo");
        assert_eq!(leading, "-Wl,--export-dynamic   -Wl,-rpath-link,/foo");
    }

    #[test]
    fn codemodel_link_flags_matches_executable_extra_flags_and_static_flags() {
        let make_model =
            |shared_build: bool, exports: bool, shared_link: &str, link_flags: &str| {
                let mut model = ProjectModel::default();
                model.set_root_dirs("/src".into(), "/out".into());
                model
                    .global
                    .props
                    .set("TARGET_SUPPORTS_SHARED_LIBS".into(), "TRUE".into());
                let root = model.dir_mut(DirId::ROOT);
                root.vars
                    .insert("CMAKE_EXE_LINKER_FLAGS".into(), "-Wl,--elf".into());
                root.vars.insert(
                    "CMAKE_EXE_EXPORTS_C_FLAG".into(),
                    "-Wl,--export-dynamic".into(),
                );
                root.vars.insert(
                    "CMAKE_SHARED_LIBRARY_LINK_C_FLAGS".into(),
                    shared_link.into(),
                );
                if shared_build {
                    root.vars.insert("BUILD_SHARED_LIBS".into(), "ON".into());
                }
                let mut target = Target::new(
                    "app".into(),
                    TargetKind::Executable,
                    TargetOrigin::Normal,
                    DirId::ROOT,
                    Default::default(),
                    Default::default(),
                );
                if exports {
                    target.props.set("ENABLE_EXPORTS".into(), "ON".into());
                }
                if !link_flags.is_empty() {
                    target.props.set("LINK_FLAGS".into(), link_flags.into());
                }
                (model, target)
            };
        let step = LinkStep {
            kind: LinkKind::Executable,
            language: "C".into(),
            fragments: Vec::new(),
            output: "app".into(),
        };
        let cfg = ConfigId("Release".into());

        // e1: BUILD_SHARED_LIBS=ON, no exports, no LINK_FLAGS -> emits empty "" fragment.
        let (mut m, t) = make_model(true, false, "", "");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, post, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(frags, vec![BString::from("-Wl,--elf"), BString::from("")]);
        assert!(post.is_empty());

        // e2: BUILD_SHARED_LIBS=ON, no exports, LINK_FLAGS="-Wl,--lf" -> trimmed "-Wl,--lf".
        let (mut m, t) = make_model(true, false, "", "-Wl,--lf");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, _, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(
            frags,
            vec![BString::from("-Wl,--elf"), BString::from("-Wl,--lf")]
        );

        // e3: BUILD_SHARED_LIBS=ON, ENABLE_EXPORTS=ON, shared_link="", LINK_FLAGS="-Wl,--lf" -> 3 spaces.
        let (mut m, t) = make_model(true, true, "", "-Wl,--lf");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, _, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(
            frags,
            vec![
                BString::from("-Wl,--elf"),
                BString::from("-Wl,--export-dynamic   -Wl,--lf"),
            ]
        );

        // e4: BUILD_SHARED_LIBS=ON, ENABLE_EXPORTS=ON, shared_link="-rdynamic", LINK_FLAGS="-Wl,--lf".
        let (mut m, t) = make_model(true, true, "-rdynamic", "-Wl,--lf");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, _, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(
            frags,
            vec![
                BString::from("-Wl,--elf"),
                BString::from("-Wl,--export-dynamic  -rdynamic  -Wl,--lf"),
            ]
        );

        // e5: BUILD_SHARED_LIBS=OFF, ENABLE_EXPORTS=ON, shared_link="", LINK_FLAGS="-Wl,--lf".
        let (mut m, t) = make_model(false, true, "", "-Wl,--lf");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, _, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(
            frags,
            vec![
                BString::from("-Wl,--elf"),
                BString::from("-Wl,--export-dynamic  -Wl,--lf"),
            ]
        );

        // e6: BUILD_SHARED_LIBS=OFF, ENABLE_EXPORTS=ON, shared_link="-rdynamic", LINK_FLAGS="-Wl,--lf".
        let (mut m, t) = make_model(false, true, "-rdynamic", "-Wl,--lf");
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (frags, _, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(
            frags,
            vec![
                BString::from("-Wl,--elf"),
                BString::from("-Wl,--export-dynamic -rdynamic  -Wl,--lf"),
            ]
        );

        // e7: CMP0083=NEW + POSITION_INDEPENDENT_CODE=ON emits PIE flags in post_flags.
        let (mut m, mut t) = make_model(false, false, "", "");
        let root = m.dir_mut(DirId::ROOT);
        root.vars
            .insert("CMAKE_C_LINK_PIE_SUPPORTED".into(), "YES".into());
        root.vars
            .insert("CMAKE_C_LINK_OPTIONS_PIE".into(), "-fPIE;-pie".into());
        let _ = t
            .policies
            .policies
            .set(PolicyId::new(83), PolicyStatus::New);
        t.props.set("POSITION_INDEPENDENT_CODE".into(), "ON".into());
        let app_id = m.add_target(t).unwrap();
        let t = m.target(app_id).unwrap();
        let (_, post, _) = codemodel_link_flags(&m, app_id, t, &cfg, &step, None).unwrap();
        assert_eq!(post, vec![BString::from("-fPIE -pie")]);

        // e8: $<LINK_ONLY:pic_lib> must NOT leak INTERFACE_POSITION_INDEPENDENT_CODE
        // into File API PIE flags when the linker omits it.
        let (mut m, mut t) = make_model(false, false, "", "");
        let root = m.dir_mut(DirId::ROOT);
        root.vars
            .insert("CMAKE_C_LINK_PIE_SUPPORTED".into(), "YES".into());
        root.vars
            .insert("CMAKE_C_LINK_OPTIONS_PIE".into(), "-fPIE;-pie".into());
        let _ = t
            .policies
            .policies
            .set(PolicyId::new(83), PolicyStatus::New);
        let mut pic_lib = Target::new(
            "pic_lib".into(),
            TargetKind::InterfaceLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        pic_lib
            .props
            .set("INTERFACE_POSITION_INDEPENDENT_CODE".into(), "ON".into());
        let _ = m.add_target(pic_lib).unwrap();
        t.usage.link_libraries = Some(vec![rcmake_model::LinkItem {
            text: rcmake_model::BT::new("$<LINK_ONLY:pic_lib>".into(), Default::default()),
            lookup_dir: DirId::ROOT,
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: None,
        }]);
        let app_id = m.add_target(t).unwrap();
        let mut usage = UsageResolver::new(&m);
        let t = m.target(app_id).unwrap();
        let (_, post_link_only, _) =
            codemodel_link_flags(&m, app_id, t, &cfg, &step, Some(&mut usage)).unwrap();
        assert!(
            post_link_only.is_empty(),
            "$<LINK_ONLY:...> must not leak PIE flags into codemodel: {post_link_only:?}"
        );

        // Static archive includes STATIC_LIBRARY_FLAGS and STATIC_LIBRARY_FLAGS_<CONFIG>.
        let mut sm = ProjectModel::default();
        sm.set_root_dirs("/src".into(), "/out".into());
        let mut st = Target::new(
            "lib".into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        st.props
            .set("STATIC_LIBRARY_FLAGS".into(), "-Wl,--st".into());
        st.props
            .set("STATIC_LIBRARY_FLAGS_RELEASE".into(), "-Wl,--strel".into());
        let sid = sm.add_target(st).unwrap();
        let st = sm.target(sid).unwrap();
        let sstep = LinkStep {
            kind: LinkKind::StaticArchive,
            language: "C".into(),
            fragments: Vec::new(),
            output: "libfoo.a".into(),
        };
        let (sfrags, spost, _) = codemodel_link_flags(&sm, sid, st, &cfg, &sstep, None).unwrap();
        assert_eq!(sfrags, vec![BString::from("-Wl,--st -Wl,--strel")]);
        assert!(spost.is_empty());
    }
}
