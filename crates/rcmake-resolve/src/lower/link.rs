//! Link and archive rules.

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal, evaluate, parse};
use rcmake_model::{DirId, ProjectModel, Target};

use super::artifacts::{build_relative, is_darwin, is_msvc};
use super::compile::language_flags;
use super::launcher;
use super::policy_flags::Flags;
use super::shell::{command as shell_command, direct, join_words, simple_args};
use super::{definition, join};
use crate::{
    ArtifactSet, BuildStep, LinkFragmentRole, LinkKind, LinkStep, ResolveError,
    usage::UsageResolver,
};

pub(super) mod driver;
mod frameworks;
mod install_name;
mod linker_type;
pub(crate) use install_name::{install_name_dir, install_name_prefix, warn_install_name_policy};
mod response;
pub(super) mod rpath;
use driver::{
    LinkerDriver, archive_commands, deduplicate_darwin_flags, definition_file, library_file,
    shared_library, without_empty_output_flags,
};
pub(crate) use frameworks::bundle_name as framework_bundle_name;
pub(crate) use rpath::ide_rpaths;

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
    sources: Option<&IndexSet<BString>>,
    position_independent: Option<bool>,
    link_languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<(BuildStep, BString), ResolveError> {
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
    let mut driver = language_flags(model, target, config, language);
    if kind != LinkKind::StaticArchive {
        driver.extend(super::apple::arch::flags(
            model, target, config, language, None,
        )?);
    }
    let driver = join_words(driver);
    let option = |key: &[u8]| policy.option(key);
    let cmake = || {
        definition(model, target.dir, b"CMAKE_COMMAND").map_or_else(|| "cmake".into(), Clone::clone)
    };
    let mut flags = Vec::new();
    let language_flags = [b"CMAKE_", language, b"_LINK_FLAGS"].concat();
    let key = if kind == LinkKind::Executable && !policy_new(210) {
        BString::from(language_flags.clone())
    } else {
        kind.create_flags_key(language)
    };
    flags.push(parse_flags(
        definition(model, target.dir, &key)
            .or_else(|| {
                (kind == LinkKind::ModuleLibrary)
                    .then(|| {
                        definition(
                            model,
                            target.dir,
                            &LinkKind::SharedLibrary.create_flags_key(language),
                        )
                    })
                    .flatten()
            })
            .map_or_else(|| kind.default_create_flag(darwin).into(), Clone::clone),
    )?);
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
        let form = if target
            .props
            .get(b"WIN32_EXECUTABLE")
            .is_some_and(|value| truth::is_on(value))
        {
            b"_CREATE_WIN32_EXE".as_slice()
        } else {
            b"_CREATE_CONSOLE_EXE"
        };
        if let Some(value) = definition(model, target.dir, &[b"CMAKE_", language, form].concat()) {
            flags.push(parse_flags(value.clone())?);
        }
    }
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
    if kind == LinkKind::SharedLibrary && darwin {
        flags.extend(darwin_versions(model, target, language));
    }
    if darwin {
        deduplicate_darwin_flags(&mut flags);
    }
    let mut inputs = objects.clone();
    let mut before_link = Vec::new();
    if kind != LinkKind::StaticArchive && super::artifacts::is_dll_platform(model, target.dir) {
        let definitions =
            definition_file(model, target, config, usage, sources, &objects, language)?;
        inputs.extend(definitions.inputs);
        flags.extend(definitions.flag);
        before_link.extend(definitions.invocation);
    }
    let flags = join_words(flags);
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
    let libraries = link_libraries(
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
    )?;
    let mut implicit_inputs = Vec::new();
    if kind != LinkKind::StaticArchive
        && let Some(id) = model.find_target(target.dir, &target.name)
    {
        let context = EvalContext {
            config: config.clone(),
            link_language: Some(step.language.clone()),
            traversal: UsageTraversal::Link,
            ..EvalContext::default()
        };
        for entry in usage
            .collect_property(id, b"LINK_DEPENDS", &context)?
            .entries
        {
            let path = join(&model.dir(target.dir).source_dir, &entry.value);
            let path = build_relative(model, &path);
            if !inputs.contains(&path) && !implicit_inputs.contains(&path) {
                implicit_inputs.push(path);
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
    let install_name_dir = if darwin && soname.is_some() {
        install_name_prefix(model, target, artifacts, config, None)?
    } else {
        BString::default()
    };
    let shared_flags_key = [b"CMAKE_SHARED_LIBRARY_", language, b"_FLAGS"].concat();
    let output = quote(&step.output);
    let output_ext = Lexical::new(&step.output).stem_extension(true).1;
    let output_base = quote(step.output.strip_suffix(output_ext).unwrap_or(&step.output));
    let object_dir = super::object_dir(model, target, config);
    let lookup = |key: &[u8]| match key {
        b"CMAKE_LINKER" => linker.executable.as_ref().map(|path| quote(path)),
        b"TARGET" => Some(output.clone()),
        b"TARGET_BASE" => Some(output_base.clone()),
        b"OBJECT_DIR" | b"TARGET_SUPPORT_DIR" => Some(quote(&object_dir)),
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
        b"FLAGS" | b"LANGUAGE_COMPILE_FLAGS" => Some(driver.clone()),
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

    let template = option(kind.rule_variable()).filter(|v| !v.is_empty());
    let mut invocations = if kind == LinkKind::StaticArchive {
        archive_commands(
            &step.output,
            &objects,
            cmake(),
            &policy,
            (ipo, msvc),
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
        && matches!(
            model.generator,
            Some(
                rcmake_model::GeneratorKind::Ninja | rcmake_model::GeneratorKind::NinjaMultiConfig
            )
        )
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
            let mut usage = UsageResolver::new(model);
            evaluate(&parse(value)?, &ctx, &mut usage.host(target.dir))
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
    implicit_inputs.retain(|path| !inputs.contains(path));
    Ok((
        BuildStep::Link {
            implicit_inputs,
            inputs,
            step,
            invocations: before_link,
        },
        install_name_dir,
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
) -> Result<BString, ResolveError> {
    if step.kind == LinkKind::StaticArchive {
        return Ok(BString::default());
    }
    let link_fragments = step
        .fragments
        .iter()
        .filter(|f| f.role != LinkFragmentRole::Flags);
    let mut libraries = linked_libraries(step);
    let named = frameworks::named_framework_files(model, target, config, step, &libraries);
    libraries.extend(named);
    let runtime = rpath::runtime_libraries(model, target, config, step, &libraries, usage)?;
    let rpaths = rpath::link_flags(
        model,
        target,
        config,
        step,
        usage.source_paths(),
        &runtime,
        link_languages,
    )?;
    inputs.extend(libraries);
    let (mut arguments, mut rpaths) = if darwin {
        (Vec::new(), Some(rpaths))
    } else {
        (rpaths, None)
    };
    for fragment in link_fragments {
        if darwin && fragment.role == LinkFragmentRole::Libraries {
            arguments.extend(rpaths.take().into_iter().flatten());
        }
        arguments.push(wrapper.argument(&fragment.text.value)?);
    }
    arguments.extend(rpaths.into_iter().flatten());
    arguments.extend(runtime_args.iter().map(|arg| quote(arg)));
    if let Some(standard) = definition(
        model,
        target.dir,
        &[b"CMAKE_", step.language.as_slice(), b"_STANDARD_LIBRARIES"].concat(),
    ) {
        arguments.push(wrapper.driver_flags(standard));
    }
    Ok(join_words(arguments))
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

fn linked_libraries(step: &LinkStep) -> Vec<BString> {
    step.fragments
        .iter()
        .filter(|f| f.role == LinkFragmentRole::Libraries)
        .flat_map(|fragment| simple_args(&fragment.text.value).into_iter().flatten())
        .filter(|arg| library_file(arg))
        .collect()
}
