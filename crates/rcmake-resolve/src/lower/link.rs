//! Link and archive rules.

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal, evaluate};
use rcmake_model::{DirId, ProjectModel, Target};

use super::artifacts::{build_relative, is_darwin, is_msvc};
use super::compile::language_flags;
use super::launcher;
use super::policy_flags::Flags;
use super::shell::{command as shell_command, direct, join_words, simple_args};
use super::{definition, join};
use crate::{
    ArtifactSet, BuildStep, LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget,
    sources::TargetInputs, usage::UsageResolver,
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
    shared_library, without_empty_output_flags,
};
pub(crate) use frameworks::bundle_name as framework_bundle_name;
pub(crate) use rpath::ide_rpaths;

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

/// Return the untagged flag fragments and IPO status of a codemodel link step.
pub fn codemodel_link_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    link: &LinkStep,
) -> Result<(Vec<BString>, bool), ResolveError> {
    let kind = link.kind;
    let language = link.language.as_slice();
    let mut flags = Vec::new();
    let policy = Flags::new(model, target, language);
    let ipo = policy.ipo(config);
    if kind == LinkKind::StaticArchive {
        return Ok((flags, ipo));
    }
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
        flags.push(join_words(language_flags));
    }
    let wrapper = LinkerDriver::new(model, target, language);
    if let Some(value) = creation_flags(model, target, kind, language) {
        let value = if target.policies.policies.get(PolicyId::new(181)) == PolicyStatus::New {
            wrapper.flag_string(value)?
        } else {
            wrapper.driver_flags(value)
        };
        flags.push(value);
    }
    let base = [b"CMAKE_", kind.flag_class(), b"_LINKER_FLAGS"].concat();
    let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
    flags.push(join_words(
        [base.as_slice(), &configured]
            .into_iter()
            .filter_map(|key| {
                definition(model, target.dir, key).map(|value| wrapper.driver_flags(value))
            }),
    ));
    flags.retain(|value| !value.is_empty());
    Ok((flags, ipo))
}

pub(super) struct LoweredLink {
    pub(super) step: BuildStep,
    pub(super) install_name_dir: BString,
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
    let arch = if kind == LinkKind::StaticArchive && language != b"Swift" {
        Vec::new()
    } else {
        super::apple::arch::flags(model, target, config, language, None)?
    };
    let language_compile_flags = (ninja_posix && kind != LinkKind::Executable).then(|| {
        [
            join_words(driver.iter().cloned()).as_slice(),
            b" ",
            join_words(arch.iter().cloned()).as_slice(),
        ]
        .concat()
        .into()
    });
    let swift_group = (language == b"Swift")
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
        driver.extend(arch);
        join_words(driver)
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
    if kind == LinkKind::Executable {
        let exports = target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|v| truth::is_on(v));
        let shared_build =
            definition(model, target.dir, b"BUILD_SHARED_LIBS").is_some_and(|v| truth::is_on(v));
        let shared_exports = exports
            && model
                .global
                .props
                .get(b"TARGET_SUPPORTS_SHARED_LIBS")
                .is_some_and(|v| truth::is_on(v))
            && definition(model, target.dir, b"CMAKE_SYSTEM_NAME").is_none_or(|v| v != b"AIX");
        for (enabled, prefix, suffix) in [
            (
                exports,
                b"CMAKE_EXE_EXPORTS_".as_slice(),
                b"_FLAG".as_slice(),
            ),
            (shared_build, b"CMAKE_SHARED_BUILD_", b"_FLAGS"),
            (shared_exports, b"CMAKE_SHARED_LIBRARY_LINK_", b"_FLAGS"),
        ] {
            if enabled
                && let Some(value) =
                    definition(model, target.dir, &[prefix, language, suffix].concat())
                        .filter(|value| !value.is_empty())
            {
                flags.push(wrapper.driver_flags(value));
            }
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
        ninja_posix,
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
    let one_step_swift = language == b"Swift" && super::compile::is_one_step_swift(model, target);
    let swift_defines = swift_group
        .map(|group| super::compile::formatted_defines(model, target, config, group))
        .unwrap_or_default();
    let swift_includes = swift_group
        .map(|group| super::compile::formatted_includes(model, target, group, framework_dirs))
        .unwrap_or_default();
    let mut extra_outputs = Vec::new();
    let mut swift_output_file_map = None;
    let mut driver = driver;
    let swift_sources = (language == b"Swift").then(|| {
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
    let swift_library_name = if language == b"Swift" {
        Some(swift_library_base_name(
            model, target, config, resolved, usage,
        )?)
    } else {
        None
    };
    let swift_module_name = (language == b"Swift").then(|| swift_module_name(target));
    let swift_module = if language == b"Swift" {
        Some(swift_module_path(
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
    Ok(LoweredLink {
        step: BuildStep::Link {
            implicit_inputs,
            inputs,
            step,
            invocations: before_link,
        },
        install_name_dir,
        extra_outputs,
        swift_output_file_map,
    })
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
    let mut rpaths =
        rpath::link_flags(model, target, config, step, usage, &runtime, link_languages)?;
    if ninja_posix && rpaths.len() > 1 {
        rpaths = vec![join_words(rpaths)];
    }
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
    if let Some(standard) = definition(
        model,
        target.dir,
        &[b"CMAKE_", step.language.as_slice(), b"_STANDARD_LIBRARIES"].concat(),
    ) {
        arguments.push(wrapper.driver_flags(standard));
    }
    if !ninja_posix {
        return Ok(join_words(arguments));
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
    Ok(line)
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

fn swift_library_base_name(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    resolved: &ResolvedTarget,
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
    if let Some(ide) = &resolved.ide
        && !ide.naming.base_name.is_empty()
    {
        return Ok(ide.naming.base_name.clone());
    }
    let class = match target.kind {
        rcmake_model::TargetKind::Executable => "RUNTIME",
        rcmake_model::TargetKind::StaticLibrary => "ARCHIVE",
        _ => "LIBRARY",
    };
    let config_upper = config.0.to_ascii_uppercase();
    let raw_name = super::output_name_keys(class, &config_upper)
        .into_iter()
        .find_map(|key| {
            target
                .props
                .get(key.as_slice())
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        });
    let base = if let Some((key, value)) = raw_name {
        if value.windows(2).any(|part| part == b"$<")
            && let Some(id) = model.find_target(target.dir, &target.name)
        {
            let ctx = EvalContext {
                config: config.clone(),
                head: Some(crate::usage::handle(id)),
                ..EvalContext::default()
            };
            usage.eval_ide_property(id, &key, value, &ctx)?
        } else {
            value.clone()
        }
    } else {
        target.name.clone()
    };
    let postfix = if !config.0.is_empty() {
        let postfix_key = [config_upper.as_slice(), b"_POSTFIX"].concat();
        let value = if target.kind == rcmake_model::TargetKind::Executable {
            target.props.get(&postfix_key)
        } else {
            super::target_property(model, target, &postfix_key)
        };
        value.map_or(b"".as_slice(), |value| value.as_slice())
    } else {
        b""
    };
    Ok([base.as_slice(), postfix].concat().into())
}

fn swift_module_name(target: &Target) -> BString {
    if let Some(name) = target
        .props
        .get(b"Swift_MODULE_NAME")
        .filter(|value| !value.is_empty())
    {
        return name.clone();
    }
    target
        .name
        .iter()
        .map(|&byte| if byte == b'-' { b'_' } else { byte })
        .collect::<Vec<_>>()
        .into()
}

fn swift_module_path(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    module_name: &[u8],
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
    let mut filename = target
        .props
        .get(b"Swift_MODULE")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| [module_name, b".swiftmodule"].concat().into());
    if target.policies.policies.get(PolicyId::new(195)) == PolicyStatus::New
        && let Some(triple) =
            definition(model, target.dir, b"CMAKE_Swift_MODULE_TRIPLE").filter(|v| !v.is_empty())
    {
        filename = [
            filename.as_slice(),
            b"/",
            triple.as_slice(),
            b".swiftmodule",
        ]
        .concat()
        .into();
    }
    let binary_dir = &model.dir(target.dir).binary_dir;
    let mut append_config = true;
    let mut dir = if let Some(raw) = target
        .props
        .get(b"Swift_MODULE_DIRECTORY")
        .filter(|value| !value.is_empty())
    {
        let evaluated = if raw.windows(2).any(|part| part == b"$<")
            && let Some(id) = model.find_target(target.dir, &target.name)
        {
            let ctx = EvalContext {
                config: config.clone(),
                head: Some(crate::usage::handle(id)),
                ..EvalContext::default()
            };
            usage.eval_ide_property(id, b"Swift_MODULE_DIRECTORY", raw, &ctx)?
        } else {
            raw.clone()
        };
        append_config = evaluated == *raw;
        if evaluated.is_empty() {
            binary_dir.clone()
        } else {
            join(binary_dir, &evaluated)
        }
    } else {
        binary_dir.clone()
    };
    if append_config
        && model
            .generator
            .is_some_and(rcmake_model::GeneratorKind::multi_config)
        && !config.0.is_empty()
    {
        dir = join(&dir, &config.0);
    }
    Ok(build_relative(model, &join(&dir, &filename)))
}
