//! Lower C++ named module scans and collation.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, path_lex::Lexical, shell::quote};
use rcmake_cxxmodules::{
    BmiInstallInfo, ModmapFormat, ModuleSourceInfo, ScannedSourceInfo, TargetDependInfo,
};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target};

use super::compile::compile_arguments;
use super::launcher;
use super::shell::{command, join_words, rule_invocations};
use super::{definition, expand_rule};
use crate::{BuildStep, CompileUnit, ResolveError, ResolvedTarget, ScanResponse};

/// Return the generated BMI install script path for a target and configuration.
#[must_use]
pub fn bmi_script_path(build_dir: &[u8], tdi: &[u8], config: &[u8], multi_config: bool) -> BString {
    let mut dir = Lexical::new(tdi).parent();
    if multi_config {
        dir = Lexical::new(dir).parent();
    }
    let dir = Lexical::new(build_dir).append(dir);
    let config = if config.is_empty() {
        b"noconfig"
    } else {
        config
    };
    Lexical::new(&dir).append(&[b"install-cxx-module-bmi-", config, b".cmake"].concat())
}

pub(super) fn modmap_path(object: &[u8]) -> BString {
    [object, b".modmap"].concat().into()
}

pub(super) fn module_map_flag(model: &ProjectModel, target: &Target, path: &[u8]) -> BString {
    let fallback: &[u8] = if module_map_format(model, target) == Ok(ModmapFormat::Gcc) {
        b"-fmodules-ts -fmodule-mapper=<MODULE_MAP_FILE>"
    } else {
        b"@<MODULE_MAP_FILE>"
    };
    let template = definition(model, target.dir, b"CMAKE_CXX_MODULE_MAP_FLAG")
        .filter(|flag| !flag.is_empty())
        .map_or(fallback, |flag| flag.as_slice());
    expand_rule(template, |key| {
        (key == b"MODULE_MAP_FILE").then(|| quote(path))
    })
}

pub(super) fn is_std_bmi(model: &ProjectModel, target: &Target) -> bool {
    (crate::cxx_modules::is_std_provider(target)
        || crate::cxx_modules::is_synthetic_provider(target))
        && definition(model, target.dir, b"CMAKE_CXX_COMPILER_ID")
            .is_none_or(|id| id != b"MSVC".as_slice())
}

pub(super) fn module_map_format(
    model: &ProjectModel,
    target: &Target,
) -> Result<ModmapFormat, ResolveError> {
    let compiler = definition(model, target.dir, b"CMAKE_CXX_COMPILER_ID");
    match definition(model, target.dir, b"CMAKE_CXX_MODULE_MAP_FORMAT").filter(|v| !v.is_empty()) {
        Some(format) => ModmapFormat::from_name(format).ok_or_else(|| {
            ResolveError::Message(format!("Unknown C++ module map format: {format}.").into())
        }),
        None => Ok(match compiler.map(|id| id.as_slice()) {
            Some(b"GNU") => ModmapFormat::Gcc,
            Some(b"MSVC") => ModmapFormat::Msvc,
            _ => ModmapFormat::Clang,
        }),
    }
}

/// Build the scan and collation steps of `target`.
///
/// # Errors
///
/// Returns an error if a scanned unit has no compile group, scan rule, or module metadata.
pub fn lower_modules(
    model: &ProjectModel,
    target: &ResolvedTarget,
) -> Result<Vec<BuildStep>, ResolveError> {
    lower_modules_with_standard(model, target, None, None)
}

pub(super) fn lower_modules_with_standard(
    model: &ProjectModel,
    resolved: &ResolvedTarget,
    standard: Option<&[u8]>,
    position_independent: Option<bool>,
) -> Result<Vec<BuildStep>, ResolveError> {
    if !resolved.compile_units.iter().any(|unit| unit.scan) {
        return Ok(Vec::new());
    }
    let module_info = resolved.cxx_modules.as_ref().ok_or_else(|| {
        ResolveError::Message("A scanned target has no C++ module metadata.".into())
    })?;
    let target = model
        .target(resolved.id)
        .ok_or_else(|| ResolveError::Message("The C++ module target is missing.".into()))?;
    let dir = target.dir;
    let root = model.dir(DirId::ROOT);
    let compiler = |suffix: &[u8]| {
        definition(model, dir, &[b"CMAKE_CXX_", suffix].concat())
            .cloned()
            .unwrap_or_default()
    };
    let template = compiler(b"SCANDEP_SOURCE");
    let module_map_format = module_map_format(model, target)?;
    let destination = |value: &BString| -> Result<BString, ResolveError> {
        let ctx = rcmake_genex::EvalContext {
            config: resolved.config.clone(),
            head: Some(crate::usage::handle(resolved.id)),
            ..rcmake_genex::EvalContext::default()
        };
        let mut usage = crate::usage::UsageResolver::new(model);
        let ast = rcmake_genex::parse(value)?;
        Ok(rcmake_genex::evaluate(&ast, &ctx, &mut usage.host(dir))?.0)
    };
    let mut info = TargetDependInfo {
        compiler_id: compiler(b"COMPILER_ID"),
        compiler_frontend_variant: compiler(b"COMPILER_FRONTEND_VARIANT"),
        compiler_simulate_id: compiler(b"SIMULATE_ID"),
        language: "CXX".into(),
        config: resolved.config.0.clone(),
        module_dir: Lexical::new(&root.binary_dir).append(&module_info.module_dir),
        dir_cur_bld: resolved.metadata.binary_dir.clone(),
        dir_cur_src: resolved.metadata.source_dir.clone(),
        dir_top_bld: root.binary_dir.clone(),
        dir_top_src: root.source_dir.clone(),
        linked_target_dirs: module_info.linked_target_dirs.clone(),
        forward_modules_from_target_dirs: module_info.forward_from.clone(),
        ..TargetDependInfo::default()
    };
    if target
        .props
        .get(b"EXPORT_BUILD_DATABASE")
        .or_else(|| definition(model, target.dir, b"CMAKE_EXPORT_BUILD_DATABASE"))
        .is_some_and(|value| rcmake_base::truth::is_on(value))
        && definition(
            model,
            target.dir,
            b"CMAKE_EXPERIMENTAL_EXPORT_BUILD_DATABASE",
        )
        .is_some_and(|value| value == rcmake_diag::EXPORT_BUILD_DATABASE_ACTIVATION)
    {
        let directory = Lexical::new(&module_info.tdi).parent();
        let output = Lexical::new(&root.binary_dir)
            .append(&Lexical::new(directory).append(b"CXX_build_database.json"));
        info.database_info = Some(rcmake_cxxmodules::DatabaseInfo {
            template_path: [output.as_slice(), b".in"].concat().into(),
            output,
        });
    }
    if let Some(rule) = &module_info.bmi_install {
        let destination = destination(&rule.destination)?;
        info.bmi_installation = (!destination.is_empty()).then(|| BmiInstallInfo {
            destination,
            message_level: match rule.message.as_slice() {
                b"ALWAYS" => "MESSAGE_ALWAYS",
                b"LAZY" => "MESSAGE_LAZY",
                b"NEVER" => "MESSAGE_NEVER",
                _ => "",
            }
            .into(),
            permissions: join_words(rule.permissions.iter().cloned()),
            optional: rule.optional,
            script_location: bmi_script_path(
                &root.binary_dir,
                &module_info.tdi,
                &resolved.config.0,
                model.generator == Some(GeneratorKind::NinjaMultiConfig),
            ),
            ..BmiInstallInfo::default()
        });
    }
    let mut steps = Vec::new();
    let mut ddis = Vec::new();
    let mut modmaps = Vec::new();
    let mut includes = IndexSet::new();
    let members: indexmap::IndexMap<_, _> = module_info
        .units
        .iter()
        .map(|unit| (unit.unit, unit))
        .collect();
    for (index, unit) in resolved
        .compile_units
        .iter()
        .enumerate()
        .filter(|(_, unit)| unit.scan)
    {
        let group = resolved.compile_groups.get(unit.group).ok_or_else(|| {
            ResolveError::Message("The C++ module scan has no compile group.".into())
        })?;
        includes.extend(
            group
                .includes
                .iter()
                .map(|include| include.path.value.clone()),
        );
        if let Some(member) = members.get(&index) {
            info.cxx_modules.insert(
                unit.object.clone(),
                ModuleSourceInfo {
                    source: unit.source.value.clone(),
                    file_set: member.file_set.clone(),
                    visibility: member.visibility,
                    bmi_only: member.bmi_only,
                    relative_directory: member.relative_directory.clone(),
                    destination: member.destination.as_ref().map(destination).transpose()?,
                    compile_features: standard
                        .map(|version| [b"cxx_std_".as_slice(), version].concat().into())
                        .into_iter()
                        .collect(),
                    compile_options: group
                        .flags
                        .iter()
                        .map(|entry| entry.value.clone())
                        .collect(),
                    definitions: group
                        .defines
                        .iter()
                        .map(|entry| entry.value.clone())
                        .collect(),
                    include_directories: group
                        .includes
                        .iter()
                        .map(|entry| entry.path.value.clone())
                        .collect(),
                    ..ModuleSourceInfo::default()
                },
            );
        } else {
            info.sources.insert(
                unit.object.clone(),
                ScannedSourceInfo {
                    source: unit.source.value.clone(),
                    language: group.language.clone(),
                },
            );
        }
        let (flags, defines, includes) = compile_arguments(
            model,
            target,
            &resolved.config,
            group,
            standard,
            &unit.source.value,
            position_independent,
        )?;
        let step = scan(
            model,
            target,
            &resolved.config,
            unit,
            &template,
            (join_words(flags), defines, includes),
        )?;
        if let BuildStep::Scan { ddi, .. } = &step {
            ddis.push(ddi.clone());
        }
        steps.push(step);
        modmaps.push(modmap_path(&unit.object));
    }
    info.include_dirs = includes.into_iter().collect();
    let implicit_inputs = std::iter::once(module_info.tdi.clone())
        .chain(
            info.linked_target_dirs
                .iter()
                .chain(&info.forward_modules_from_target_dirs)
                .map(|dir| Lexical::new(dir).append(b"CXXModules.json")),
        )
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect();
    steps.push(BuildStep::Collate {
        lang: "CXX".into(),
        target: resolved.metadata.name.clone(),
        ddis,
        dd: module_info.dyndep.clone(),
        modmaps,
        tdi: module_info.tdi.clone(),
        depend_info: Box::new(crate::DependInfo::Cxx(info)),
        modmap_format: Some(module_map_format),
        implicit_inputs,
    });
    Ok(steps)
}

pub(crate) fn scan_responses(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    standard: Option<&[u8]>,
    position_independent: Option<bool>,
) -> Result<IndexMap<BString, ScanResponse>, ResolveError> {
    let mut responses = IndexMap::new();
    if !matches!(
        model.generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    ) || definition(model, target.dir, b"CMAKE_NINJA_FORCE_RESPONSE_FILE").is_none()
    {
        return Ok(responses);
    }
    let Some(template) = definition(model, target.dir, b"CMAKE_CXX_SCANDEP_SOURCE") else {
        return Ok(responses);
    };
    let prefix = definition(model, target.dir, b"CMAKE_CXX_RESPONSE_FILE_FLAG")
        .map_or(b"@".as_slice(), |value| value.as_slice());
    for unit in resolved.compile_units.iter().filter(|unit| unit.scan) {
        let Some(group) = resolved.compile_groups.get(unit.group) else {
            continue;
        };
        let (flags, defines, includes) = compile_arguments(
            model,
            target,
            &resolved.config,
            group,
            standard,
            &unit.source.value,
            position_independent,
        )?;
        let arguments = [defines, includes, join_words(flags)]
            .iter()
            .filter(|part| !part.is_empty())
            .map(|value| value.as_slice())
            .collect::<Vec<_>>()
            .join(b" ".as_slice())
            .into();
        let rsp = quote(&[unit.object.as_slice(), b".ddi.rsp"].concat());
        let flag = [prefix, &rsp].concat().into();
        if let BuildStep::Scan {
            ddi, invocation, ..
        } = scan(
            model,
            target,
            &resolved.config,
            unit,
            template,
            (flag, BString::default(), BString::default()),
        )? {
            responses.insert(
                ddi,
                ScanResponse {
                    invocation,
                    arguments,
                },
            );
        }
    }
    Ok(responses)
}

fn scan(
    model: &ProjectModel,
    target: &Target,
    config: &rcmake_genex::ConfigId,
    unit: &CompileUnit,
    template: &[u8],
    (flags, defines, includes): (BString, BString, BString),
) -> Result<BuildStep, ResolveError> {
    let object = unit.object.as_slice();
    let ddi: BString = [object, b".ddi"].concat().into();
    let depfile: BString = [object, b".ddi.d"].concat().into();
    let preprocessed: BString = [object, b".ddi.i"].concat().into();
    let commands = rule_invocations(model, target.dir, template, |key| match key {
        b"FLAGS" => Some(flags.clone()),
        b"DEFINES" => Some(defines.clone()),
        b"INCLUDES" => Some(includes.clone()),
        b"SOURCE" => Some(quote(&unit.source.value)),
        b"OBJECT" | b"OBJ_FILE" => Some(quote(object)),
        b"DYNDEP_FILE" | b"DYNDEP_INTERMEDIATE_FILE" => Some(quote(&ddi)),
        b"DEP_FILE" => Some(quote(&depfile)),
        b"PREPROCESSED_SOURCE" | b"PREPROCESSED_OUTPUT_FILE" => Some(quote(&preprocessed)),
        _ => None,
    });
    let invocation = match commands.as_slice() {
        [] => command(":".into()),
        [invocation] => invocation.clone(),
        _ => command(
            commands
                .iter()
                .map(|invocation| invocation.shell.as_slice())
                .collect::<Vec<_>>()
                .join(b" && ".as_slice())
                .into(),
        ),
    };
    let invocation = launcher::Command {
        role: "compile",
        config,
        output: &ddi,
        source: Some(&unit.source.value),
        language: &"CXX".into(),
    }
    .wrap(model, target, invocation);
    Ok(BuildStep::Scan {
        lang: "CXX".into(),
        preprocessed: None,
        source: unit.source.value.clone(),
        ddi,
        object: unit.object.clone(),
        invocation,
        depfile: template
            .windows(b"<DEP_FILE>".len())
            .any(|part| part == b"<DEP_FILE>")
            .then_some(depfile),
    })
}
