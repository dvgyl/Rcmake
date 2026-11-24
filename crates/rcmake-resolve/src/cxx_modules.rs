use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_cxxmodules::ModuleVisibility;
use rcmake_diag::{Backtrace, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path::to_native};
use rcmake_model::{
    BT, DirId, FileSet, FileSetKind, InstallKind, ProjectModel, Target, TargetId, TargetKind,
    TargetOrigin, UsageField, UsageScope,
};

use crate::{
    BmiInstall, CxxModuleInfo, ModuleUnit, ResolveError, ResolvedTarget, lower,
    sources::{self, SourcePaths, TargetInputs, absolute_path},
    target::FileSetMember,
    unity,
};

pub(crate) const STD_MODULE_TARGET: &[u8] = b"@cmake_cxx_std";

fn scan_rule<'a>(model: &'a ProjectModel, target: &Target) -> Option<&'a BString> {
    lower::definition(model, target.dir, b"CMAKE_CXX_SCANDEP_SOURCE")
}

fn missing_scanner(target: &Target) -> ResolveError {
    ResolveError::Message(format!(
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
    ResolveError::Message(body)
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
            "Target \"{name}\" has source file\n\n  {source}\n\nin a \"FILE_SET TYPE CXX_MODULES\" but it is not scheduled for compilation."
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
            "Target \"{name}\" contains the source\n\n  {source}\n\nin a file set of type \"CXX_MODULES\" but the source is not classified as a \"CXX\" source."
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
    if has_module_sets && !allows_module_scanning(model, target, inputs) {
        let standard = cxx_standard(model, target).map_or_else(|| "98".into(), BString::clone);
        return Err(module_error(model, paths, target, format!(
            "The target named \"{}\" has C++ sources that use modules, but does not include \"cxx_std_20\" (or newer) among its `target_compile_features`; found \"cxx_std_{standard}\".",
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
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD_DEFAULT"))
}

fn allows_module_scanning(model: &ProjectModel, target: &Target, inputs: &TargetInputs) -> bool {
    let supports_modules = |standard: &[u8]| matches!(standard, b"20" | b"23" | b"26");
    cxx_standard(model, target).is_some_and(|value| supports_modules(value))
        || inputs
            .standard_requirements
            .get(b"CXX".as_slice())
            .is_some_and(|value| supports_modules(value))
}

pub(crate) fn resolve_module_info(
    model: &ProjectModel,
    target: &Target,
    inputs: &TargetInputs,
    resolved: &mut ResolvedTarget,
    has_module_sets: bool,
    members: &IndexMap<BString, FileSetMember<'_>>,
    unity: Option<&unity::TargetUnity>,
) {
    let allows_scanning = allows_module_scanning(model, target, inputs);
    let by_default = target.policies.policies.get(PolicyId::new(155)) == PolicyStatus::New
        && scan_rule(model, target).is_some();
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
            let bmi_only = target.name == STD_MODULE_TARGET
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
                .map_or(by_default, |value| truth::is_on(value));
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

pub(crate) fn std_module_model(
    model: &ProjectModel,
    host: &dyn Host,
) -> Result<Option<(ProjectModel, TargetId)>, ResolveError> {
    use rcmake_json::{ReadOptions, Value};

    let dir = model.dir(DirId::ROOT);
    let Some(manifest) = lower::definition(model, DirId::ROOT, b"CMAKE_CXX_STDLIB_MODULES_JSON")
        .filter(|path| !truth::is_off(path))
    else {
        return Ok(None);
    };
    let error = |reason: &dyn std::fmt::Display| {
        ResolveError::Message(
            format!(
                "Failed to load C++ standard library modules metadata from \"{manifest}\": {reason}"
            )
            .into(),
        )
    };
    let path = absolute_path(&dir.source_dir, manifest);
    let data = host
        .read(
            &to_native(&path),
            rcmake_host::ReadRange::All {
                limit: rcmake_base::limits::Limits::default().max_value_bytes as u64,
            },
        )
        .map_err(|err| error(&err))?;
    let Value::Object(root) =
        rcmake_json::parse(&data, &ReadOptions::default()).map_err(|err| error(&err))?
    else {
        return Err(error(&"Expected a JSON object."));
    };
    if !matches!(
        root.get(b"version".as_slice()),
        Some(Value::Int(1) | Value::UInt(1))
    ) {
        return Err(error(&"The manifest version must be 1."));
    }
    let Some(Value::Array(entries)) = root.get(b"modules".as_slice()) else {
        return Err(error(&"The manifest has no modules array."));
    };
    let base = Lexical::new(&path).parent();
    let mut sources = Vec::new();
    let mut bases = IndexSet::new();
    let mut target = Target::new(
        STD_MODULE_TARGET.into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        Default::default(),
    );
    for entry in entries {
        let Value::Object(entry) = entry else {
            return Err(error(&"Expected a module object."));
        };
        if !matches!(entry.get(b"logical-name".as_slice()), Some(Value::String(name)) if !name.is_empty())
        {
            return Err(error(&"A module has no logical name."));
        }
        let Some(Value::String(source)) = entry
            .get(b"source-path".as_slice())
            .filter(|value| matches!(value, Value::String(path) if !path.is_empty()))
        else {
            return Err(error(&"A module has no source path."));
        };
        let source = absolute_path(base, source);
        let lexical = Lexical::new(&source);
        bases.insert(BString::from(
            [lexical.root_name(), lexical.root_directory()].concat(),
        ));
        sources.push(BT::new(source, Backtrace::default()));
        if let Some(local) = entry.get(b"local-arguments".as_slice()) {
            std_module_arguments(&mut target, base, local).map_err(|reason| error(&reason))?;
        }
    }
    if sources.is_empty() {
        return Ok(None);
    }
    let mut model = model.clone();
    target.props.set("EXCLUDE_FROM_ALL".into(), "ON".into());
    target.usage.push(
        UsageField::CompileFeatures,
        BT::new("cxx_std_20".into(), Backtrace::default()),
    );
    if lower::definition(&model, DirId::ROOT, b"CMAKE_CXX_COMPILER_ID")
        .is_some_and(|id| id.ends_with(b"Clang"))
    {
        target.usage.push(
            UsageField::CompileOptions,
            BT::new(
                "-Wno-reserved-module-identifier".into(),
                Backtrace::default(),
            ),
        );
    }
    let id = model.add_target(target).map_err(|err| error(&err))?;
    model
        .add_file_set(FileSet {
            target: id,
            name: "CXX_MODULES".into(),
            kind: FileSetKind::CxxModules,
            scope: UsageScope::Public,
            base_dirs: bases
                .into_iter()
                .map(|path| BT::new(path, Backtrace::default()))
                .collect(),
            files: sources,
            props: Default::default(),
        })
        .map_err(|err| error(&err))?;
    Ok(Some((model, id)))
}

fn std_module_arguments(
    target: &mut Target,
    base: &[u8],
    value: &rcmake_json::Value,
) -> Result<(), &'static str> {
    use rcmake_json::Value;

    let Value::Object(local) = value else {
        return Err("Expected a local-arguments object.");
    };
    for key in [
        b"include-directories".as_slice(),
        b"system-include-directories",
    ] {
        if let Some(value) = local.get(key) {
            let Value::Array(dirs) = value else {
                return Err("Expected an array of include paths.");
            };
            for dir in dirs {
                let Value::String(dir) = dir else {
                    return Err("Expected an include path.");
                };
                let path = absolute_path(base, dir);
                target.usage.push(
                    UsageField::IncludeDirectories,
                    BT::new(path, Backtrace::default()),
                );
            }
        }
    }
    if let Some(value) = local.get(b"definitions".as_slice()) {
        let Value::Array(definitions) = value else {
            return Err("Expected an array of definitions.");
        };
        for definition in definitions {
            let Value::Object(definition) = definition else {
                return Err("Expected a definition object.");
            };
            let Some(Value::String(name)) = definition.get(b"name".as_slice()) else {
                return Err("A definition has no name.");
            };
            if matches!(definition.get(b"undef".as_slice()), Some(Value::Bool(true))) {
                continue;
            }
            let value = match definition.get(b"value".as_slice()) {
                Some(Value::String(value)) => [name.as_slice(), b"=", value].concat().into(),
                None => name.clone(),
                _ => return Err("Expected a string definition value."),
            };
            target.usage.push(
                UsageField::CompileDefinitions,
                BT::new(value, Backtrace::default()),
            );
        }
    }
    if let Some(Value::Object(vendor)) = local.get(b"vendor".as_slice())
        && let Some(Value::Object(cmake)) = vendor.get(b"cmake".as_slice())
    {
        for (key, field) in [
            (b"compile-options".as_slice(), UsageField::CompileOptions),
            (b"compile-features".as_slice(), UsageField::CompileFeatures),
        ] {
            if let Some(value) = cmake.get(key) {
                let Value::Array(values) = value else {
                    return Err("Expected an array of compiler arguments.");
                };
                for value in values {
                    let Value::String(value) = value else {
                        return Err("Expected a compiler argument.");
                    };
                    target
                        .usage
                        .push(field, BT::new(value.clone(), Backtrace::default()));
                }
            }
        }
    }
    Ok(())
}
