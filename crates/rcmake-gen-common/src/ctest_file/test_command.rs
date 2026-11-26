use rcmake_genex::{EvalContext, GenexNode, evaluate, parse};

use super::{
    BString, DirId, DiscoverySpec, GenError, Host, Lexical, Location, PolicyId, PolicyStatus,
    ProjectModel, Target, TargetKind, TestEntry, UsageResolver, list, truth, warn_policy,
};

pub(super) fn resolve_discovery(
    spec: &mut DiscoverySpec,
    mut evaluate: impl FnMut(&[u8]) -> Result<BString, GenError>,
) -> Result<(), GenError> {
    for value in spec
        .discovery_args
        .iter_mut()
        .chain([&mut spec.discovery_match, &mut spec.test_name])
        .chain(&mut spec.test_args)
        .chain(spec.discovery_props.iter_mut().map(|(_, value)| value))
        .chain(spec.test_props.iter_mut().map(|(_, value)| value))
    {
        *value = evaluate(value)?;
    }
    Ok(())
}

pub(super) fn resolve_test(
    test: &TestEntry,
    mut resolve: impl FnMut(&[u8]) -> Result<BString, GenError>,
) -> Result<TestEntry, GenError> {
    let mut resolved = test.clone();
    resolved.command.clear();
    for arg in &test.command {
        let value = resolve(arg)?;
        if test.command_expand_lists {
            resolved
                .command
                .extend(list::split(&value, list::Empty::Keep));
        } else {
            resolved.command.push(value);
        }
    }
    for name in test.props.names() {
        if name != b"_BACKTRACE_TRIPLES"
            && let Some(value) = test.props.get(&name)
        {
            resolved.props.set(name, resolve(value)?);
        }
    }
    if test.props.get(b"WORKING_DIRECTORY").is_none()
        && let Some(value) = &test.working_directory
    {
        resolved.working_directory = Some(resolve(value)?);
    }
    Ok(resolved)
}

pub(super) fn resolve_test_program<'a>(
    model: &ProjectModel,
    test: &mut TestEntry,
    lookup: impl Fn(&[u8]) -> Option<(&'a Target, Option<BString>)>,
    mut evaluate: impl FnMut(&[u8]) -> Result<BString, GenError>,
    host: Option<&dyn Host>,
) -> Result<(), GenError> {
    let Some(program) = test.command.first_mut() else {
        test.command.push(BString::default());
        return Ok(());
    };
    let Some((target, Some(file))) =
        lookup(program).filter(|(target, _)| target.kind == TargetKind::Executable)
    else {
        return Ok(());
    };
    *program = file;
    let emulator_enabled = test.policies.get(PolicyId::new(158)) != PolicyStatus::New
        || model
            .definition(test.dir, b"CMAKE_CROSSCOMPILING")
            .is_some_and(|value| truth::is_on(value));
    let policy = PolicyId::new(178);
    let status = test.policies.get(policy);
    let keep_empty = if status == PolicyStatus::New {
        list::Empty::Keep
    } else {
        list::Empty::Drop
    };
    let mut prefix = Vec::new();
    for property in [b"TEST_LAUNCHER".as_slice(), b"CROSSCOMPILING_EMULATOR"] {
        if property == b"CROSSCOMPILING_EMULATOR" && !emulator_enabled {
            continue;
        }
        if let Some(value) = target.props.get(property).filter(|value| !value.is_empty()) {
            let folded = evaluate(value)?;
            let words = list::split(&folded, keep_empty);
            if words.first().is_some_and(|word| !word.is_empty()) {
                if status == PolicyStatus::Warn
                    && let Some(host) = host
                    && list::split(&folded, list::Empty::Keep) != words
                {
                    let file =
                        Lexical::new(&model.dir(test.dir).source_dir).append(b"CMakeLists.txt");
                    warn_policy(
                        model,
                        host,
                        policy,
                        Location::FileLine {
                            file: file.as_slice().into(),
                            line: 0,
                        },
                        &format!(
                            "The {} property of target '{}' contains empty list items. Those empty items are being silently discarded to preserve backward compatibility.",
                            BString::from(property),
                            target.name
                        ),
                        "",
                    );
                }
                prefix.extend(words);
            }
        }
    }
    test.command.splice(0..0, prefix);
    Ok(())
}

pub(super) fn resolve_argument<'a>(
    value: &[u8],
    context: &EvalContext,
    usage: &mut UsageResolver<'_>,
    dir: DirId,
    root: &[u8],
    lookup: &impl Fn(&[u8]) -> Option<(&'a Target, Option<BString>)>,
) -> Result<BString, GenError> {
    if value.len() <= rcmake_base::Limits::default().max_value_bytes
        && rcmake_genex::find_genex(value).is_none()
    {
        return Ok(value.into());
    }
    let result = parse(value).and_then(|mut ast| {
        use_resolved_artifacts(&mut ast.nodes, lookup);
        evaluate(&ast, context, &mut usage.host(dir)).map(|(value, _)| value)
    });
    result.map_err(|error| {
        let diagnostic = if context.site.backtrace.is_empty() {
            error.without_location()
        } else {
            error.at(&context.site.backtrace, root)
        };
        GenError::Invalid(format!("\n{diagnostic}").into())
    })
}

fn use_resolved_artifacts<'a>(
    nodes: &mut [GenexNode],
    lookup: &impl Fn(&[u8]) -> Option<(&'a Target, Option<BString>)>,
) {
    for node in nodes {
        let GenexNode::Expr(expr) = node else {
            continue;
        };
        if let ([GenexNode::Text(op)], Some([parameter])) =
            (expr.identifier.as_slice(), expr.params.as_deref())
            && let [GenexNode::Text(name)] = parameter.as_slice()
            && matches!(
                op.as_slice(),
                b"TARGET_FILE"
                    | b"TARGET_FILE_DIR"
                    | b"TARGET_FILE_NAME"
                    | b"TARGET_FILE_BASE_NAME"
            )
            && let Some((target, Some(file))) = lookup(name)
        {
            let path = Lexical::new(&file);
            let value = match op.as_slice() {
                b"TARGET_FILE" => Some(file.clone()),
                b"TARGET_FILE_DIR" => Some(path.parent().into()),
                b"TARGET_FILE_NAME" => Some(path.filename().into()),
                b"TARGET_FILE_BASE_NAME" if target.is_imported() => Some(target.name.clone()),
                _ => None,
            };
            if let Some(value) = value {
                *node = GenexNode::Text(value);
                continue;
            }
        }
        use_resolved_artifacts(&mut expr.identifier, lookup);
        for parameter in expr.params.iter_mut().flatten() {
            use_resolved_artifacts(parameter, lookup);
        }
    }
}
