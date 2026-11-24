use rcmake_base::truth;
use rcmake_host::path::to_native;
use rcmake_presets::TestVerbosity;

use super::{
    Invocation, Script, display, output_integer, parse_action, parse_definition, truncation_mode,
};
use crate::selection::IndexSource;

pub(super) fn selection<'a>(
    cli: &mut Invocation,
    flag: &[u8],
    value: &mut impl FnMut() -> Result<&'a [u8], String>,
) -> Result<bool, String> {
    match flag {
        b"-R" | b"--tests-regex" => cli.options.include = Some(value()?.into()),
        b"-E" | b"--exclude-regex" => cli.options.exclude = Some(value()?.into()),
        b"-L" | b"--label-regex" => cli.options.include_labels.push(value()?.into()),
        b"-LE" | b"--label-exclude" => cli.options.exclude_labels.push(value()?.into()),
        b"-FA" | b"--fixture-exclude-any" => cli.selection.exclude_fixture = Some(value()?.into()),
        b"-FS" | b"--fixture-exclude-setup" => {
            cli.selection.exclude_fixture_setup = Some(value()?.into())
        }
        b"-FC" | b"--fixture-exclude-cleanup" => {
            cli.selection.exclude_fixture_cleanup = Some(value()?.into())
        }
        b"-I" | b"--tests-information" => {
            cli.selection.indices = Some(IndexSource::Argument(value()?.into()))
        }
        b"-U" | b"--union" => cli.selection.union = true,
        b"--tests-from-file" => cli.selection.tests_from_file = Some(to_native(value()?)),
        b"--exclude-from-file" => cli.selection.exclude_from_file = Some(to_native(value()?)),
        b"--rerun-failed" => cli.selection.rerun_failed = true,
        b"--out-of-date" => cli.selection.out_of_date = true,
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn output<'a>(
    cli: &mut Invocation,
    flag: &[u8],
    value: &mut impl FnMut() -> Result<&'a [u8], String>,
) -> Result<bool, String> {
    match flag {
        b"--output-on-failure" => cli.output.output_on_failure = Some(true),
        b"--output-junit" => cli.output.output_junit_file = Some(value()?.into()),
        b"-O" | b"--output-log" => cli.output.output_log_file = Some(value()?.into()),
        b"-V" | b"--verbose" => {
            if cli.output.verbosity != Some(TestVerbosity::Extra) {
                cli.output.verbosity = Some(TestVerbosity::Verbose);
            }
        }
        b"-VV" | b"--extra-verbose" => cli.output.verbosity = Some(TestVerbosity::Extra),
        b"-Q" | b"--quiet" => cli.output.quiet = Some(true),
        b"--progress" => cli.output.short_progress = Some(true),
        b"--print-labels" => cli.print_labels = true,
        b"--no-compress-output" => {}
        b"--no-label-summary" => cli.output.label_summary = Some(false),
        b"--no-subproject-summary" => cli.output.subproject_summary = Some(false),
        b"--debug" => cli.output.debug = Some(true),
        b"-W" | b"--max-width" => {
            cli.output.max_test_name_width = Some(output_integer(flag, value()?)?)
        }
        b"--test-output-size-passed" => {
            cli.output.max_passed_test_output_size = Some(output_integer(flag, value()?)?)
        }
        b"--test-output-size-failed" => {
            cli.output.max_failed_test_output_size = Some(output_integer(flag, value()?)?)
        }
        b"--test-output-truncation" => {
            let raw = value()?;
            cli.output.test_output_truncation = Some(truncation_mode(raw).ok_or_else(|| {
                format!(
                    "Invalid value for '--test-output-truncation': {}",
                    display(raw)
                )
            })?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn execution<'a>(
    cli: &mut Invocation,
    flag: &[u8],
    value: &mut impl FnMut() -> Result<&'a [u8], String>,
) -> Result<bool, String> {
    match flag {
        b"-F" => cli.execution.enable_failover = Some(true),
        b"--resource-spec-file" => cli.execution.resource_spec_file = Some(value()?.into()),
        b"--interactive-debug-mode" => {
            cli.execution.interactive_debugging = Some(truth::is_on(value()?));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn dashboard<'a>(
    cli: &mut Invocation,
    flag: &[u8],
    value: &mut impl FnMut() -> Result<&'a [u8], String>,
) -> Result<bool, String> {
    match flag {
        b"-M" | b"--test-model" => {
            let raw = value()?;
            cli.dashboard_model = Some(
                crate::dart::Model::parse(raw)
                    .ok_or_else(|| format!("Unknown dashboard model: {}", display(raw)))?,
            );
        }
        b"-T" | b"--test-action" => cli.actions.push(parse_action(value()?)?),
        b"-D" | b"--dashboard" => cli.definitions.push(parse_definition(value()?)?),
        b"-S" | b"--script" | b"-SP" | b"--script-new-process" => cli.scripts.push(Script {
            argument: value()?.into(),
            new_process: matches!(flag, b"-SP" | b"--script-new-process"),
        }),
        _ => return Ok(false),
    }
    Ok(true)
}
