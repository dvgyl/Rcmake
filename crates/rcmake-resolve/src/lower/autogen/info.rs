//! CMake 4.5.1 metadata for the cmake_autogen and cmake_autorcc commands.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, ReadRange, WriteMode, path::to_native};
use rcmake_json::{Value, write_styled};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, UsageField};

use super::graph::target_list;
use super::scan::{ConfigValues, HEADER_EXTENSIONS, QtVersion, ScanKind, eval_property};
use super::{ResourceJob, TargetPlan, concatenate, named};
use crate::lower::{definition, join};
use crate::sources::absolute_path;
use crate::usage::UsageResolver;
use crate::usage::link_options::shell_arguments;
use crate::{CompileGroup, ResolveError, ResolvedTarget};

type Object = BTreeMap<BString, Value>;

const DEFAULT_MOC_MACROS: &[&[u8]] = &[
    b"Q_OBJECT",
    b"Q_GADGET",
    b"Q_NAMESPACE",
    b"Q_NAMESPACE_EXPORT",
];
const PLUGIN_METADATA_FILTER: &[u8] =
    b"[\n][ \t]*Q_PLUGIN_METADATA[ \t]*\\([^\\)]*FILE[ \t]*\"([^\"]+)\"";

/// Serialize the metadata for each planned target and resource manifest.
pub(super) fn write_info(
    host: &dyn Host,
    model: &ProjectModel,
    plans: &[TargetPlan],
    resolved: &[ResolvedTarget],
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model);
    for plan in plans {
        let target = model
            .target(plan.id)
            .ok_or_else(|| ResolveError::Message("The AUTOGEN origin target is missing.".into()))?;
        if plan.moc_or_uic() {
            let data = autogen_info(model, target, plan, resolved, &mut usage)?;
            write_if_changed(host, &plan.info_path(), data)?;
        }
        for job in &plan.resources {
            write_if_changed(host, &job.info_path(), rcc_info(model, target, plan, job))?;
        }
    }
    Ok(())
}

fn common(model: &ProjectModel, target: &Target, plan: &TargetPlan) -> Object {
    let root = model.dir(DirId::ROOT);
    let dir = model.dir(target.dir);
    let mut info = BTreeMap::new();
    for (key, value) in [
        ("MULTI_CONFIG", plan.multi),
        ("CROSS_CONFIG", plan.cross),
        ("USE_BETTER_GRAPH", plan.better),
    ] {
        info.insert(key.into(), Value::Bool(value));
    }
    info.insert("VERBOSITY".into(), Value::UInt(plan.verbosity.into()));
    for (key, value) in [
        ("CMAKE_SOURCE_DIR", &root.source_dir),
        ("CMAKE_BINARY_DIR", &root.binary_dir),
        ("CMAKE_CURRENT_SOURCE_DIR", &dir.source_dir),
        ("CMAKE_CURRENT_BINARY_DIR", &dir.binary_dir),
        ("BUILD_DIR", &plan.build_dir),
    ] {
        info.insert(key.into(), Value::string(value.clone()));
    }
    config_paths(&mut info, "INCLUDE_DIR", plan, |config| {
        plan.include_dir(config)
    });
    info
}

fn autogen_info(
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    resolved: &[ResolvedTarget],
    usage: &mut UsageResolver<'_>,
) -> Result<Object, ResolveError> {
    let mut info = common(model, target, plan);
    let dir = model.dir(target.dir);
    info.insert("PARALLEL".into(), Value::UInt(plan.parallel.into()));
    if let Some(limit) = plan.command_length {
        info.insert(
            "AUTOGEN_COMMAND_LINE_LENGTH_MAX".into(),
            Value::UInt(limit.into()),
        );
    }
    info.insert(
        "QT_VERSION_MAJOR".into(),
        Value::UInt(plan.version.0.into()),
    );
    info.insert(
        "QT_VERSION_MINOR".into(),
        Value::UInt(plan.version.1.into()),
    );
    info.insert("CMAKE_EXECUTABLE".into(), Value::string(plan.cmake.clone()));
    config_values(
        &mut info,
        "QT_MOC_EXECUTABLE",
        &plan.moc.executable,
        value_string,
    );
    config_values(
        &mut info,
        "QT_UIC_EXECUTABLE",
        &plan.uic.executable,
        value_string,
    );
    for (key, stem) in [
        ("SETTINGS_FILE", b"AutogenUsed".as_slice()),
        ("PARSE_CACHE_FILE", b"ParseCache"),
    ] {
        let base = join(&plan.info_dir, stem);
        config_paths(&mut info, key, plan, |config| named(&base, config, b".txt"));
    }
    for (key, is_rule) in [("DEP_FILE", false), ("DEP_FILE_RULE_NAME", true)] {
        let path = |config| {
            if is_rule {
                let stamp = plan.stamp(config);
                Lexical::new(&stamp)
                    .relative(Lexical::new(&dir.binary_dir))
                    .unwrap_or(stamp)
            } else {
                plan.depfile(config)
            }
        };
        let mut dep = ConfigValues::default();
        if plan.use_stamp {
            if plan.multi && plan.better {
                dep.per_config.extend(
                    plan.configs
                        .iter()
                        .map(|config| (config.clone(), path(Some(&config.0)))),
                );
            } else {
                dep.default = path(None);
            }
        }
        config_values(&mut info, key, &dep, value_string);
    }
    // The overlay adds registered resource manifests to the directory listfiles.
    info.insert("CMAKE_LIST_FILES".into(), strings(&dir.listfiles));
    info.insert(
        "HEADER_EXTENSIONS".into(),
        bytes(HEADER_EXTENSIONS.iter().copied()),
    );
    add_scan_rows(&mut info, plan);
    if plan.moc.enabled {
        add_moc_info(&mut info, model, target, plan, resolved, usage)?;
    }
    if plan.uic.enabled {
        add_uic_info(&mut info, model, target, plan, usage)?;
    }
    Ok(info)
}

fn add_scan_rows(info: &mut Object, plan: &TargetPlan) {
    for (key, kind) in [
        ("HEADERS", ScanKind::Header),
        ("SOURCES", ScanKind::Source),
        ("CXX_MODULE_UNITS", ScanKind::Module),
    ] {
        let rows = plan
            .scan
            .files
            .iter()
            .filter(|(_, row)| row.kind == kind)
            .map(|(path, row)| {
                let flags = [
                    if row.moc { b'M' } else { b'm' },
                    if row.uic { b'U' } else { b'u' },
                ];
                let configs = if row.configs.is_empty()
                    || plan
                        .configs
                        .iter()
                        .all(|config| row.configs.contains(config))
                {
                    Value::Null
                } else {
                    bytes(
                        plan.configs
                            .iter()
                            .filter(|config| row.configs.contains(*config))
                            .map(|config| config.0.as_slice()),
                    )
                };
                let mut fields = vec![value_string(path), Value::string(flags.as_slice())];
                match kind {
                    ScanKind::Header => fields.extend([value_string(&row.moc_output), configs]),
                    ScanKind::Source => fields.push(configs),
                    ScanKind::Module => fields.extend([configs, value_string(&row.moc_output)]),
                }
                Value::Array(fields)
            });
        info.insert(key.into(), Value::Array(rows.collect()));
    }
}

fn add_moc_info(
    info: &mut Object,
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    resolved: &[ResolvedTarget],
    usage: &mut UsageResolver<'_>,
) -> Result<(), ResolveError> {
    info.insert(
        "MOC_SKIP".into(),
        bytes(plan.scan.moc_skip.iter().map(|value| value.as_slice())),
    );
    info.insert(
        "MOC_OPTIONS".into(),
        strings(&target_list(target, b"AUTOMOC_MOC_OPTIONS")),
    );
    for (field, value) in [
        (
            "MOC_RELAXED_MODE",
            definition(model, target.dir, b"CMAKE_AUTOMOC_RELAXED_MODE"),
        ),
        ("MOC_PATH_PREFIX", target.props.get(b"AUTOMOC_PATH_PREFIX")),
    ] {
        let enabled = value.is_some_and(|value| truth::is_on(value));
        info.insert(field.into(), Value::Bool(enabled));
    }

    let macros = if let Some(value) = target.props.get(b"AUTOMOC_MACRO_NAMES") {
        list::split(value, list::Empty::Drop)
            .into_iter()
            .collect::<IndexSet<_>>()
    } else {
        DEFAULT_MOC_MACROS
            .iter()
            .copied()
            .map(BString::from)
            .collect()
    };
    let default = ConfigId::default();
    let configs = if plan.configs.is_empty() {
        std::slice::from_ref(&default)
    } else {
        &plan.configs
    };
    let mut macros = macros;
    let mut definitions = ConfigValues::default();
    let mut includes = ConfigValues::default();
    let predefs = predefs_command(model, target, plan, usage)?;
    for config in configs {
        let context = cxx_context(config);
        let group = compile_group(resolved, plan, config);
        let config_definitions = moc_definitions(
            model,
            target,
            plan,
            usage,
            &context,
            group,
            predefs.is_empty(),
        )?;
        set_usage(
            &mut definitions,
            plan.multi,
            config,
            config_definitions,
            false,
        );
        let (config_includes, explicit) =
            moc_includes(model, target, plan, usage, &context, group)?;
        set_usage(&mut includes, plan.multi, config, config_includes, explicit);
        for id in usage.reachable_link_targets(plan.id, &context)? {
            let Some(link) = model.target(id) else {
                continue;
            };
            let Some(raw) = link.props.get(b"INTERFACE_AUTOMOC_MACRO_NAMES") else {
                continue;
            };
            let value = eval_property(
                usage,
                id,
                link,
                config,
                b"INTERFACE_AUTOMOC_MACRO_NAMES",
                raw,
                &mut IndexSet::new(),
            )?;
            macros.extend(list::split(&value, list::Empty::Drop));
        }
    }
    info.insert(
        "MOC_MACRO_NAMES".into(),
        bytes(macros.iter().map(|value| value.as_slice())),
    );
    config_values(info, "MOC_DEFINITIONS", &definitions, |values| {
        strings(values)
    });
    config_values(info, "MOC_INCLUDES", &includes, |values| strings(values));

    let filters = target_list(target, b"AUTOMOC_DEPEND_FILTERS");
    if !filters.len().is_multiple_of(2) {
        return Err(ResolveError::Message(
            "AUTOMOC_DEPEND_FILTERS must contain keyword and regular expression pairs.".into(),
        ));
    }
    let mut dependencies = vec![bytes([
        b"Q_PLUGIN_METADATA".as_slice(),
        PLUGIN_METADATA_FILTER,
    ])];
    dependencies.extend(filters.chunks(2).map(strings));
    info.insert("MOC_DEPEND_FILTERS".into(), Value::Array(dependencies));
    if plan.generator == GeneratorKind::Xcode {
        info.insert(
            "MOC_COMPILATION_FILE".into(),
            Value::string(plan.compilation_file(None)),
        );
    } else {
        config_paths(info, "MOC_COMPILATION_FILE", plan, |config| {
            plan.compilation_file(config)
        });
    }
    if predefs.is_empty() {
        info.insert("MOC_PREDEFS_FILE".into(), Value::string(""));
    } else {
        let base = join(&plan.build_dir, b"moc_predefs");
        config_paths(info, "MOC_PREDEFS_FILE", plan, |config| {
            named(&base, config, b".h")
        });
    }
    info.insert("MOC_PREDEFS_CMD".into(), strings(&predefs));
    Ok(())
}

fn cxx_context(config: &ConfigId) -> EvalContext {
    EvalContext {
        config: config.clone(),
        language: Some("CXX".into()),
        ..EvalContext::default()
    }
}

fn compile_group<'a>(
    resolved: &'a [ResolvedTarget],
    plan: &TargetPlan,
    config: &ConfigId,
) -> Option<&'a CompileGroup> {
    let target = resolved
        .iter()
        .find(|target| target.id == plan.id && &target.config == config)?;
    let source = plan.compilation_file(plan.multi.then_some(config.0.as_slice()));
    target
        .compile_units
        .iter()
        .find(|unit| unit.source.value == source)
        .and_then(|unit| target.compile_groups.get(unit.group))
        .or_else(|| {
            target
                .compile_groups
                .iter()
                .find(|group| group.language == b"CXX")
        })
}

fn moc_definitions(
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    usage: &mut UsageResolver<'_>,
    context: &EvalContext,
    group: Option<&CompileGroup>,
    no_predefs: bool,
) -> Result<Vec<BString>, ResolveError> {
    let mut defs: BTreeSet<_> = if let Some(group) = group {
        group
            .defines
            .iter()
            .map(|entry| entry.value.clone())
            .collect()
    } else {
        usage
            .collect(plan.id, UsageField::CompileDefinitions, context)?
            .entries
            .into_iter()
            .map(|entry| entry.value)
            .collect()
    };
    let get = |name: &[u8]| definition(model, target.dir, name);
    if get(b"CMAKE_SYSTEM_NAME").is_some_and(|name| name == b"Windows")
        && (no_predefs || get(b"MSVC").is_some_and(|value| truth::is_on(value)))
    {
        defs.insert("WIN32".into());
    }
    Ok(defs.into_iter().collect())
}

fn moc_includes(
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    usage: &mut UsageResolver<'_>,
    context: &EvalContext,
    group: Option<&CompileGroup>,
) -> Result<(Vec<BString>, bool), ResolveError> {
    let property = b"AUTOMOC_INCLUDE_DIRECTORIES";
    if let Some(raw) = target.props.get(property) {
        let value = eval_property(
            usage,
            plan.id,
            target,
            &context.config,
            property,
            raw,
            &mut IndexSet::new(),
        )?;
        let includes = list::split(&value, list::Empty::Drop);
        if let Some(path) = includes.iter().find(|path| !Lexical::new(path).absolute()) {
            return Err(ResolveError::Message(
                format!("AUTOMOC_INCLUDE_DIRECTORIES: path '{path}' is not absolute.").into(),
            ));
        }
        return Ok((includes, true));
    }
    let generated = plan.include_dir(plan.multi.then_some(context.config.0.as_slice()));
    let generated_base = plan.include_dir(None);
    let includes = if let Some(group) = group {
        group.includes.clone()
    } else {
        usage.collect_includes(plan.id, context)?
    };
    let mut paths: IndexSet<BString> = includes
        .into_iter()
        .map(|entry| entry.path.value)
        .filter(|value| value != &generated && value != &generated_base)
        .collect();
    if plan.version.0 >= 5
        && let Some(implicit) =
            definition(model, target.dir, b"CMAKE_CXX_IMPLICIT_INCLUDE_DIRECTORIES")
    {
        paths.extend(list::split(implicit, list::Empty::Drop));
    }
    Ok((paths.into_iter().collect(), false))
}

fn predefs_command(
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    let enabled = target
        .props
        .get(b"AUTOMOC_COMPILER_PREDEFINES")
        .is_none_or(|value| truth::is_on(value));
    if !enabled || plan.version < QtVersion(5, 8) {
        return Ok(Vec::new());
    }
    let Some(raw) = definition(model, target.dir, b"CMAKE_CXX_COMPILER_PREDEFINES_COMMAND") else {
        return Ok(Vec::new());
    };
    let mut command = list::split(raw, list::Empty::Drop);
    let args = definition(model, target.dir, b"CMAKE_CXX_COMPILER_ARG1")
        .map_or(1, |value| 1 + shell_arguments(value).len());
    if args < command.len() {
        let default = ConfigId::default();
        let config = if plan.multi {
            &default
        } else {
            plan.configs.first().unwrap_or(&default)
        };
        let features = usage.collect(plan.id, UsageField::CompileFeatures, &cxx_context(config))?;
        let standard = crate::lower::required_standard(b"CXX", &features.entries);
        if let Some(flag) = crate::lower::standard::standard_flag(model, target, b"CXX", standard) {
            command.splice(args..args, shell_arguments(&flag));
        }
    }
    Ok(command)
}

fn add_uic_info(
    info: &mut Object,
    model: &ProjectModel,
    target: &Target,
    plan: &TargetPlan,
    usage: &mut UsageResolver<'_>,
) -> Result<(), ResolveError> {
    let dir = model.dir(target.dir);
    let search: Vec<_> = target_list(target, b"AUTOUIC_SEARCH_PATHS")
        .iter()
        .map(|path| absolute_path(&dir.source_dir, path))
        .collect();
    info.insert("UIC_SEARCH_PATHS".into(), strings(&search));
    info.insert(
        "UIC_SKIP".into(),
        bytes(plan.scan.uic_skip.iter().map(|path| path.as_slice())),
    );
    let files = plan
        .scan
        .ui
        .iter()
        .filter(|(_, options)| !options.is_empty())
        .map(|(path, options)| Value::Array(vec![value_string(path), strings(options)]))
        .collect();
    info.insert("UIC_UI_FILES".into(), Value::Array(files));

    let default = ConfigId::default();
    let get =
        |config: &ConfigId, usage: &mut UsageResolver<'_>| -> Result<Vec<BString>, ResolveError> {
            let mut opts = Vec::new();
            for id in usage.reachable_link_targets(plan.id, &cxx_context(config))? {
                let Some(dep) = model.target(id) else {
                    continue;
                };
                if let Some(raw) = dep.props.get(b"INTERFACE_AUTOUIC_OPTIONS") {
                    let value = eval_property(
                        usage,
                        id,
                        dep,
                        config,
                        b"INTERFACE_AUTOUIC_OPTIONS",
                        raw,
                        &mut IndexSet::new(),
                    )?;
                    super::scan::merge_options(
                        &mut opts,
                        list::split(&value, list::Empty::Drop),
                        &[
                            b"tr",
                            b"translate",
                            b"postfix",
                            b"generator",
                            b"include",
                            b"g",
                        ],
                        plan.version.0 >= 5,
                    );
                }
            }
            if let Some(raw) = target.props.get(b"AUTOUIC_OPTIONS") {
                let value = eval_property(
                    usage,
                    plan.id,
                    target,
                    config,
                    b"AUTOUIC_OPTIONS",
                    raw,
                    &mut IndexSet::new(),
                )?;
                super::scan::merge_options(
                    &mut opts,
                    list::split(&value, list::Empty::Drop),
                    &[
                        b"tr",
                        b"translate",
                        b"postfix",
                        b"generator",
                        b"include",
                        b"g",
                    ],
                    plan.version.0 >= 5,
                );
            }
            Ok(opts)
        };
    let config = plan.configs.first().unwrap_or(&default);
    let mut values = ConfigValues {
        default: get(config, usage)?,
        per_config: Default::default(),
    };
    if plan.multi {
        for config in &plan.configs {
            let options = get(config, usage)?;
            set_usage(&mut values, true, config, options, false);
        }
    }
    config_values(info, "UIC_OPTIONS", &values, |values| strings(values));
    Ok(())
}

fn rcc_info(model: &ProjectModel, target: &Target, plan: &TargetPlan, job: &ResourceJob) -> Object {
    let mut info = common(model, target, plan);
    for (key, value) in [
        ("GENERATOR", plan.generator.name().into()),
        ("LOCK_FILE", concatenate(&[&job.prefix, b"_Lock.lock"])),
        ("SOURCE", job.source.clone()),
        ("OUTPUT_CHECKSUM", job.checksum.clone()),
        ("OUTPUT_NAME", Lexical::new(&job.output).filename().into()),
    ] {
        info.insert(key.into(), Value::string(value));
    }
    let settings = concatenate(&[&job.prefix, b"_Used"]);
    config_paths(&mut info, "SETTINGS_FILE", plan, |config| {
        named(&settings, config, b".txt")
    });
    config_values(
        &mut info,
        "RCC_EXECUTABLE",
        &plan.rcc.executable,
        value_string,
    );
    config_values(
        &mut info,
        "RCC_LIST_OPTIONS",
        &plan.rcc.list_options,
        |value| strings(value),
    );
    config_values(&mut info, "INPUTS", &job.inputs, |value| strings(value));
    info.insert("OPTIONS".into(), strings(&job.options));
    info
}

fn config_paths(
    info: &mut Object,
    key: &str,
    plan: &TargetPlan,
    path: impl Fn(Option<&[u8]>) -> BString,
) {
    info.insert(key.into(), Value::string(path(None)));
    if plan.multi {
        for config in &plan.configs {
            info.insert(
                config_key(key, config),
                Value::string(path(Some(&config.0))),
            );
        }
    }
}

fn config_values<T>(
    info: &mut Object,
    key: &str,
    values: &ConfigValues<T>,
    value: impl Fn(&T) -> Value,
) {
    info.insert(key.into(), value(&values.default));
    for (config, entry) in &values.per_config {
        info.insert(config_key(key, config), value(entry));
    }
}

fn config_key(key: &str, config: &ConfigId) -> BString {
    concatenate(&[key.as_bytes(), b"_", &config.0])
}

fn set_usage<T: PartialEq>(
    values: &mut ConfigValues<T>,
    multi: bool,
    config: &ConfigId,
    entry: T,
    force: bool,
) {
    if multi {
        if force || entry != values.default {
            values.per_config.insert(config.clone(), entry);
        }
    } else {
        values.default = entry;
    }
}

fn value_string(value: &BString) -> Value {
    Value::string(value.clone())
}

fn strings(values: &[BString]) -> Value {
    Value::Array(values.iter().map(value_string).collect())
}

fn bytes<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> Value {
    Value::Array(values.into_iter().map(Value::string).collect())
}

fn write_if_changed(host: &dyn Host, path: &BString, info: Object) -> Result<(), ResolveError> {
    let mut data = write_styled(&Value::Object(info), "\t");
    data.push(b'\n');
    let file = to_native(path);
    let limit = u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if host
        .read(&file, ReadRange::All { limit })
        .is_ok_and(|old| old == data.as_slice())
    {
        return Ok(());
    }
    let error = |error: &dyn std::fmt::Display| {
        ResolveError::Message(format!("Cannot write AUTOGEN metadata {path}. {error}").into())
    };
    if let Some(dir) = file.parent() {
        host.create_dir_all(dir).map_err(|err| error(&err))?;
    }
    let pid = host.system().pid;
    for attempt in 0..32 {
        let suffix = format!(".{pid}.{attempt}.tmp");
        let temp = to_native(&concatenate(&[path, suffix.as_bytes()]));
        match host.write(&temp, &data, WriteMode::CreateNew) {
            Ok(()) => {
                if let Err(err) = host.rename(&temp, &file) {
                    let _ = host.remove_file(&temp);
                    return Err(error(&err));
                }
                return Ok(());
            }
            Err(err) if err.kind == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(error(&err)),
        }
    }
    Err(error(&"No temporary filename is available."))
}
