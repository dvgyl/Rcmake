//! Swift split-compilation action model, module path resolution, and flags.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, path_lex::Lexical, shell::quote, truth, version};
use rcmake_diag::{
    Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus, RenderOptions,
};
use rcmake_genex::{ConfigId, EvalContext, UsageTraversal};
use rcmake_host::Stream;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};

use super::artifacts::build_relative;
use super::compile::{formatted_defines, formatted_includes, swift_compile_flags};
use super::shell::{join_words, rule_invocations};
use super::{definition, join, object_dir};
use crate::{
    BuildStep, CustomStep, ResolveError, ResolvedTarget, SwiftAction, SwiftActionKind,
    sources::TargetInputs, usage::UsageResolver,
};

/// Swift compilation mode selected under `CMP0157` `NEW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwiftCompileMode {
    /// Incremental per-file compilation (`-incremental`).
    Incremental,
    /// Single-file compilation (no `-wmo` or `-incremental`).
    Singlefile,
    /// Whole-module optimization (`-wmo`).
    Wholemodule,
    /// Unrecognized `Swift_COMPILATION_MODE` value.
    Unknown,
}

/// Return whether split Swift compilation (`CMP0157` `NEW`) is active.
pub(crate) fn is_split_build(model: &ProjectModel, target: &Target) -> bool {
    definition(model, target.dir, b"CMAKE_Swift_COMPILATION_MODE_DEFAULT")
        .is_some_and(|value| !value.is_empty())
        && definition(model, target.dir, b"CMAKE_Swift_COMPILE_OBJECT")
            .is_none_or(|value| value.trim_ascii() != b":")
}

/// Return whether `target` emits an importable `.swiftmodule`.
pub(crate) fn is_importable(target: &Target) -> bool {
    if target.kind == TargetKind::Executable {
        target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|value| truth::is_on(value))
    } else {
        !matches!(
            target.kind,
            TargetKind::Utility | TargetKind::InterfaceLibrary | TargetKind::GlobalTarget
        )
    }
}

/// Return whether separate module emission (`CMP0215`) is enabled for `target`.
pub(crate) fn emit_module_separately(target: &Target) -> bool {
    if let Some(prop) = target.props.get(b"Swift_SEPARATE_MODULE_EMISSION") {
        return truth::is_on(prop);
    }
    target.policies.policies.get(PolicyId::new(215)) == PolicyStatus::New
}

/// Evaluate `Swift_COMPILATION_MODE` for `target` in `config` (`cmLocalGenerator::GetSwiftCompileMode`).
pub(crate) fn compile_mode(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<SwiftCompileMode>, ResolveError> {
    let Some(evaluated) = evaluated_compile_mode(model, target, config, usage)? else {
        return Ok(None);
    };
    let mode = match evaluated.as_slice() {
        b"wholemodule" => SwiftCompileMode::Wholemodule,
        b"singlefile" => SwiftCompileMode::Singlefile,
        b"incremental" => SwiftCompileMode::Incremental,
        _ => SwiftCompileMode::Unknown,
    };
    Ok(Some(mode))
}

/// Evaluate the effective `Swift_COMPILATION_MODE` value for `target` in `config`
/// when `CMP0157` `NEW` (`CMAKE_Swift_COMPILATION_MODE_DEFAULT`) is active.
pub fn swift_compilation_mode(
    model: &ProjectModel,
    target: &Target,
    config: &[u8],
) -> Result<Option<BString>, ResolveError> {
    let mut usage = UsageResolver::new(model);
    let config = ConfigId(config.into());
    evaluated_compile_mode(model, target, &config, &mut usage)
}

fn evaluated_compile_mode(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<BString>, ResolveError> {
    let Some(default) = definition(model, target.dir, b"CMAKE_Swift_COMPILATION_MODE_DEFAULT")
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let raw = target
        .props
        .get(b"Swift_COMPILATION_MODE")
        .unwrap_or(default);
    if rcmake_genex::find_genex(raw).is_some()
        && let Some(id) = model.find_target(target.dir, &target.name)
    {
        let ctx = EvalContext {
            config: config.clone(),
            head: Some(crate::usage::handle(id)),
            language: Some("Swift".into()),
            ..EvalContext::default()
        };
        Ok(Some(usage.eval_ide_property(
            id,
            b"Swift_COMPILATION_MODE",
            raw,
            &ctx,
        )?))
    } else {
        Ok(Some(raw.clone()))
    }
}

/// Emit `Swift_LANGUAGE_VERSION`, `Swift_PACKAGE_NAME` (`CMP0216`), and compilation mode flags.
pub(crate) fn target_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: Option<&mut UsageResolver<'_>>,
) -> Result<Vec<BString>, ResolveError> {
    let mut flags = Vec::new();
    let compiler_version = definition(model, target.dir, b"CMAKE_Swift_COMPILER_VERSION")
        .map_or(b"".as_slice(), |value| value.as_slice());

    // cmGeneratorTarget::AddSwiftTargetFlags
    if let Some(lang_version) = target
        .props
        .get(b"Swift_LANGUAGE_VERSION")
        .filter(|value| !value.is_empty())
        && (compiler_version.is_empty() || version::compare(compiler_version, b"4.2").is_ge())
    {
        flags.push("-swift-version".into());
        flags.push(quote(lang_version));
    }

    if model.generator != Some(GeneratorKind::Xcode)
        && (compiler_version.is_empty() || version::compare(compiler_version, b"5.8").is_ge())
    {
        let package = swift_package_name(model, target);
        if !package.is_empty()
            && let Some(flag) = definition(model, target.dir, b"CMAKE_Swift_PACKAGE_NAME_FLAG")
                .filter(|value| !value.is_empty())
        {
            flags.push(flag.clone());
            flags.push(quote(&package));
        }
    }

    // cmLocalGenerator::GetTargetCompileFlags Swift compilation mode
    if is_split_build(model, target) {
        let (mode, source_paths) = match usage {
            Some(resolver) => (
                compile_mode(model, target, config, resolver)?,
                resolver.source_paths(),
            ),
            None => (
                compile_mode(model, target, config, &mut UsageResolver::new(model))?,
                None,
            ),
        };
        if let Some(mode) = mode {
            match mode {
                SwiftCompileMode::Incremental => {
                    let flag = definition(
                        model,
                        target.dir,
                        b"CMAKE_Swift_COMPILE_OPTIONS_INCREMENTAL",
                    )
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "-incremental".into());
                    flags.push(flag);
                }
                SwiftCompileMode::Wholemodule => {
                    let flag = definition(model, target.dir, b"CMAKE_Swift_COMPILE_OPTIONS_WMO")
                        .filter(|v| !v.is_empty())
                        .cloned()
                        .unwrap_or_else(|| "-wmo".into());
                    flags.push(flag);
                }
                SwiftCompileMode::Singlefile => {}
                SwiftCompileMode::Unknown => {
                    if let Some(paths) = source_paths {
                        let diagnostic = Diagnostic {
                            annotations: None,
                            kind: MessageKind::AuthorWarning,
                            effect: Effect::None,
                            category: None,
                            policy: None,
                            location: Location::Backtrace(super::listed_backtrace(
                                &target.backtrace,
                            )),
                            body: format!(
                                "Unknown Swift_COMPILATION_MODE on target '{}'",
                                target.name
                            )
                            .into(),
                        };
                        paths.host.write_out(
                            Stream::Stderr,
                            &rcmake_diag::render(
                                &diagnostic,
                                &RenderOptions {
                                    relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                                    ..RenderOptions::default()
                                },
                            ),
                        );
                    }
                }
            }
        }
    }

    Ok(flags)
}

/// Return `Swift_PACKAGE_NAME`, or `PROJECT_NAME` under `CMP0216` `NEW`.
///
/// Matches `cmGeneratorTarget::GetSwiftPackageName`: an explicitly set empty
/// `Swift_PACKAGE_NAME` overrides and clears the `CMP0216` `NEW` fallback.
#[must_use]
pub fn swift_package_name(model: &ProjectModel, target: &Target) -> BString {
    if let Some(name) = target.props.get(b"Swift_PACKAGE_NAME") {
        return name.clone();
    }
    if target.policies.policies.get(PolicyId::new(216)) == PolicyStatus::New
        && let Some(project) = definition(model, target.dir, b"PROJECT_NAME")
    {
        return project.clone();
    }
    BString::default()
}

/// Return `Swift_MODULE_NAME` or the target name with `-` replaced by `_`.
pub(crate) fn module_name(target: &Target) -> BString {
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

/// Return the absolute directory containing `target`'s `.swiftmodule` (`GetSwiftModuleDirectory`).
pub(crate) fn module_directory(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
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
    Ok(dir)
}

/// Return the build-relative path to `target`'s `.swiftmodule` (`GetSwiftModulePath`).
pub(crate) fn module_path(
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
    let dir = module_directory(model, target, config, usage)?;
    Ok(build_relative(model, &join(&dir, &filename)))
}

/// Return the library base name used for `-module-link-name`.
pub(crate) fn library_base_name(
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
        TargetKind::Executable => "RUNTIME",
        TargetKind::StaticLibrary => "ARCHIVE",
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
        let value = if target.kind == TargetKind::Executable {
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

/// Lowered grouped Swift steps and outputs for one target.
pub(crate) struct LoweredSwift {
    /// Module (optional) and object/combined steps with their [`SwiftAction`] metadata
    /// and optional compiler launcher prefix.
    pub(crate) steps: Vec<(BuildStep, SwiftAction, Option<BString>)>,
    /// Compiled `.o` files to feed into the target's link/archive step.
    pub(crate) objects: Vec<BString>,
    /// Build-relative `.swiftmodule` path emitted by this target (when importable).
    pub(crate) module_output: Option<BString>,
    /// Build-relative `output-file-map.json` path and JSON contents (for multi-output builds).
    pub(crate) output_file_map: Option<(BString, BString)>,
}

/// Lower all Swift compile units of `target` into grouped split-Swift actions (`CMP0157` `NEW`).
#[expect(
    clippy::too_many_arguments,
    reason = "Grouped Swift lowering collects flags, output maps, and upstream module dependencies."
)]
pub(crate) fn lower_target(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    resolved: &ResolvedTarget,
    resolved_targets: &IndexMap<TargetId, &ResolvedTarget>,
    inputs: Option<&TargetInputs>,
    extra_dependencies: &IndexSet<TargetId>,
    framework_dirs: &[(BString, bool)],
    position_independent: Option<bool>,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<LoweredSwift>, ResolveError> {
    if !is_split_build(model, target) {
        return Ok(None);
    }
    let swift_units: Vec<_> = resolved
        .compile_units
        .iter()
        .filter(|unit| {
            resolved
                .compile_groups
                .get(unit.group)
                .is_some_and(|group| group.language == b"Swift")
        })
        .collect();
    if swift_units.is_empty() {
        return Ok(None);
    }
    let Some(mode) = compile_mode(model, target, config, usage)? else {
        return Ok(None);
    };
    let Some(primary_group) = swift_units
        .first()
        .and_then(|unit| resolved.compile_groups.get(unit.group))
    else {
        return Ok(None);
    };

    let mod_name = module_name(target);
    let mod_file_path = module_path(model, target, config, &mod_name, usage)?;
    let obj_dir = object_dir(model, target, config);
    let obj_ext = definition(model, target.dir, b"CMAKE_Swift_OUTPUT_EXTENSION")
        .filter(|value| !value.is_empty())
        .map_or(b".o".as_slice(), |value| value.as_slice());
    let module_object_filename = join(&obj_dir, &[mod_name.as_slice(), obj_ext].concat());

    let importable = is_importable(target);
    let separate_module = emit_module_separately(target);
    let multi_thread = definition(model, target.dir, b"CMAKE_Swift_NUM_THREADS")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|threads| threads > 0);
    let single_output = !multi_thread && mode == SwiftCompileMode::Wholemodule;

    let mut common_flags = Vec::new();
    common_flags.push(BString::from(
        [b"-module-name ".as_slice(), &mod_name].concat(),
    ));
    if target.kind != TargetKind::Executable {
        let link_name = library_base_name(model, target, config, resolved, usage)?;
        common_flags.push(BString::from(
            [b"-module-link-name ".as_slice(), &link_name].concat(),
        ));
        common_flags.push("-parse-as-library".into());
    }
    if target.kind == TargetKind::StaticLibrary {
        common_flags.push("-static".into());
    }
    let required_standard =
        inputs.and_then(|i| i.standard_requirements.get(b"Swift".as_slice()).copied());
    let mut base_compile_flags = swift_compile_flags(
        model,
        target,
        config,
        primary_group,
        required_standard,
        position_independent,
        usage,
    )?;
    // Preserve any source-level compile options from additional Swift groups.
    let mut seen_groups: IndexSet<usize> = swift_units
        .first()
        .map(|unit| unit.group)
        .into_iter()
        .collect();
    for unit in swift_units.iter().skip(1) {
        if seen_groups.insert(unit.group)
            && let Some(group) = resolved.compile_groups.get(unit.group)
            && let Some(start) = group.source_flag_start
        {
            for flag in group.flags.get(start..).into_iter().flatten() {
                if !flag.value.is_empty() {
                    base_compile_flags.push(
                        flag.value
                            .strip_prefix(b"SHELL:")
                            .map_or_else(|| quote(&flag.value), BString::from),
                    );
                }
            }
        }
    }
    common_flags.extend(base_compile_flags);

    let defines = formatted_defines(model, target, config, primary_group);
    let includes = formatted_includes(model, target, primary_group, framework_dirs);

    let source_dir = &model.dir(target.dir).source_dir;
    let dir_props = &model.dir(target.dir).source_props;
    let replace_depfile_ext = definition(model, target.dir, b"CMAKE_Swift_DEPFLE_EXTNSION_REPLACE")
        .is_some_and(|value| truth::is_on(value));

    let mut abs_sources = Vec::with_capacity(swift_units.len());
    let mut ninja_sources = Vec::with_capacity(swift_units.len());
    let mut object_outputs = Vec::new();
    let mut map_entries = BTreeMap::new();

    for unit in &swift_units {
        let abs_source = Lexical::new(&join(source_dir, &unit.source.value)).normalize();
        let ninja_source = build_relative(model, &abs_source);
        abs_sources.push(abs_source.clone());
        ninja_sources.push(ninja_source);

        if !single_output {
            let obj = unit.object.clone();
            object_outputs.push(obj.clone());

            let source_props = dir_props
                .get(abs_source.as_slice())
                .or_else(|| dir_props.get(unit.source.value.as_slice()));
            let swift_deps = source_props
                .and_then(|props| props.get(b"Swift_DEPENDENCIES_FILE"))
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or_else(|| [obj.as_slice(), b".swiftdeps"].concat().into());
            let swift_dia = source_props
                .and_then(|props| props.get(b"Swift_DIAGNOSTICS_FILE"))
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or_else(|| [obj.as_slice(), b".dia"].concat().into());
            let make_deps = if replace_depfile_ext {
                let lex = Lexical::new(&obj);
                let (stem, _) = lex.stem_extension(true);
                let parent = lex.parent();
                let name = [stem, b".d"].concat();
                if parent.is_empty() {
                    name.into()
                } else {
                    join(parent, &name)
                }
            } else {
                [obj.as_slice(), b".d"].concat().into()
            };

            let mut entry = BTreeMap::new();
            entry.insert("dependencies".into(), rcmake_json::Value::string(make_deps));
            entry.insert("diagnostics".into(), rcmake_json::Value::string(swift_dia));
            entry.insert("object".into(), rcmake_json::Value::string(obj));
            entry.insert(
                "swift-dependencies".into(),
                rcmake_json::Value::string(swift_deps),
            );
            map_entries.insert(abs_source, rcmake_json::Value::Object(entry));
        }
    }

    let output_file_map = if single_output {
        object_outputs.push(module_object_filename.clone());
        None
    } else {
        // `cmNinjaTargetGenerator::GenerateSwiftOutputFileMap` places the map and
        // default `.swiftdeps` under `<support_dir>/<config>/` even on single-config
        // Ninja when `CMAKE_BUILD_TYPE` is non-empty.
        let map_dir =
            if !config.0.is_empty() && !model.generator.is_some_and(GeneratorKind::multi_config) {
                join(&obj_dir, &config.0)
            } else {
                obj_dir.clone()
            };
        let target_swift_deps = target
            .props
            .get(b"Swift_DEPENDENCIES_FILE")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| join(&map_dir, &[target.name.as_slice(), b".swiftdeps"].concat()));
        let mut global = BTreeMap::new();
        global.insert(
            "swift-dependencies".into(),
            rcmake_json::Value::string(target_swift_deps),
        );
        let mut root = BTreeMap::new();
        root.insert(BString::default(), rcmake_json::Value::Object(global));
        root.extend(map_entries);
        let map_path = join(&map_dir, b"output-file-map.json");
        let json = rcmake_json::write_styled(&rcmake_json::Value::Object(root), "  ");
        common_flags.push("-output-file-map".into());
        common_flags.push(quote(&map_path));
        Some((map_path, json))
    };

    // Upstream imported `.swiftmodule` dependencies.
    let mut direct_dep_ids = IndexSet::new();
    direct_dep_ids.extend(resolved.dependencies.iter().copied());
    direct_dep_ids.extend(extra_dependencies.iter().copied());
    let link_ctx = EvalContext {
        config: config.clone(),
        traversal: UsageTraversal::Link,
        ..EvalContext::default()
    };
    if let Ok((links, _)) = usage.direct_links(resolved.id, &link_ctx) {
        direct_dep_ids.extend(links.iter().flat_map(|item| item.targets()));
    }
    let mut imported_module_deps = Vec::new();
    let mut seen_mod_deps = IndexSet::new();
    for dep_id in direct_dep_ids {
        if dep_id == resolved.id {
            continue;
        }
        let Some(dep) = model.target(dep_id) else {
            continue;
        };
        if dep.is_imported() || !is_importable(dep) {
            continue;
        }
        let dep_uses_swift = match resolved_targets.get(&dep_id) {
            Some(dep_resolved) => dep_resolved
                .compile_groups
                .iter()
                .any(|group| group.language == b"Swift"),
            None => usage
                .target_compile_languages(dep_id, config)?
                .contains(b"Swift".as_slice()),
        };
        if !dep_uses_swift {
            continue;
        }
        let dep_mod_name = module_name(dep);
        let dep_mod_path = module_path(model, dep, config, &dep_mod_name, usage)?;
        if seen_mod_deps.insert(dep_mod_path.clone()) {
            imported_module_deps.push(dep_mod_path);
        }
    }

    let template = definition(model, target.dir, b"CMAKE_Swift_COMPILE_OBJECT")
        .filter(|value| !value.is_empty() && value.trim_ascii() != b":")
        .map_or(
            b"<CMAKE_Swift_COMPILER> <DEFINES> <FLAGS> <INCLUDES> <SOURCE>".as_slice(),
            |value| value.as_slice(),
        );
    let compiler_index = super::compile::compiler_command_index(template, b"Swift");
    let rsp_flag = definition(model, target.dir, b"CMAKE_Swift_RESPONSE_FILE_FLAG")
        .filter(|value| !value.is_empty())
        .map_or(b"@".as_slice(), |value| value.as_slice());
    let quoted_sources = join_words(abs_sources.iter().map(|path| quote(path)));
    let source_count = swift_units.len();
    let source_word = if source_count == 1 {
        "source"
    } else {
        "sources"
    };
    let template_has_flags = template.windows(7).any(|part| part == b"<FLAGS>");

    type ExpandedSwiftAction = (
        crate::Invocation,
        Option<BString>,
        Option<crate::Invocation>,
        Option<BString>,
    );

    let expand_action = |flags_str: BString,
                         objects_str: BString,
                         output: &BString,
                         rsp_file: &BString,
                         usage: &mut UsageResolver<'_>|
     -> Result<ExpandedSwiftAction, ResolveError> {
        let lookup_paths = |key: &[u8]| match key {
            b"OBJECTS" | b"OBJECT" => Some(objects_str.clone()),
            b"OBJECT_DIR" | b"OBJECT_FILE_DIR" | b"TARGET_SUPPORT_DIR" => Some(quote(&obj_dir)),
            b"TARGET_NAME" => Some(quote(&target.name)),
            b"LANGUAGE" => Some(quote(b"Swift")),
            b"CMAKE_CURRENT_BINARY_DIR" => Some(quote(&model.dir(target.dir).binary_dir)),
            b"CMAKE_BINARY_DIR" => Some(quote(&model.dir(DirId::ROOT).binary_dir)),
            _ => None,
        };
        let swift_lang = BString::from("Swift");
        let launch = super::launcher::Command {
            role: "compile",
            config,
            output,
            source: abs_sources.first(),
            language: &swift_lang,
        };
        let invocations = rule_invocations(model, target.dir, template, |key| match key {
            b"FLAGS" => Some(flags_str.clone()),
            b"DEFINES" => Some(defines.clone()),
            b"INCLUDES" => Some(includes.clone()),
            b"SOURCE" => Some(quoted_sources.clone()),
            _ => lookup_paths(key),
        });
        let (invocation, launcher) = super::compile::finalize_compile_invocations(
            model,
            target,
            &launch,
            invocations,
            compiler_index,
            lookup_paths,
            None,
            usage,
        )?;
        let rsp_args = join_words([
            defines.clone(),
            includes.clone(),
            flags_str,
            quoted_sources.clone(),
        ]);
        let (rsp_invocation, rsp_args) = if template_has_flags && !rsp_args.is_empty() {
            let trimmed = rsp_flag.trim_ascii_end();
            let rsp_ref: BString = if trimmed.len() == rsp_flag.len() {
                quote(&[rsp_flag, rsp_file.as_slice()].concat())
            } else {
                join_words([BString::from(trimmed), quote(rsp_file)])
            };
            let rsp_invocations = rule_invocations(model, target.dir, template, |key| match key {
                b"FLAGS" => Some(rsp_ref.clone()),
                b"DEFINES" | b"INCLUDES" | b"SOURCE" => Some(BString::default()),
                _ => lookup_paths(key),
            });
            let (rsp_inv, _) = super::compile::finalize_compile_invocations(
                model,
                target,
                &launch,
                rsp_invocations,
                compiler_index,
                lookup_paths,
                None,
                usage,
            )?;
            (Some(rsp_inv), Some(rsp_args))
        } else {
            (None, None)
        };
        Ok((invocation, launcher, rsp_invocation, rsp_args))
    };

    let mut steps = Vec::new();
    let module_flags = if importable {
        vec![
            BString::from("-emit-module"),
            BString::from(
                [
                    b"-emit-module-path ".as_slice(),
                    quote(&mod_file_path).as_slice(),
                ]
                .concat(),
            ),
        ]
    } else {
        Vec::new()
    };

    if importable && separate_module {
        let mut flags = module_flags.clone();
        flags.extend(common_flags.iter().cloned());
        let rsp_file: BString = [mod_file_path.as_slice(), b".rsp"].concat().into();
        let (invocation, launcher, rsp_invocation, rsp_args) = expand_action(
            join_words(flags),
            quote(&mod_file_path),
            &mod_file_path,
            &rsp_file,
            usage,
        )?;
        let description: BString =
            format!("Building Swift Module '{mod_name}' with {source_count} {source_word}").into();
        let mut custom_depends = ninja_sources.clone();
        custom_depends.extend(imported_module_deps.iter().cloned());
        let step = BuildStep::Custom(CustomStep {
            outputs: vec![mod_file_path.clone()],
            byproducts: Vec::new(),
            depends: custom_depends,
            invocations: vec![invocation],
            comment: Some(description.clone()),
            uses_terminal: false,
            job_pool: None,
            depfile: None,
            symbolic: false,
            symbolic_outputs: Vec::new(),
            target_dependencies: Vec::new(),
        });
        let action = SwiftAction {
            kind: SwiftActionKind::Module,
            module_name: mod_name.clone(),
            sources: ninja_sources.clone(),
            abs_sources: abs_sources.clone(),
            outputs: vec![mod_file_path.clone()],
            implicit_inputs: imported_module_deps.clone(),
            order_only_inputs: Vec::new(),
            description,
            rsp_file,
            rsp_invocation,
            rsp_args,
            single_output,
            restat: true,
        };
        steps.push((step, action, launcher));
    }

    {
        let mut flags = vec![BString::from("-c")];
        if single_output {
            flags.push(BString::from(
                [b"-o ".as_slice(), quote(&module_object_filename).as_slice()].concat(),
            ));
        }
        flags.extend(common_flags);
        let mut action_outputs = object_outputs.clone();
        let mut order_only_inputs = Vec::new();
        let kind = if importable && separate_module {
            // Serialize the split object compile after emit-module so the two actions
            // do not race on the `.swiftdeps` files declared in `output-file-map.json`.
            order_only_inputs.push(mod_file_path.clone());
            SwiftActionKind::Objects
        } else if importable {
            flags.extend(module_flags);
            action_outputs.push(mod_file_path.clone());
            SwiftActionKind::Combined
        } else {
            SwiftActionKind::Objects
        };
        let primary_object = action_outputs.first().cloned().unwrap_or_default();
        let rsp_file: BString = [module_object_filename.as_slice(), b".swift.rsp"]
            .concat()
            .into();
        let (invocation, launcher, rsp_invocation, rsp_args) = expand_action(
            join_words(flags),
            join_words(object_outputs.iter().map(|path| quote(path))),
            &primary_object,
            &rsp_file,
            usage,
        )?;
        let description: BString =
            format!("Building Swift Module '{mod_name}' objects with {source_count} {source_word}")
                .into();
        let byproducts = action_outputs.get(1..).unwrap_or(&[]).to_vec();
        let first_source = ninja_sources.first().cloned().unwrap_or_default();
        let step = BuildStep::Compile {
            object: primary_object,
            byproducts,
            source: first_source,
            compile_input: None,
            invocation,
            depfile: None,
            modmap: None,
            dyndep: None,
        };
        let action = SwiftAction {
            kind,
            module_name: mod_name,
            sources: ninja_sources,
            abs_sources,
            outputs: action_outputs,
            implicit_inputs: imported_module_deps,
            order_only_inputs,
            description,
            rsp_file,
            rsp_invocation,
            rsp_args,
            single_output,
            restat: true,
        };
        steps.push((step, action, launcher));
    }

    Ok(Some(LoweredSwift {
        steps,
        objects: object_outputs,
        module_output: importable.then_some(mod_file_path),
        output_file_map,
    }))
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_diag::{PolicyId, PolicyMap, PolicyStatus};
    use rcmake_genex::ConfigId;
    use rcmake_host::{Host, PureHost, WriteMode};
    use rcmake_model::{
        BT, DirId, GeneratorKind, ProjectModel, Target, TargetKind, TargetOrigin, UsageField,
    };

    use crate::{BuildStep, SwiftActionKind, resolve};

    fn swift_model(cmp0157: PolicyStatus) -> ProjectModel {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/work/src".into(), "/work/build".into());
        model.generator = Some(GeneratorKind::Ninja);
        model
            .global
            .props
            .set("ENABLED_LANGUAGES".into(), "Swift".into());
        let root = model.dir_mut(DirId::ROOT);
        root.vars.insert("PROJECT_NAME".into(), "SwiftDemo".into());
        root.vars
            .insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
        root.vars
            .insert("CMAKE_Swift_COMPILER_ID".into(), "Apple".into());
        root.vars
            .insert("CMAKE_Swift_COMPILER_VERSION".into(), "6.0".into());
        root.vars.insert(
            "CMAKE_Swift_MODULE_TRIPLE".into(),
            "arm64-apple-macos".into(),
        );
        root.vars
            .insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
        root.vars
            .insert("CMAKE_Swift_OUTPUT_EXTENSION".into(), ".o".into());
        root.vars
            .insert("CMAKE_Swift_DEFINE_FLAG".into(), "-D".into());
        root.vars
            .insert("CMAKE_INCLUDE_FLAG_Swift".into(), "-I ".into());
        root.vars.insert(
            "CMAKE_Swift_PACKAGE_NAME_FLAG".into(),
            "-package-name".into(),
        );
        root.vars
            .insert("CMAKE_Swift_NUM_THREADS".into(), "4".into());
        root.vars.insert("CMAKE_AR".into(), "/usr/bin/ar".into());
        root.vars.insert(
            "CMAKE_Swift_ARCHIVE_CREATE".into(),
            "<CMAKE_AR> crs <TARGET> <OBJECTS>".into(),
        );
        root.vars.insert(
            "CMAKE_Swift_CREATE_STATIC_LIBRARY".into(),
            "<CMAKE_Swift_COMPILER> -emit-library -static -o <TARGET> <OBJECTS> <LINK_FLAGS>"
                .into(),
        );
        root.vars.insert(
            "CMAKE_Swift_LINK_EXECUTABLE".into(),
            "<CMAKE_Swift_COMPILER> -j 4 -num-threads 4 -emit-executable -o <TARGET> <FLAGS> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES>".into(),
        );
        if cmp0157 == PolicyStatus::New {
            root.vars.insert(
                "CMAKE_Swift_COMPILATION_MODE_DEFAULT".into(),
                "incremental".into(),
            );
            root.vars.insert(
                "CMAKE_Swift_COMPILE_OBJECT".into(),
                "<CMAKE_Swift_COMPILER> -j 4 -num-threads 4 <DEFINES> <FLAGS> <INCLUDES> <SOURCE>"
                    .into(),
            );
        } else {
            root.vars
                .insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
            root.vars.insert(
                "CMAKE_Swift_CREATE_STATIC_LIBRARY".into(),
                "<CMAKE_Swift_COMPILER> -emit-library -static -o <TARGET> -module-name <SWIFT_MODULE_NAME> -module-link-name <SWIFT_LIBRARY_NAME> -emit-module -emit-module-path <SWIFT_MODULE> <DEFINES> <FLAGS> <INCLUDES> <SWIFT_SOURCES> <LINK_FLAGS> <LINK_LIBRARIES>".into(),
            );
        }
        model
    }

    fn swift_policies(
        cmp0157: PolicyStatus,
        cmp0195: PolicyStatus,
        cmp0215: PolicyStatus,
    ) -> PolicyMap {
        let mut map = PolicyMap::default();
        let _ = map.set(PolicyId::new(157), cmp0157);
        let _ = map.set(PolicyId::new(195), cmp0195);
        let _ = map.set(PolicyId::new(215), cmp0215);
        let _ = map.set(PolicyId::new(214), PolicyStatus::New);
        let _ = map.set(PolicyId::new(216), PolicyStatus::New);
        map
    }

    fn add_swift_target(
        model: &mut ProjectModel,
        name: &str,
        kind: TargetKind,
        sources: &[&str],
        policies: PolicyMap,
    ) -> Result<rcmake_model::TargetId, Box<dyn std::error::Error>> {
        let mut target = Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            policies,
        );
        for src in sources {
            target
                .sources
                .push(BT::new((*src).into(), Default::default()));
        }
        Ok(model.add_target(target)?)
    }

    #[test]
    fn split_swift_separate_module_disambiguates_basenames_and_writes_output_map()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::default();
        host.create_dir_all(std::path::Path::new("/work/src/a"))?;
        host.create_dir_all(std::path::Path::new("/work/src/b"))?;
        host.write(
            std::path::Path::new("/work/src/a/dup.swift"),
            b"",
            WriteMode::Truncate,
        )?;
        host.write(
            std::path::Path::new("/work/src/b/dup.swift"),
            b"",
            WriteMode::Truncate,
        )?;
        let mut model = swift_model(PolicyStatus::New);
        let policies = swift_policies(PolicyStatus::New, PolicyStatus::New, PolicyStatus::New);
        let lib = add_swift_target(
            &mut model,
            "mylib",
            TargetKind::StaticLibrary,
            &["a/dup.swift", "b/dup.swift"],
            policies,
        )?;
        let raw = model.target_mut(lib).ok_or("missing lib")?;
        raw.props.set("Swift_LANGUAGE_VERSION".into(), "6".into());
        raw.usage.push(
            UsageField::CompileOptions,
            BT::new("-emit-objc-header".into(), Default::default()),
        );
        raw.usage.push(
            UsageField::CompileOptions,
            BT::new("-emit-objc-header-path".into(), Default::default()),
        );
        raw.usage.push(
            UsageField::CompileOptions,
            BT::new("/work/build/mylib-Swift.h".into(), Default::default()),
        );

        let result = resolve(&model, &host, &[ConfigId::default()])?;
        let graph = result.graphs.first().ok_or("missing graph")?;
        assert_eq!(graph.swift_actions.len(), 2);

        let (&mod_idx, mod_action) = graph
            .swift_actions
            .iter()
            .find(|(_, a)| a.kind == SwiftActionKind::Module)
            .ok_or("missing module action")?;
        let (&_obj_idx, obj_action) = graph
            .swift_actions
            .iter()
            .find(|(_, a)| a.kind == SwiftActionKind::Objects)
            .ok_or("missing objects action")?;

        // CMP0195 module triple layout.
        assert_eq!(
            mod_action.outputs,
            vec![BString::from(
                "mylib.swiftmodule/arm64-apple-macos.swiftmodule"
            )]
        );
        // Split objects action waits on its own module via an order-only edge.
        assert_eq!(
            obj_action.order_only_inputs,
            vec![BString::from(
                "mylib.swiftmodule/arm64-apple-macos.swiftmodule"
            )]
        );
        // Equal-basename sources produce distinct object paths.
        assert_eq!(obj_action.outputs.len(), 2);
        assert_ne!(obj_action.outputs[0], obj_action.outputs[1]);

        let mod_shell = match &graph.steps[mod_idx] {
            BuildStep::Custom(step) => step.invocations[0].shell.to_string(),
            _ => return Err("module step should be Custom".into()),
        };
        assert!(mod_shell.contains("-emit-module"), "{mod_shell}");
        assert!(mod_shell.contains("-incremental"), "{mod_shell}");
        assert!(mod_shell.contains("-swift-version 6"), "{mod_shell}");
        assert!(mod_shell.contains("-package-name SwiftDemo"), "{mod_shell}");
        assert!(mod_shell.contains("-emit-objc-header"), "{mod_shell}");
        assert!(mod_shell.contains("output-file-map.json"), "{mod_shell}");

        // Final archive step implicitly depends on its own .swiftmodule.
        let link_step = graph
            .steps
            .iter()
            .find_map(|step| match step {
                BuildStep::Link {
                    implicit_inputs, ..
                } => Some(implicit_inputs),
                _ => None,
            })
            .ok_or("missing link step")?;
        assert!(
            link_step
                .iter()
                .any(|p| p == "mylib.swiftmodule/arm64-apple-macos.swiftmodule"),
            "{link_step:?}"
        );

        // Verify output-file-map.json recorded on BuildGraph with absolute source keys.
        let map_bytes = graph
            .swift_output_file_maps
            .get(b"CMakeFiles/mylib.dir/output-file-map.json".as_slice())
            .ok_or("missing output-file-map.json in BuildGraph")?;
        let map_text = std::str::from_utf8(map_bytes)?;
        assert!(map_text.contains("/work/src/a/dup.swift"), "{map_text}");
        assert!(map_text.contains("/work/src/b/dup.swift"), "{map_text}");
        assert!(map_text.contains("\"swift-dependencies\""), "{map_text}");
        Ok(())
    }

    #[test]
    fn wholemodule_single_thread_uses_direct_o_and_legacy_cmp0157_old_preserves_colon_rule()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::default();
        host.create_dir_all(std::path::Path::new("/work/src"))?;
        host.write(
            std::path::Path::new("/work/src/a.swift"),
            b"",
            WriteMode::Truncate,
        )?;
        host.write(
            std::path::Path::new("/work/src/b.swift"),
            b"",
            WriteMode::Truncate,
        )?;
        // 1. Wholemodule single-output (`CMAKE_Swift_NUM_THREADS=0`, CMP0157 NEW, CMP0215 OLD -> Combined -o).
        let mut model = swift_model(PolicyStatus::New);
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("CMAKE_Swift_NUM_THREADS".into(), "0".into());
        let lib = add_swift_target(
            &mut model,
            "wmo",
            TargetKind::StaticLibrary,
            &["a.swift", "b.swift"],
            swift_policies(PolicyStatus::New, PolicyStatus::Old, PolicyStatus::Old),
        )?;
        model
            .target_mut(lib)
            .ok_or("missing wmo")?
            .props
            .set("Swift_COMPILATION_MODE".into(), "wholemodule".into());
        let result = resolve(&model, &host, &[ConfigId::default()])?;
        let graph = result.graphs.first().ok_or("missing graph")?;
        assert_eq!(graph.swift_actions.len(), 1);
        let (&idx, action) = graph.swift_actions.iter().next().ok_or("missing action")?;
        assert_eq!(action.kind, SwiftActionKind::Combined);
        assert_eq!(action.outputs.len(), 2); // single module object + wmo.swiftmodule
        assert!(action.outputs.iter().any(|o| o == "wmo.swiftmodule"));
        let shell = match &graph.steps[idx] {
            BuildStep::Compile { invocation, .. } => invocation.shell.to_string(),
            _ => return Err("expected Compile step".into()),
        };
        assert!(shell.contains("-wmo"), "{shell}");
        assert!(shell.contains("-o "), "{shell}");
        assert!(!shell.contains("-output-file-map"), "{shell}");

        // 2. Legacy CMP0157 OLD preserves colon-rule link-time Swift compilation (0 swift_actions).
        let mut legacy = swift_model(PolicyStatus::Old);
        add_swift_target(
            &mut legacy,
            "oldlib",
            TargetKind::StaticLibrary,
            &["a.swift"],
            swift_policies(PolicyStatus::Old, PolicyStatus::Old, PolicyStatus::Old),
        )?;
        let legacy_res = resolve(&legacy, &host, &[ConfigId::default()])?;
        let legacy_graph = legacy_res.graphs.first().ok_or("missing legacy graph")?;
        assert!(legacy_graph.swift_actions.is_empty());
        let link_shell = legacy_graph
            .steps
            .iter()
            .find_map(|step| match step {
                BuildStep::Link { invocations, .. } => Some(
                    invocations
                        .iter()
                        .map(|inv| inv.shell.to_string())
                        .collect::<Vec<_>>()
                        .join(" && "),
                ),
                _ => None,
            })
            .ok_or("missing legacy link step")?;
        assert!(link_shell.contains("-emit-module"), "{link_shell}");
        assert!(link_shell.contains("a.swift"), "{link_shell}");
        Ok(())
    }

    #[test]
    fn multi_command_rule_launchers_empty_package_and_genex_module_search_path()
    -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_model::{LinkItem, UsageScope};

        let host = PureHost::default();
        host.create_dir_all(std::path::Path::new("/work/src"))?;
        for file in ["a.swift", "main.swift"] {
            host.write(
                &std::path::Path::new("/work/src").join(file),
                b"",
                WriteMode::Truncate,
            )?;
        }

        let mut model = swift_model(PolicyStatus::New);
        model.dir_mut(DirId::ROOT).vars.insert(
            "CMAKE_Swift_COMPILE_OBJECT".into(),
            "setup <OBJECT_DIR>;<CMAKE_Swift_COMPILER> -j 4 <DEFINES> <FLAGS> <INCLUDES> <SOURCE>;post <OBJECT_DIR>".into(),
        );
        let policies = swift_policies(PolicyStatus::New, PolicyStatus::New, PolicyStatus::New);

        // Library whose ONLY Swift source is selected by a generator expression, with a
        // custom Swift_MODULE_DIRECTORY, empty Swift_PACKAGE_NAME="", and both launchers.
        let lib = add_swift_target(
            &mut model,
            "genexlib",
            TargetKind::StaticLibrary,
            &["$<$<CONFIG:Debug>:a.swift>"],
            policies,
        )?;
        {
            let raw = model.target_mut(lib).ok_or("missing lib")?;
            raw.props.set(
                "Swift_MODULE_DIRECTORY".into(),
                "/work/build/custom_mods".into(),
            );
            raw.props.set("Swift_PACKAGE_NAME".into(), "".into());
            raw.props
                .set("Swift_COMPILER_LAUNCHER".into(), "ccache".into());
            raw.props
                .set("RULE_LAUNCH_COMPILE".into(), "rule-wrap".into());
        }

        // Consumer executable with NO module emission linking genexlib.
        let app = add_swift_target(
            &mut model,
            "app",
            TargetKind::Executable,
            &["main.swift"],
            policies,
        )?;
        model
            .target_mut(app)
            .ok_or("missing app")?
            .usage
            .link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: Some(UsageScope::Private),
                text: BT::new("genexlib".into(), Default::default()),
                lookup_dir: DirId::ROOT,
            });

        let result = resolve(&model, &host, &[ConfigId("Debug".into())])?;
        let graph = result.graphs.first().ok_or("missing graph")?;

        let (&mod_idx, mod_action) = graph
            .swift_actions
            .iter()
            .find(|(_, a)| a.module_name == "genexlib" && a.kind == SwiftActionKind::Module)
            .ok_or("missing genexlib module action")?;
        let mod_shell = match &graph.steps[mod_idx] {
            BuildStep::Custom(step) => step.invocations[0].shell.to_string(),
            _ => return Err("expected Custom step for module".into()),
        };
        assert!(mod_shell.contains("rule-wrap setup "), "{mod_shell}");
        assert!(
            mod_shell.contains("rule-wrap ccache /usr/bin/swiftc -j 4 "),
            "{mod_shell}"
        );
        assert!(mod_shell.contains(" && rule-wrap post "), "{mod_shell}");
        // Explicit Swift_PACKAGE_NAME="" clears CMP0216 NEW fallback.
        assert!(!mod_shell.contains("-package-name"), "{mod_shell}");

        // Response-file invocation preserves surrounding commands and launchers while moving
        // only compiler arguments into rsp_args.
        let rsp_inv = mod_action
            .rsp_invocation
            .as_ref()
            .ok_or("missing rsp_invocation")?;
        let rsp_shell = rsp_inv.shell.to_string();
        assert!(rsp_shell.contains("rule-wrap setup "), "{rsp_shell}");
        assert!(
            rsp_shell.contains("rule-wrap ccache /usr/bin/swiftc -j 4 "),
            "{rsp_shell}"
        );
        assert!(rsp_shell.contains("@custom_mods/"), "{rsp_shell}");
        assert!(rsp_shell.contains(" && rule-wrap post "), "{rsp_shell}");
        assert!(!rsp_shell.contains("a.swift"), "{rsp_shell}");
        let rsp_args = mod_action.rsp_args.as_ref().ok_or("missing rsp_args")?;
        assert!(
            rsp_args.to_string().contains("/work/src/a.swift"),
            "{rsp_args}"
        );
        assert!(rsp_args.to_string().contains("-emit-module"), "{rsp_args}");

        // Consumer executable has no module action of its own, yet receives both the
        // custom module -I search path and the upstream .swiftmodule implicit input.
        assert!(
            !graph
                .swift_actions
                .values()
                .any(|a| a.module_name == "app" && a.kind == SwiftActionKind::Module),
            "app executable must not emit a module action"
        );
        let (&app_idx, app_action) = graph
            .swift_actions
            .iter()
            .find(|(_, a)| a.module_name == "app")
            .ok_or("missing app objects action")?;
        assert!(
            app_action
                .implicit_inputs
                .iter()
                .any(|dep| dep == "custom_mods/genexlib.swiftmodule/arm64-apple-macos.swiftmodule"),
            "app missing upstream module implicit input: {:?}",
            app_action.implicit_inputs
        );
        let app_shell = match &graph.steps[app_idx] {
            BuildStep::Compile { invocation, .. } => invocation.shell.to_string(),
            _ => return Err("expected Compile step for app".into()),
        };
        assert!(
            app_shell.contains("-I/work/build/custom_mods"),
            "app missing custom module search path -I: {app_shell}"
        );
        Ok(())
    }
}
