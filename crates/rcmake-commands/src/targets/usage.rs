use rcmake_base::{BString, list, truth};
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
        let target = cx
            .project()
            .target(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        validate_scope(target, scope, false)?;
        if items.is_empty() {
            continue;
        }
        if let (Some(tables), Some(enabled)) = (&feature_tables, &enabled_languages)
            && scope != UsageScope::Interface
        {
            for feature in items
                .iter()
                .flat_map(|arg| list::split(bytes(arg), list::Empty::Drop))
            {
                let deferred = [b"C".as_slice(), b"CXX", b"CUDA", b"HIP"]
                    .iter()
                    .any(|&lang| {
                        !enabled.iter().any(|value| value == lang)
                            && cx
                                .global()
                                .props
                                .get(&[b"CMAKE_", lang, b"_KNOWN_FEATURES"].concat())
                                .is_some_and(|table| {
                                    list::iter(table, list::Empty::Drop)
                                        .any(|known| feature.as_slice() == known.as_ref())
                                })
                    });
                if !deferred
                    && !feature.windows(2).any(|pair| pair == b"$<")
                    && let Err(message) = cx.project().validate_compile_feature(&feature, |key| {
                        tables
                            .iter()
                            .find(|(name, _)| *name == key)
                            .and_then(|(_, value)| value.as_ref())
                    })
                {
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
        let target = cx
            .project()
            .target_mut(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
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
        if scope != UsageScope::Interface {
            record(target, own, BT::new(value.clone(), bt.clone()), before);
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
    }
    Ok(Flow::Normal)
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
