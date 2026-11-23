use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_model::{BT, PropScope, TestEntry};

use super::{scoped_test_directory, status, values};

pub(super) fn register(registry: &mut Registry, flags: CmdFlags) {
    registry.add("enable_testing", enable_testing, flags);
    registry.add("add_test", add_test, flags);
    registry.add("set_tests_properties", set_tests_properties, flags);
    registry.add("get_test_property", get_test_property, flags);
}

fn enable_testing(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    cx.set_var(b"CMAKE_TESTING_ENABLED", "1".into());
    cx.directory_props()
        .set("CMAKE_TESTING_ENABLED".into(), "1".into());
    Ok(Flow::Normal)
}

fn add_test(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = values(args).collect();
    let Some((first, rest)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let (name, args, old_style) = match (*first, rest) {
        (b"NAME", [name, args @ ..]) => (*name, args, false),
        (b"NAME", []) => return Err(status("must be given non-empty NAME.")),
        (name, args) if !args.is_empty() => (name, args, true),
        _ => return Err(status("called with incorrect number of arguments")),
    };
    if name.is_empty() && !old_style {
        return Err(status("must be given non-empty NAME."));
    }
    let mut test = TestEntry {
        name: name.into(),
        dir: cx.current_dir_id(),
        backtrace: cx.backtrace(),
        command: Vec::new(),
        configurations: Vec::new(),
        working_directory: None,
        command_expand_lists: false,
        old_style,
        props: Default::default(),
        build_depends: Vec::new(),
        kind: rcmake_model::TestKind::Single,
    };
    if old_style {
        test.command.extend(args.iter().copied().map(BString::from));
    } else {
        parse_test_options(&mut test, args)?;
    }
    let model = cx.project();
    if let Some(id) = model.find_test(test.dir, name) {
        if let Some(existing) = model.test_mut(id).filter(|t| t.old_style && old_style) {
            existing.command = test.command;
            return Ok(Flow::Normal);
        }
        return Err(status(
            [
                b"given test NAME \"".as_slice(),
                name,
                b"\" which already exists in this directory.",
            ]
            .concat(),
        ));
    }
    model
        .add_test(test)
        .map_err(|error| status(error.to_string()))?;
    Ok(Flow::Normal)
}

fn parse_test_options(test: &mut TestEntry, mut args: &[&[u8]]) -> Result<(), CmdError> {
    let mut has_command = false;
    let mut has_configurations = false;
    while let Some((option, rest)) = args.split_first() {
        args = rest;
        match *option {
            b"COMMAND" | b"CONFIGURATIONS" | b"BUILD_DEPENDS" => {
                let end = args
                    .iter()
                    .position(|arg| {
                        matches!(
                            *arg,
                            b"COMMAND"
                                | b"CONFIGURATIONS"
                                | b"WORKING_DIRECTORY"
                                | b"COMMAND_EXPAND_LISTS"
                                | b"BUILD_DEPENDS"
                        )
                    })
                    .unwrap_or(args.len());
                let (items, rest) = args.split_at(end);
                if *option == b"COMMAND" {
                    if has_command {
                        return Err(status("may be given at most one COMMAND."));
                    }
                    has_command = true;
                    test.command
                        .extend(items.iter().copied().map(BString::from));
                } else if *option == b"BUILD_DEPENDS" {
                    test.build_depends.extend(
                        items
                            .iter()
                            .map(|item| BT::new((*item).into(), test.backtrace.clone())),
                    );
                } else {
                    if has_configurations {
                        return Err(status("may be given at most one set of CONFIGURATIONS."));
                    }
                    has_configurations = true;
                    test.configurations
                        .extend(items.iter().copied().map(BString::from));
                }
                args = rest;
            }
            b"WORKING_DIRECTORY" => {
                let Some((directory, rest)) = args.split_first() else {
                    break;
                };
                if test.working_directory.is_some() {
                    return Err(status("may be given at most one WORKING_DIRECTORY."));
                }
                let directory = BString::from(*directory);
                test.props
                    .set("WORKING_DIRECTORY".into(), directory.clone());
                test.working_directory = Some(directory);
                args = rest;
            }
            b"COMMAND_EXPAND_LISTS" => test.command_expand_lists = true,
            _ => {
                return Err(status(
                    [b"given unknown argument:\n  ".as_slice(), option].concat(),
                ));
            }
        }
    }
    if test.command.is_empty() {
        return Err(status("must be given non-empty COMMAND."));
    }
    Ok(())
}

fn set_tests_properties(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = values(args).collect();
    const BAD_PROPERTIES: &str =
        "called with illegal arguments, maybe missing a PROPERTIES specifier?";
    let Some(split) = args.iter().position(|arg| *arg == b"PROPERTIES") else {
        return Err(status(BAD_PROPERTIES));
    };
    let (tests, rest) = args.split_at(split);
    let (pairs, extra) = rest.get(1..).unwrap_or_default().as_chunks::<2>();
    if pairs.is_empty() || !extra.is_empty() {
        return Err(status(BAD_PROPERTIES));
    }
    let (tests, dir) = match tests {
        [tests @ .., b"DIRECTORY", directory] => (tests, scoped_test_directory(cx, directory)?),
        [.., b"DIRECTORY"] => {
            return Err(crate::command_error(
                cx,
                "Error after keyword \"DIRECTORY\":\n  missing required value",
            ));
        }
        _ => (tests, cx.current_dir_id()),
    };
    let project = cx.project();
    let ids = tests
        .iter()
        .map(|name| {
            project.find_test(dir, name).ok_or_else(|| {
                status([b"Can not find test to add properties to: ".as_slice(), name].concat())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        if let Some(test) = project.test_mut(id) {
            for [name, value] in pairs {
                test.props.set((*name).into(), (*value).into());
                if *name == b"WORKING_DIRECTORY" {
                    test.working_directory = Some((*value).into());
                }
            }
        }
    }
    Ok(Flow::Normal)
}

fn get_test_property(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args: Vec<_> = values(args).collect();
    let (test, property, dir, out) = match args.as_slice() {
        [test, property, out] => (*test, *property, cx.current_dir_id(), *out),
        [test, property, b"DIRECTORY", directory, out] => (
            *test,
            *property,
            scoped_test_directory(cx, directory)?,
            *out,
        ),
        _ => return Err(status("called with incorrect number of arguments.")),
    };
    let scope = PropScope::Test {
        dir,
        name: test.into(),
    };
    let project = cx.project();
    let value = if project.find_test(dir, test).is_none() {
        None
    } else if project
        .global
        .defined_property(b"TEST", property)
        .is_some_and(|def| def.inherited)
    {
        project.get_prop_inherited(&scope, property)
    } else {
        project.get_prop(&scope, property)
    };
    cx.set_var(out, value.unwrap_or_else(|| "NOTFOUND".into()));
    Ok(Flow::Normal)
}
