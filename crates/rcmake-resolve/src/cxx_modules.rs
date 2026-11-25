use indexmap::IndexMap;
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_cxxmodules::ModuleVisibility;
use rcmake_diag::{Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, path::to_native};
use rcmake_model::{
    DirId, FileSetKind, GeneratorKind, InstallKind, ProjectModel, Target, TargetId, UsageScope,
};

use crate::{
    BmiInstall, CxxModuleInfo, ModuleUnit, ResolveError, ResolvedTarget, lower,
    sources::{self, SourcePaths, TargetInputs, absolute_path},
    target::FileSetMember,
    unity,
};

pub(crate) mod stdlib;

pub(crate) const STD_MODULE_TARGET: &[u8] = b"@cmake_cxx_std";

pub(crate) fn is_std_provider(target: &Target) -> bool {
    target.name == STD_MODULE_TARGET
        || target
            .name
            .strip_prefix(b"@cmake_cxx_std_")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.iter().all(u8::is_ascii_digit))
}

fn scan_rule<'a>(model: &'a ProjectModel, target: &Target) -> Option<&'a BString> {
    lower::definition(model, target.dir, b"CMAKE_CXX_SCANDEP_SOURCE")
}

fn generator_supports_scanning(model: &ProjectModel, paths: &SourcePaths<'_>) -> bool {
    match model.generator {
        None | Some(GeneratorKind::VisualStudio(_)) => true,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig) => paths
            .ninja_cxx_dyndep
            .get_or_init(|| ninja_cxx_dyndep(model, paths))
            .unwrap_or(true),
        _ => false,
    }
}

fn ninja_cxx_dyndep(model: &ProjectModel, paths: &SourcePaths<'_>) -> Option<bool> {
    let program = lower::definition(model, DirId::ROOT, b"CMAKE_MAKE_PROGRAM")
        .filter(|program| !truth::is_off(program))?;
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: vec!["--version".into()],
        }],
        cwd: to_native(&model.dir(DirId::ROOT).binary_dir),
        env: paths.host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(std::time::Duration::from_secs(5)),
        tee: false,
    };
    match paths.generator_facts {
        Some(facts) => facts.ninja_cxx_dyndep(paths.host, &request),
        None => query_ninja_cxx_dyndep(paths.host, &request),
    }
}

/// Run the Ninja version query and check for version 1.11 or later.
pub fn query_ninja_cxx_dyndep(host: &dyn Host, request: &RunRequest) -> Option<bool> {
    let result = host.run_bounded(request, 4096).ok()?;
    if result.results != [ProcessResult::Exited(0)] {
        return None;
    }
    let version = rcmake_base::version::Version::parse_strict(
        result
            .stdout
            .split(|byte| !byte.is_ascii_digit() && *byte != b'.')
            .next()?,
    )?;
    Some((version.major(), version.minor()) >= (1, 11))
}

fn missing_scanner(target: &Target) -> ResolveError {
    ResolveError::Reported(format!(
        "The target named \"{}\" has C++ sources that may use modules, but the compiler does not provide a way to discover the import graph dependencies.",
        target.name
    ).into())
}

pub(crate) fn generation_location(model: &ProjectModel, target: &Target) -> Location {
    let dir = &model.dir(target.dir).source_dir;
    Location::FileLine {
        file: absolute_path(dir, b"CMakeLists.txt").as_slice().into(),
        line: 0,
    }
}

pub(crate) fn module_error(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    body: BString,
) -> ResolveError {
    paths.emit(
        model,
        MessageKind::Error,
        generation_location(model, target),
        body.clone(),
    );
    ResolveError::Reported(body)
}

pub(crate) fn validate_module_source(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    source: &[u8],
    language: Option<&BString>,
) -> Result<(), ResolveError> {
    let name = &target.name;
    let source = BString::from(source);
    let cxx_enabled = [b"CMAKE_CXX_COMPILER".as_slice(), b"CMAKE_CXX_COMPILER_ID"]
        .iter()
        .any(|key| lower::definition(model, target.dir, key).is_some());
    let unscheduled = || {
        format!(
            "Target \"{name}\" has source file\n  {source}\nin a \"FILE_SET TYPE CXX_MODULES\" but it is not scheduled for compilation."
        )
    };
    if !cxx_enabled {
        module_error(model, paths, target, format!(
            "The target named \"{name}\" has C++ sources that use modules, but the \"CXX\" language has not been enabled."
        ).into());
        return Err(module_error(model, paths, target, unscheduled().into()));
    }
    match language {
        Some(language) if language == b"CXX" => Ok(()),
        Some(_) => Err(module_error(model, paths, target, format!(
            "Target \"{name}\" contains the source\n  {source}\nin a file set of type \"CXX_MODULES\" but the source is not classified as a \"CXX\" source."
        ).into())),
        None => Err(module_error(model, paths, target, unscheduled().into())),
    }
}

pub(crate) fn validate_module_scanning(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    inputs: &TargetInputs,
    has_module_sets: bool,
) -> Result<(), ResolveError> {
    if !generator_supports_scanning(model, paths) {
        let generator = model.generator.map_or("Ninja", GeneratorKind::name);
        return Err(module_error(
            model,
            paths,
            target,
            format!(
                "The target named \"{}\" has C++ sources that may use modules, but modules are not supported by this generator:\n  {generator}\nModules are supported only by Ninja, Ninja Multi-Config, and Visual Studio generators for VS 17.4 and newer.  See the cmake-cxxmodules(7) manual for details.  Use the CMAKE_CXX_SCAN_FOR_MODULES variable to enable or disable scanning.",
                target.name,
            ).into(),
        ));
    }
    if has_module_sets && !allows_module_scanning(model, target, inputs) {
        let detail = diagnostic_standard(model, target, inputs).map_or_else(
            || "no C++ standard found.".to_owned(),
            |standard| format!("found \"cxx_std_{}\".", BString::from(standard)),
        );
        return Err(module_error(model, paths, target, format!(
            "The target named \"{}\" has C++ sources that use modules, but does not include \"cxx_std_20\" (or newer) among its `target_compile_features`; {detail}",
            target.name,
        ).into()));
    }
    if scan_rule(model, target).is_none() {
        let error = missing_scanner(target);
        paths.emit(
            model, MessageKind::Error, generation_location(model, target),
            format!("{error}  See the cmake-cxxmodules(7) manual for details.  Use the CMAKE_CXX_SCAN_FOR_MODULES variable to enable or disable scanning.").into(),
        );
        return Err(error);
    }
    Ok(())
}

fn cxx_standard<'a>(model: &'a ProjectModel, target: &'a Target) -> Option<&'a BString> {
    target
        .props
        .get(b"CXX_STANDARD")
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD"))
        .filter(|standard| !standard.is_empty())
}

fn diagnostic_standard<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
    inputs: &'a TargetInputs,
) -> Option<&'a [u8]> {
    let default = lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD_DEFAULT")
        .filter(|standard| !standard.is_empty())?;
    let default_extensions = lower::definition(model, target.dir, b"CMAKE_CXX_EXTENSIONS_DEFAULT")
        .is_some_and(|value| truth::is_on(value));
    let new = target.policies.policies.get(PolicyId::new(128)) == PolicyStatus::New;
    let extensions = lower::target_property(model, target, b"CXX_EXTENSIONS")
        .map_or(!new || default_extensions, |value| truth::is_on(value));
    let standard = inputs
        .standard_requirements
        .get(b"CXX".as_slice())
        .copied()
        .or_else(|| cxx_standard(model, target).map(|value| value.as_slice()));
    if let Some(standard) = standard {
        let required = lower::target_property(model, target, b"CXX_STANDARD_REQUIRED")
            .is_some_and(|value| truth::is_on(value));
        return (required
            || !new
            || extensions != default_extensions
            || standard != default.as_slice())
        .then_some(standard);
    }
    (if new {
        extensions != default_extensions
    } else {
        extensions
    })
    .then_some(default.as_slice())
}

fn allows_module_scanning(model: &ProjectModel, target: &Target, inputs: &TargetInputs) -> bool {
    let supports_modules = |standard: &[u8]| matches!(standard, b"20" | b"23" | b"26");
    cxx_standard(model, target).is_some_and(|value| supports_modules(value))
        || inputs
            .standard_requirements
            .get(b"CXX".as_slice())
            .is_some_and(|value| supports_modules(value))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Module resolution uses source and target facts."
)]
pub(crate) fn resolve_module_info(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    inputs: &TargetInputs,
    resolved: &mut ResolvedTarget,
    has_module_sets: bool,
    members: &IndexMap<BString, FileSetMember<'_>>,
    unity: Option<&unity::TargetUnity>,
) {
    let allows_scanning = allows_module_scanning(model, target, inputs);
    let by_default = || {
        target.policies.policies.get(PolicyId::new(155)) == PolicyStatus::New
            && scan_rule(model, target).is_some()
            && generator_supports_scanning(model, paths)
    };
    let target_setting = target
        .props
        .get(b"CXX_SCAN_FOR_MODULES")
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_SCAN_FOR_MODULES"));
    let dir = model.dir(target.dir);
    let mut destinations = IndexMap::new();
    let mut units = Vec::new();
    for (index, unit) in resolved.compile_units.iter_mut().enumerate() {
        if unity.is_some_and(|plan| plan.files.contains_key(&unit.source.value)) {
            continue;
        }
        let member = members.get(&unit.source.value);
        if let Some(member) = member.filter(|member| member.set.kind == FileSetKind::CxxModules) {
            let visibility = match member.set.scope {
                UsageScope::Private => ModuleVisibility::Private,
                UsageScope::Public => ModuleVisibility::Public,
                UsageScope::Interface => ModuleVisibility::Interface,
            };
            let bmi_only = is_std_provider(target)
                && lower::definition(model, target.dir, b"CMAKE_CXX_COMPILER_ID")
                    .is_none_or(|id| id != b"MSVC".as_slice());
            units.push(ModuleUnit {
                unit: index,
                file_set: member.set.name.clone(),
                public: visibility != ModuleVisibility::Private,
                visibility,
                relative_directory: member.relative_directory.clone(),
                destination: destinations
                    .entry(&member.set.name)
                    .or_insert_with(|| {
                        module_install(model, resolved.id, &resolved.config, Some(&member.set.name))
                            .map(|install| install.destination)
                    })
                    .clone(),
                bmi_only,
            });
            if bmi_only {
                unit.object.extend_from_slice(b".bmi");
            }
            unit.scan = true;
            continue;
        }
        if allows_scanning
            && resolved
                .compile_groups
                .get(unit.group)
                .is_some_and(|group| group.language == "CXX")
        {
            unit.scan = member
                .and_then(|member| member.set.props.get(b"CXX_SCAN_FOR_MODULES"))
                .or_else(|| {
                    sources::properties(dir, &unit.source.value).get(b"CXX_SCAN_FOR_MODULES")
                })
                .or(target_setting)
                .map_or_else(by_default, |value| truth::is_on(value));
        }
    }
    if has_module_sets || resolved.compile_units.iter().any(|unit| unit.scan) {
        let module_dir = resolved.artifacts.object_dir.clone().unwrap_or_default();
        let directory = Lexical::new(&module_dir);
        resolved.cxx_modules = Some(CxxModuleInfo {
            units,
            tdi: directory.append(b"CXXDependInfo.json"),
            dyndep: directory.append(b"CXX.dd"),
            module_dir,
            bmi_install: module_install(model, resolved.id, &resolved.config, None),
            ..CxxModuleInfo::default()
        });
    }
}

fn module_install(
    model: &ProjectModel,
    id: TargetId,
    config: &ConfigId,
    file_set: Option<&[u8]>,
) -> Option<BmiInstall> {
    model.installs.iter().enumerate().find_map(|(index, rule)| {
        let InstallKind::Targets { targets, .. } = &rule.kind else {
            return None;
        };
        if !targets.contains(&id) {
            return None;
        }
        let prefix = format!("_RCMAKE_INSTALL_{index}_ARG");
        let mut args = (0..)
            .map_while(|i| {
                model
                    .global
                    .props
                    .get(format!("{prefix}{i}").as_bytes())
                    .map(|value| value.as_slice())
            })
            .peekable();
        if let Some(name) = file_set {
            loop {
                args.find(|arg| *arg == b"FILE_SET")?;
                if args.next()? == name {
                    break;
                }
            }
        } else if args.find(|arg| *arg == b"CXX_MODULES_BMI").is_none()
            && rule.options.destination.is_none()
        {
            return None;
        }
        let options = &rule.options;
        let mut install = BmiInstall {
            component: options
                .component
                .clone()
                .or_else(|| {
                    lower::definition(model, rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                        .cloned()
                })
                .unwrap_or_else(|| "Unspecified".into()),
            permissions: options.permissions.clone(),
            exclude_from_all: options.exclude_from_all,
            optional: options.optional,
            message: model
                .global
                .props
                .get(format!("_RCMAKE_INSTALL_{index}_MESSAGE").as_bytes())
                .cloned()
                .unwrap_or_default(),
            ..BmiInstall::default()
        };
        let mut has_destination = options.destination.is_some();
        if has_destination {
            install.destination = options.destination.clone().unwrap_or_default();
        }
        let mut configs = options.configurations.clone();
        let is_keyword = |arg: &[u8]| {
            matches!(
                arg,
                b"ARCHIVE"
                    | b"LIBRARY"
                    | b"RUNTIME"
                    | b"FRAMEWORK"
                    | b"BUNDLE"
                    | b"OBJECTS"
                    | b"PUBLIC_HEADER"
                    | b"PRIVATE_HEADER"
                    | b"RESOURCE"
                    | b"FILE_SET"
                    | b"CXX_MODULES_BMI"
                    | b"INCLUDES"
                    | b"DESTINATION"
                    | b"COMPONENT"
                    | b"PERMISSIONS"
                    | b"OPTIONAL"
                    | b"EXCLUDE_FROM_ALL"
                    | b"CONFIGURATIONS"
            )
        };
        while let Some(arg) = args.next() {
            match arg {
                b"DESTINATION" => {
                    let destination = args.next()?;
                    install.destination = if destination.is_empty() {
                        BString::default()
                    } else {
                        Lexical::new(destination).normalize()
                    };
                    has_destination = true;
                }
                b"COMPONENT" => install.component = args.next()?.into(),
                b"OPTIONAL" => install.optional = true,
                b"EXCLUDE_FROM_ALL" => install.exclude_from_all = true,
                b"PERMISSIONS" | b"CONFIGURATIONS" => {
                    let values = std::iter::from_fn(|| args.next_if(|arg| !is_keyword(arg)))
                        .map(BString::from)
                        .collect();
                    if arg == b"PERMISSIONS" {
                        install.permissions = values;
                    } else {
                        configs = values;
                    }
                }
                _ => break,
            }
        }
        (has_destination
            && !install.destination.is_empty()
            && (configs.is_empty() || configs.iter().any(|name| config.matches(name))))
        .then_some(install)
    })
}
