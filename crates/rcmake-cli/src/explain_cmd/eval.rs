//! `rcmake explain --eval <expression>` and `rcmake explain <target> --eval-property <p>`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use rcmake_api::{ApiError, EvalRequest, render_eval_json, render_eval_terminal, run_configure};
use rcmake_base::BString;
use rcmake_eval::FindTraceLevel;
use rcmake_host::{Host, ReadRange, Stream};

use super::args::EvalOptions;
use super::{error, request};
use crate::resolve_cli_path;

pub(super) const HELP: &str = "\
Usage: rcmake explain --eval <expression> [-B <build-dir>] [--target <name>]
                      [--config <cfg>] [--lang <lang>] [--link-lang <lang>]
                      [--directory <dir>] [--link] [--no-steps] [--json]
                      [--color=auto|always|never]
       rcmake explain <target> --eval-property <property> [same options]

Evaluate a generator expression in a configured build directory.

  -B <build-dir>          Read the build directory. Default: current directory.
  --eval <expression>     Expression to evaluate. Use @<file> to read it from a file.
  --eval-property <p>     Evaluate property <p> of <target>, the property owner.
  --target <name>         Evaluate as this consuming target. With --eval-property,
                          the default consumer is the owner.
  --config <cfg>          Evaluate for one configuration. Default: every configuration.
  --lang <lang>           Set $<COMPILE_LANGUAGE>.
  --link-lang <lang>      Set $<LINK_LANGUAGE>.
  --directory <dir>       Look up target names from this source directory.
  --link                  Follow link-only usage ($<LINK_ONLY:...>).
  --command-config <cfg>  Set $<COMMAND_CONFIG> for a cross-configuration build.
  --output-config <cfg>   Set $<OUTPUT_CONFIG> for a cross-configuration build.
  --no-steps              Show only the result.
  --json                  Write one JSON report to standard output.
  --color=<mode>          Set terminal color. Default: auto.
  -h, --help              Show this help.

Put the expression in single quotes so that the shell does not expand $<.
rcmake evaluates the project again without running subprocesses or writing to
the build directory.

Examples
  rcmake explain --eval '$<$<CONFIG:Debug>:-DTRACE>' --config Debug
  rcmake explain --eval '$<TARGET_PROPERTY:app,INCLUDE_DIRECTORIES>' --lang CXX
  rcmake explain liba --eval-property INTERFACE_COMPILE_DEFINITIONS --target app

Exit status
  0  The expression evaluated in every selected configuration.
  1  Evaluation failed in at least one configuration, an operator had no evidence,
     or the evaluation context is provisional.
  2  Invalid arguments or an unavailable configured build directory.
";

const MAX_EXPRESSION: u64 = 64 << 10;

fn expression(host: &dyn Host, text: &BString) -> Result<BString, String> {
    let Some(file) = text.strip_prefix(b"@") else {
        if text.len() as u64 > MAX_EXPRESSION {
            return Err("The expression is larger than 64 KiB.".into());
        }
        return Ok(text.clone());
    };
    let file = resolve_cli_path(host, &BString::from(file));
    let bytes = host
        .read(
            &file,
            ReadRange::All {
                limit: MAX_EXPRESSION + 1,
            },
        )
        .map_err(|err| format!("Cannot read {}. {err}", file.display()))?;
    if bytes.len() as u64 > MAX_EXPRESSION {
        return Err(format!("{} is larger than 64 KiB.", file.display()));
    }
    let mut bytes = BString::from(bytes);
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes.pop();
    }
    Ok(bytes)
}

pub(super) fn run(argv0: Option<&BString>, options: &EvalOptions, host: &Arc<dyn Host>) -> i32 {
    let text = match options
        .expression
        .as_ref()
        .map(|text| expression(host.as_ref(), text))
    {
        Some(Ok(text)) => Some(text),
        Some(Err(message)) => return error(host.as_ref(), &message, 2),
        None => None,
    };
    let request_config = match request(options.build.as_ref(), argv0, host, FindTraceLevel::Off) {
        Ok(request) => request,
        Err(message) => return error(host.as_ref(), &message, 2),
    };
    let result = match run_configure(request_config) {
        Ok(result) => result,
        Err(err @ ApiError::InvalidConfig { .. }) => {
            return error(host.as_ref(), &err.to_string(), 2);
        }
        Err(err) => {
            return error(
                host.as_ref(),
                &format!("{err} The evaluation context is unavailable."),
                1,
            );
        }
    };
    super::report_partial(host.as_ref(), &result, false);
    let eval = EvalRequest {
        expression: text,
        owner: options.owner.clone(),
        property: options.property.clone(),
        consumer: options.consumer.clone(),
        configs: options.config.iter().cloned().collect(),
        language: options.language.clone(),
        link_language: options.link_language.clone(),
        directory: options.directory.clone(),
        link: options.link,
        command_config: options.command_config.clone(),
        output_config: options.output_config.clone(),
        step_limit: if options.no_steps { 1 } else { 1024 },
        module_root: super::module_root(host.as_ref(), argv0),
    };
    let report = match result.evaluate_genex(&eval) {
        Ok(report) => report,
        Err(message) => return error(host.as_ref(), &message, 1),
    };
    if options.json {
        host.write_out(Stream::Stdout, &render_eval_json(&report));
    } else {
        let text = render_eval_terminal(&report, !options.no_steps);
        host.write_out(Stream::Stdout, text.as_bytes());
    }
    if report.success() { 0 } else { 1 }
}
