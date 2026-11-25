use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{BT, ModelError, Target, TargetKind, UsageField, UsageScope};

use super::{absolute, bytes, model_error, scope, source_dir, usage_target};

use UsageField as Field;

pub(super) fn include_directories(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(
        cx,
        args,
        Field::IncludeDirectories,
        Field::InterfaceIncludeDirectories,
    )
}

pub(super) fn compile_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(
        cx,
        args,
        Field::CompileDefinitions,
        Field::InterfaceCompileDefinitions,
    )
}

pub(super) fn compile_options(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(
        cx,
        args,
        Field::CompileOptions,
        Field::InterfaceCompileOptions,
    )
}

pub(super) fn compile_features(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(
        cx,
        args,
        Field::CompileFeatures,
        Field::InterfaceCompileFeatures,
    )
}

pub(super) fn link_options(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(cx, args, Field::LinkOptions, Field::InterfaceLinkOptions)
}

pub(super) fn link_directories(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(
        cx,
        args,
        Field::LinkDirectories,
        Field::InterfaceLinkDirectories,
    )
}

fn add(cx: &mut Cx<'_>, args: &[ExpandedArg], own: Field, interface: Field) -> CmdResult {
    let Some((name, mut rest)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    if rest.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    let paths = matches!(own, Field::IncludeDirectories | Field::LinkDirectories);
    let what = own.name().to_ascii_lowercase().replace('_', " ");
    let id = usage_target(cx, &name.value, &what)?;
    let target = cx
        .project()
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if target
        .props
        .get(b"SYMBOLIC")
        .is_some_and(|value| truth::is_on(value))
    {
        return Err(CmdError::status("can not be used on a SYMBOLIC target."));
    }
    if matches!(target.kind, TargetKind::Utility | TargetKind::GlobalTarget) {
        return Err(CmdError::status("called with non-compilable target type"));
    }
    let system = own == Field::IncludeDirectories && take(&mut rest, b"SYSTEM");
    let before = if paths || matches!(own, Field::CompileOptions | Field::LinkOptions) {
        let before = take(&mut rest, b"BEFORE");
        if paths && !before {
            take(&mut rest, b"AFTER");
        }
        before
    } else {
        false
    };
    if rest.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    let dir = if paths {
        source_dir(cx)
    } else {
        BString::default()
    };
    let bt = cx.backtrace();
    // Before CMP0101, target_compile_options() ignores BEFORE for the target's own options.
    let own_before = before
        && (own != Field::CompileOptions || cx.policy(PolicyId::new(101)) == PolicyStatus::New);
    let feature_tables = (own == Field::CompileFeatures).then(|| {
        [
            b"CMAKE_C_COMPILE_FEATURES".as_slice(),
            b"CMAKE_CXX_COMPILE_FEATURES",
            b"CMAKE_CUDA_COMPILE_FEATURES",
            b"CMAKE_HIP_COMPILE_FEATURES",
        ]
        .map(|key| (key, cx.var(key)))
    });
    let enabled_languages = feature_tables.as_ref().map(|_| {
        cx.global()
            .props
            .get(b"ENABLED_LANGUAGES")
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
    });
    while let Some((keyword, tail)) = rest.split_first() {
        let scope = scope(bytes(keyword))
            .ok_or_else(|| CmdError::status("called with invalid arguments"))?;
        let end = tail
            .iter()
            .position(|arg| super::scope(bytes(arg)).is_some())
            .unwrap_or(tail.len());
        let (items, next) = tail.split_at(end);
        rest = next;
        if items.is_empty() {
            continue;
        }
        let target = cx
            .project()
            .target(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        validate_scope(target, scope, false)?;
        if let (Some(tables), Some(enabled)) = (&feature_tables, &enabled_languages)
            && scope != UsageScope::Interface
        {
            for feature in items.iter().map(bytes) {
                if feature.windows(2).any(|pair| pair == b"$<") {
                    continue;
                }
                let known_in = |cx: &mut Cx<'_>, lang: &[u8]| {
                    cx.global()
                        .props
                        .get(&[b"CMAKE_", lang, b"_KNOWN_FEATURES"].concat())
                        .is_some_and(|table| feature_in_table(feature, table))
                };
                let deferred = [b"C".as_slice(), b"CXX", b"CUDA", b"HIP"]
                    .iter()
                    .any(|&lang| !enabled.iter().any(|value| value == lang) && known_in(cx, lang));
                if deferred {
                    continue;
                }
                let result = if feature.starts_with(b"sycl_cxx_std_") && known_in(cx, b"SYCL") {
                    validate_sycl_standard(cx, feature, enabled)
                } else {
                    cx.project().validate_compile_feature(feature, |key| {
                        tables
                            .iter()
                            .find(|(name, _)| *name == key)
                            .and_then(|(_, value)| value.as_ref())
                    })
                };
                if let Err(message) = result {
                    return Err(crate::command_error(
                        cx,
                        format!(
                            "target_compile_features {message} for target \"{}\".",
                            name.value
                        ),
                    ));
                }
            }
        }
        let value = list::join(items.iter().map(|arg| {
            let value = bytes(arg);
            if paths {
                absolute(value, &dir)
            } else if own == Field::CompileDefinitions {
                value.strip_prefix(b"-D").unwrap_or(value).into()
            } else {
                arg.value.clone()
            }
        }));
        let fields = [
            (scope != UsageScope::Interface).then_some(own.name()),
            (system && scope == UsageScope::Private && !value.is_empty())
                .then_some("SYSTEM_INCLUDE_DIRECTORIES"),
            (system && scope != UsageScope::Private)
                .then_some(Field::InterfaceSystemIncludeDirectories.name()),
            (scope != UsageScope::Private).then_some(interface.name()),
        ]
        .into_iter()
        .flatten()
        .map(|field| (field.as_bytes(), rcmake_model::AppendMode::List));
        let origin = rcmake_model::PropOrigin {
            dir: cx.current_dir_id(),
            backtrace: &bt,
        };
        cx.project()
            .update_target_properties(id, fields, origin, |target| {
                if scope != UsageScope::Interface {
                    record(target, own, BT::new(value.clone(), bt.clone()), own_before);
                    if system && scope == UsageScope::Private && !value.is_empty() {
                        target
                            .props
                            .append("SYSTEM_INCLUDE_DIRECTORIES".into(), &value);
                    }
                }
                if scope != UsageScope::Private {
                    let entry = BT::new(value, bt.clone());
                    if system {
                        record(
                            target,
                            Field::InterfaceSystemIncludeDirectories,
                            entry.clone(),
                            false,
                        );
                    }
                    record(target, interface, entry, before);
                }
            })
            .map_err(model_error)?;
    }
    Ok(Flow::Normal)
}

fn feature_in_table(feature: &[u8], table: &[u8]) -> bool {
    list::iter(table, list::Empty::Drop).any(|known| feature == known.as_ref())
}

fn validate_sycl_standard(
    cx: &mut Cx<'_>,
    feature: &[u8],
    enabled: &[BString],
) -> Result<(), BString> {
    if !enabled.iter().any(|lang| lang == b"SYCL") {
        return Ok(());
    }
    if cx
        .var(b"CMAKE_SYCL_COMPILE_FEATURES")
        .is_some_and(|table| feature_in_table(feature, &table))
    {
        return Ok(());
    }
    Err(format!(
        "compiler does not support feature \"{}\"",
        BString::from(feature)
    )
    .into())
}

fn take(args: &mut &[ExpandedArg], token: &[u8]) -> bool {
    if let Some((arg, rest)) = args.split_first()
        && bytes(arg) == token
    {
        *args = rest;
        return true;
    }
    false
}

pub(super) fn validate_scope(
    target: &Target,
    scope: UsageScope,
    sources: bool,
) -> Result<(), CmdError> {
    if scope != UsageScope::Interface {
        if target.kind == TargetKind::InterfaceLibrary && !sources {
            return Err(CmdError::status(
                "may only set INTERFACE properties on INTERFACE targets",
            ));
        }
        if target.is_imported() {
            return Err(CmdError::status(
                "may only set INTERFACE properties on IMPORTED targets",
            ));
        }
    }
    if target.kind == TargetKind::Utility {
        if !sources {
            return Err(CmdError::status("called with non-compilable target type"));
        }
        if scope != UsageScope::Private {
            return Err(CmdError::status(
                "may only set PRIVATE properties on custom targets",
            ));
        }
    }
    Ok(())
}

pub(super) fn record(target: &mut Target, field: Field, entry: BT<BString>, before: bool) {
    if before && !entry.value.is_empty() {
        target
            .usage
            .fields
            .entry(field)
            .or_default()
            .insert(0, entry);
    } else {
        target.usage.push(field, entry);
    }
}
