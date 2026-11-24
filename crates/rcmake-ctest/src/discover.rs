//! Expand runtime test definitions from discovery command output.

use std::collections::BTreeMap;
use std::time::Duration;

use rcmake_base::{BString, Limits, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunRequest, path::to_native};
use rcmake_regex::{Captures, MatchOptions, Regex};

use crate::discovery::{Test, register_test};
use crate::{execution, state};

const KEYWORDS: &[&[u8]] = &[
    b"COMMAND",
    b"DISCOVERY_ARGS",
    b"DISCOVERY_MATCH",
    b"DISCOVERY_PROPERTIES",
    b"TEST_NAME",
    b"TEST_ARGS",
    b"TEST_PROPERTIES",
    b"BUILD_DEPENDS",
    b"TEST_LIST",
];

fn parse(args: &[ExpandedArg]) -> Result<BTreeMap<&[u8], Vec<BString>>, CmdError> {
    let mut fields = BTreeMap::<&[u8], Vec<BString>>::new();
    let mut key = None;
    for arg in args {
        if KEYWORDS.contains(&arg.value.as_slice()) {
            key = Some(arg.value.as_slice());
            fields.entry(arg.value.as_slice()).or_default();
        } else if let Some(key) = key {
            fields.entry(key).or_default().push(arg.value.clone());
        } else {
            return Err(CmdError::Status(
                format!("unknown argument {}", arg.value).into(),
            ));
        }
    }
    for key in [b"DISCOVERY_MATCH".as_slice(), b"TEST_NAME", b"TEST_LIST"] {
        if let Some(values) = fields.get(key)
            && values.len() != 1
        {
            return Err(CmdError::Status(
                format!("{} requires one value", BString::from(key)).into(),
            ));
        }
    }
    for key in [b"DISCOVERY_ARGS".as_slice(), b"TEST_ARGS"] {
        if fields.get(key).is_some_and(Vec::is_empty) {
            return Err(CmdError::Status(
                format!("{} requires at least one value", BString::from(key)).into(),
            ));
        }
    }
    Ok(fields)
}

fn properties(key: &str, values: &[BString]) -> Result<BTreeMap<BString, BString>, CmdError> {
    let (pairs, remainder) = values.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(CmdError::Status(
            format!("{key} must be key-value pairs.").into(),
        ));
    }
    Ok(pairs
        .iter()
        .map(|[key, value]| (key.clone(), value.clone()))
        .collect())
}

pub(crate) fn run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let fields = parse(args)?;
    let values = |key: &[u8]| fields.get(key).map(Vec::as_slice).unwrap_or_default();
    let first = |key: &[u8]| {
        values(key)
            .first()
            .map(|value| value.as_slice())
            .unwrap_or_default()
    };
    let Some((program, command_args)) = values(b"COMMAND").split_first() else {
        return Err(CmdError::Status("COMMAND requires a program".into()));
    };
    let pattern = first(b"DISCOVERY_MATCH");
    let mut anchored = Vec::new();
    if !pattern.starts_with(b"^") {
        anchored.push(b'^');
    }
    anchored.extend_from_slice(pattern);
    if anchored.len() < 2 || !pattern.ends_with(b"$") {
        anchored.push(b'$');
    }
    let regex = Regex::new(&anchored).map_err(|_| {
        CmdError::Status(
            format!(
                "DISCOVERY_MATCH failed to compile regex \"{}\".",
                BString::from(pattern)
            )
            .into(),
        )
    })?;
    let test_properties = properties("TEST_PROPERTIES", values(b"TEST_PROPERTIES"))?;
    let discovery = Test {
        name: BString::default(),
        command: CommandSpec {
            program: program.clone(),
            args: command_args.to_vec(),
        },
        directory: cx.current_binary_dir(),
        properties: properties("DISCOVERY_PROPERTIES", values(b"DISCOVERY_PROPERTIES"))?,
        timeout: Default::default(),
    };
    let mut command = discovery.command.clone();
    command.args.extend_from_slice(values(b"DISCOVERY_ARGS"));
    let request = RunRequest {
        pipeline: vec![command],
        cwd: discovery
            .directory
            .join(to_native(discovery.property(b"WORKING_DIRECTORY"))),
        env: execution::environment(cx.host(), &discovery)
            .map_err(|error| CmdError::Status(error.into()))?
            .into_iter()
            .collect(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(
            execution::parse_timeout(discovery.property(b"TIMEOUT"))
                .unwrap_or(Duration::from_secs(5)),
        )
        .filter(|timeout| !timeout.is_zero()),
        tee: false,
    };
    let failure = |detail: &str| {
        CmdError::Status(format!("failed to run command: {program}\n\n{detail}").into())
    };
    let output = cx
        .host()
        .run(&request, &mut |_, _| {})
        .map_err(|error| failure(&error.to_string()))?;
    match output.results.as_slice() {
        [ProcessResult::Exited(0)] => {}
        [ProcessResult::TimedOut] => return Err(failure("Process terminated due to timeout")),
        [ProcessResult::SpawnFailed(error)] => {
            let detail = if error.kind == std::io::ErrorKind::NotFound {
                "no such file or directory"
            } else {
                &error.os_message
            };
            return Err(failure(detail));
        }
        _ => {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(failure(detail.trim_end()));
        }
    }

    let limits = Limits::default();
    if output.stdout.len() > limits.max_value_bytes {
        return Err(CmdError::Status(
            "discovery output exceeds the value size limit".into(),
        ));
    }
    let mut budget = limits.regex_steps;
    let mut names_bytes = 0usize;
    let mut names = Vec::new();
    let test_list = first(b"TEST_LIST");
    let stdout = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    for line in state::lines(stdout).take_while(|_| !output.stdout.is_empty()) {
        let Some(captures) = regex
            .find_at(line, 0, MatchOptions::default(), &mut budget)
            .map_err(|error| CmdError::Status(error.to_string().into()))?
        else {
            continue;
        };
        let expand = |template: &[u8]| substitute(template, line, &captures, regex.group_count());
        let name = expand(first(b"TEST_NAME"))?;
        let mut props: BTreeMap<_, _> = test_properties
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand(value)?)))
            .collect::<Result<_, CmdError>>()?;
        if !values(b"BUILD_DEPENDS").is_empty() {
            props.insert(
                "_CMAKE_TEST_BUILD_DEPENDS".into(),
                list::join(values(b"BUILD_DEPENDS")),
            );
        }
        if !test_list.is_empty() {
            let escaped = name
                .iter()
                .flat_map(|&byte| (byte == b';').then_some(b'\\').into_iter().chain([byte]))
                .collect::<Vec<_>>();
            names_bytes = names_bytes.saturating_add(escaped.len() + 1);
            if names_bytes > limits.max_value_bytes {
                return Err(CmdError::Status(
                    "TEST_LIST exceeds the value size limit".into(),
                ));
            }
            names.push(BString::from(escaped));
        }
        let mut command = discovery.command.clone();
        for arg in values(b"TEST_ARGS") {
            command.args.push(expand(arg)?);
        }
        register_test(Test {
            name,
            command,
            properties: props,
            timeout: Default::default(),
            directory: discovery.directory.clone(),
        });
    }
    if !test_list.is_empty() {
        cx.set_var(test_list, list::join(&names));
    }
    Ok(Flow::Normal)
}

fn substitute(
    template: &[u8],
    line: &[u8],
    captures: &Captures,
    group_count: usize,
) -> Result<BString, CmdError> {
    let limit = Limits::default().max_value_bytes;
    let mut value = BString::from(template);
    for (group, span) in captures.groups.iter().take(group_count + 1).enumerate() {
        let token = format!("\\{group}");
        let replacement = span
            .and_then(|(start, end)| line.get(start..end))
            .unwrap_or_default();
        if value
            .windows(token.len())
            .any(|window| window == token.as_bytes())
        {
            let mut result = BString::default();
            let mut rest = value.as_slice();
            while let Some(index) = rest
                .windows(token.len())
                .position(|window| window == token.as_bytes())
            {
                if result
                    .len()
                    .saturating_add(index)
                    .saturating_add(replacement.len())
                    > limit
                {
                    return Err(CmdError::Status(
                        "discovery substitution exceeds the value size limit".into(),
                    ));
                }
                result.extend_from_slice(rest.get(..index).unwrap_or_default());
                result.extend_from_slice(replacement);
                rest = rest.get(index + token.len()..).unwrap_or_default();
            }
            if result.len().saturating_add(rest.len()) > limit {
                return Err(CmdError::Status(
                    "discovery substitution exceeds the value size limit".into(),
                ));
            }
            result.extend_from_slice(rest);
            value = result;
        }
    }
    Ok(value)
}
