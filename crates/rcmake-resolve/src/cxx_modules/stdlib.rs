use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{Host, path::to_native};
use rcmake_model::{
    BT, DirId, FileSet, FileSetKind, ProjectModel, Target, TargetId, TargetKind, TargetOrigin,
    UsageField, UsageScope,
};

use super::{STD_MODULE_TARGET, is_std_provider};
use crate::{
    ResolveError, ResolvedTarget, lower,
    sources::absolute_path,
    usage::{UsageResolver, handle},
};

type Providers = IndexMap<TargetId, TargetId>;

struct Manifest {
    target: Target,
    bases: Vec<BT<BString>>,
    sources: Vec<BT<BString>>,
}

fn manifest(model: &ProjectModel, host: &dyn Host) -> Result<Option<Manifest>, ResolveError> {
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
    target.props.set("EXCLUDE_FROM_ALL".into(), "ON".into());
    target.usage.push(
        UsageField::CompileFeatures,
        BT::new("cxx_std_20".into(), Backtrace::default()),
    );
    if lower::definition(model, DirId::ROOT, b"CMAKE_CXX_COMPILER_ID")
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
    let bases = bases
        .into_iter()
        .map(|path| BT::new(path, Backtrace::default()))
        .collect();
    Ok(Some(Manifest {
        target,
        bases,
        sources,
    }))
}

pub(crate) fn providers(
    model: &ProjectModel,
    host: &dyn Host,
    consumers: &IndexMap<TargetId, ResolvedTarget>,
    config: &ConfigId,
) -> Result<Option<(ProjectModel, Providers)>, ResolveError> {
    let mut consumers = consumers
        .iter()
        .filter(|(_, resolved)| resolved.compile_units.iter().any(|unit| unit.scan))
        .peekable();
    if consumers.peek().is_none() {
        return Ok(None);
    }
    let Some(manifest) = manifest(model, host)? else {
        return Ok(None);
    };
    let mut overlay = model.clone();
    let mut usage = UsageResolver::new(model);
    let mut keys = IndexMap::<Key, TargetId>::new();
    let mut bindings = Providers::new();
    for (&id, _) in consumers {
        let Some(target) = model.target(id).filter(|target| !is_std_provider(target)) else {
            continue;
        };
        let context = EvalContext {
            config: config.clone(),
            language: Some("CXX".into()),
            head: Some(handle(id)),
            current: Some(handle(id)),
            ..EvalContext::default()
        };
        let options = usage
            .collect_for_result(id, UsageField::CompileOptions, &context)?
            .entries;
        let features = usage
            .collect_for_result(id, UsageField::CompileFeatures, &context)?
            .entries;
        let (standard, extensions, required) = lower::cxx_standard_mode(model, target, &features);
        let get = |name: &[u8]| lower::definition(model, target.dir, name);
        let runtime = get(b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
            .filter(|default| !default.is_empty())
            .map(|default| {
                let raw = target.props.get(b"MSVC_RUNTIME_LIBRARY").unwrap_or(default);
                usage.eval_ide_property(id, b"MSVC_RUNTIME_LIBRARY", raw, &context)
            })
            .transpose()?;
        let compiler = [
            b"CMAKE_CXX_COMPILER".as_slice(),
            b"CMAKE_CXX_COMPILER_ID",
            b"CMAKE_CXX_COMPILER_VERSION",
            b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT",
            b"CMAKE_CXX_COMPILER_TARGET",
            b"CMAKE_CXX_COMPILER_EXTERNAL_TOOLCHAIN",
        ]
        .iter()
        .map(|name| get(name).cloned())
        .collect();
        let key = Key {
            config: config.0.clone(),
            compiler,
            standard,
            extensions,
            required,
            runtime,
            options: compatibility_options(&options, get(b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT")),
        };
        let provider = if let Some(provider) = keys.get(&key) {
            *provider
        } else {
            let mut provider = manifest.target.clone();
            if !keys.is_empty() {
                provider.name = format!("@cmake_cxx_std_{}", keys.len()).into();
            }
            provider.dir = target.dir;
            provider.policies = target.policies;
            provider
                .props
                .set("CXX_STANDARD".into(), key.standard.clone());
            provider
                .props
                .set("CXX_EXTENSIONS".into(), on_off(key.extensions));
            provider
                .props
                .set("CXX_STANDARD_REQUIRED".into(), on_off(key.required));
            if let Some(runtime) = &key.runtime {
                provider
                    .props
                    .set("MSVC_RUNTIME_LIBRARY".into(), runtime.clone());
            }
            for (field, entries) in [
                (UsageField::CompileOptions, options),
                (UsageField::CompileFeatures, features),
            ] {
                for value in entries {
                    provider.usage.push(field, value);
                }
            }
            let provider = overlay
                .add_target(provider)
                .map_err(|error| ResolveError::Message(error.to_string().into()))?;
            overlay
                .add_file_set(FileSet {
                    target: provider,
                    name: "CXX_MODULES".into(),
                    kind: FileSetKind::CxxModules,
                    scope: UsageScope::Public,
                    base_dirs: manifest.bases.clone(),
                    files: manifest.sources.clone(),
                    props: Default::default(),
                })
                .map_err(|error| ResolveError::Message(error.to_string().into()))?;
            keys.insert(key, provider);
            provider
        };
        bindings.insert(id, provider);
    }
    Ok(Some((overlay, bindings)))
}

#[derive(PartialEq, Eq, Hash)]
struct Key {
    config: BString,
    compiler: Vec<Option<BString>>,
    standard: BString,
    extensions: bool,
    required: bool,
    runtime: Option<BString>,
    options: Vec<BString>,
}

fn on_off(value: bool) -> BString {
    if value { "ON" } else { "OFF" }.into()
}

fn compatibility_options(options: &[BT<BString>], frontend: Option<&BString>) -> Vec<BString> {
    let mut flags = options.iter().map(|option| &option.value);
    let mut compatible = Vec::new();
    while let Some(flag) = flags.next() {
        match classification(flag, frontend) {
            OptionUsage::Hash => compatible.push(flag.clone()),
            OptionUsage::Ignore => {}
            OptionUsage::IgnorePair => {
                flags.next();
            }
        }
    }
    compatible
}

enum OptionUsage {
    Hash,
    Ignore,
    IgnorePair,
}

fn classification(flag: &[u8], frontend: Option<&BString>) -> OptionUsage {
    const MSVC: &[&[u8]] = &[b"D", b"I", b"U"];
    const GNU: &[&[u8]] = &[
        b"D",
        b"I",
        b"U",
        b"isystem",
        b"iquote",
        b"idirafter",
        b"include",
        b"imacros",
        b"-embed-dir",
    ];
    let frontend = frontend
        .map(|frontend| frontend.as_slice())
        .unwrap_or_default();
    let (prefixes, option) = match frontend {
        b"MSVC" => (
            MSVC,
            flag.strip_prefix(b"/").or_else(|| flag.strip_prefix(b"-")),
        ),
        b"GNU" => (GNU, flag.strip_prefix(b"-")),
        _ => return OptionUsage::Hash,
    };
    let Some(option) = option else {
        return OptionUsage::Hash;
    };
    let warning = option.starts_with(b"W") || option.starts_with(b"w");
    let warning = warning
        || if frontend == b"MSVC" {
            option.starts_with(b"external:")
        } else {
            let name = option.strip_prefix(b"-").unwrap_or(option);
            matches!(
                name,
                b"pedantic"
                    | b"pedantic-errors"
                    | b"all-warnings"
                    | b"extra-warnings"
                    | b"no-warnings"
            )
        };
    if warning {
        return OptionUsage::Ignore;
    }
    match prefixes
        .iter()
        .copied()
        .find(|prefix| option.starts_with(prefix))
    {
        Some(prefix) if option == prefix => OptionUsage::IgnorePair,
        Some(_) => OptionUsage::Ignore,
        None => OptionUsage::Hash,
    }
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
