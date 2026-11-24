//! Record test discovery rules for CTest.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{BT, DiscoverySpec, TestEntry, TestKind};

use super::values;

const OPTIONS: &[&[u8]] = &[
    b"COMMAND",
    b"COMMAND_EXPAND_LISTS",
    b"CONFIGURATIONS",
    b"DISCOVERY_ARGS",
    b"DISCOVERY_MATCH",
    b"DISCOVERY_PROPERTIES",
    b"TEST_NAME",
    b"TEST_ARGS",
    b"TEST_PROPERTIES",
    b"BUILD_DEPENDS",
];

pub(super) fn discover_tests(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let mut options = BTreeMap::<&[u8], Vec<BString>>::new();
    let mut errors = BTreeMap::<&[u8], Vec<&str>>::new();
    let mut args = values(args).peekable();
    let mut unknown = None;
    while let Some(option) = args.next() {
        if !OPTIONS.contains(&option) {
            unknown.get_or_insert(option);
            continue;
        }
        let items = options.entry(option).or_default();
        if option == b"COMMAND_EXPAND_LISTS" {
            continue;
        }
        let single = matches!(option, b"DISCOVERY_MATCH" | b"TEST_NAME");
        let start = items.len();
        while let Some(value) = args.next_if(|word| !OPTIONS.contains(word)) {
            items.push(value.into());
            if single {
                break;
            }
        }
        if items.len() == start
            && matches!(
                option,
                b"COMMAND" | b"DISCOVERY_ARGS" | b"DISCOVERY_MATCH" | b"TEST_NAME" | b"TEST_ARGS"
            )
        {
            errors
                .entry(option)
                .or_default()
                .push("missing required value");
        } else if single && items.last().is_some_and(|value| value.is_empty()) {
            errors
                .entry(option)
                .or_default()
                .push("empty string not allowed");
        }
    }
    if !errors.is_empty() {
        let message = errors
            .into_iter()
            .map(|(option, errors)| {
                format!(
                    "Error after keyword \"{}\":\n  {}",
                    BString::from(option),
                    errors.join("\n  ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(crate::command_error(cx, message));
    }
    if let Some(word) = unknown {
        return Err(CmdError::status(format!(
            "given unknown argument \"{}\".",
            BString::from(word)
        )));
    }
    let command_expand_lists = options.remove(b"COMMAND_EXPAND_LISTS".as_slice()).is_some();
    let mut take = |option: &[u8]| options.remove(option).unwrap_or_default();
    let discovery_props = property_pairs(take(b"DISCOVERY_PROPERTIES"), "DISCOVERY_PROPERTIES")?;
    let mut test_props = property_pairs(take(b"TEST_PROPERTIES"), "TEST_PROPERTIES")?;
    if cx.policy(PolicyId::new(224)) == PolicyStatus::New {
        test_props.push((
            "_CMAKE_DEFAULT_FIXTURE_REPEAT_MODE".into(),
            "AROUND_EACH_REPEAT".into(),
        ));
    }
    let spec = DiscoverySpec {
        discovery_args: take(b"DISCOVERY_ARGS"),
        discovery_match: take(b"DISCOVERY_MATCH").pop().unwrap_or_default(),
        discovery_props,
        test_name: take(b"TEST_NAME").pop().unwrap_or_default(),
        test_args: take(b"TEST_ARGS"),
        test_props,
    };
    let working_directory = spec
        .discovery_props
        .iter()
        .rev()
        .find(|(key, _)| key == b"WORKING_DIRECTORY")
        .map(|(_, value)| value.clone());
    let dir = cx.current_dir_id();
    let index = cx.project().dir(dir).discoveries.len();
    let backtrace = cx.backtrace();
    let test = TestEntry {
        name: format!("__discover_tests_{index}").into(),
        dir,
        command: take(b"COMMAND"),
        configurations: take(b"CONFIGURATIONS"),
        working_directory,
        command_expand_lists,
        old_style: false,
        props: Default::default(),
        build_depends: take(b"BUILD_DEPENDS")
            .into_iter()
            .map(|value| BT::new(value, backtrace.clone()))
            .collect(),
        backtrace,
        kind: TestKind::Discover(Box::new(spec)),
    };
    cx.project()
        .add_test(test)
        .map_err(|error| CmdError::status(error.to_string()))?;
    Ok(Flow::Normal)
}

fn property_pairs(values: Vec<BString>, name: &str) -> Result<Vec<(BString, BString)>, CmdError> {
    let mut pairs = Vec::with_capacity(values.len() / 2);
    let mut values = values.into_iter();
    while let Some(key) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| CmdError::status(format!("{name} must be key-value pairs.")))?;
        pairs.push((key, value));
    }
    Ok(pairs)
}
