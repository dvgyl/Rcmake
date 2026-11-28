use std::time::Duration;

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::to_native;
use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunRequest, Stream};

use super::incorrect_arguments;

#[derive(Clone, Copy, Default)]
enum Section {
    #[default]
    Positionals,
    Args,
    OutputName,
    ResultName,
}

#[derive(Default)]
struct Invocation {
    arguments: Vec<u8>,
    output_name: BString,
    result_name: BString,
    has_working_dir: bool,
}

impl Invocation {
    fn parse(args: &[ExpandedArg]) -> Result<Self, CmdError> {
        let mut section = Section::Positionals;
        let mut invocation = Self::default();
        let mut positionals = 0;
        for arg in args {
            let value = arg.value.as_slice();
            section = match (section, value) {
                (_, b"OUTPUT_VARIABLE") => Section::OutputName,
                (Section::OutputName, _) => {
                    if !invocation.output_name.is_empty() {
                        return Err(incorrect_arguments());
                    }
                    invocation.output_name = value.into();
                    Section::Positionals
                }
                (_, b"RETURN_VALUE") => Section::ResultName,
                (Section::ResultName, _) => {
                    if !invocation.result_name.is_empty() {
                        return Err(incorrect_arguments());
                    }
                    invocation.result_name = value.into();
                    Section::Positionals
                }
                (_, b"ARGS") => Section::Args,
                (Section::Args, _) => {
                    invocation.arguments.extend_from_slice(value);
                    invocation.arguments.push(b' ');
                    Section::Args
                }
                (Section::Positionals, _) => {
                    positionals += 1;
                    Section::Positionals
                }
            };
        }
        invocation.has_working_dir = positionals == 2;
        Ok(invocation)
    }
}

pub(super) fn exec_program(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    let policy = PolicyId::new(153);
    match cx.policy(policy) {
        PolicyStatus::New => {
            cx.issue(
                MessageKind::Error,
                Effect::SetError,
                None,
                "The exec_program command should not be called; see CMP0153.  Use execute_process() instead."
                    .into(),
            );
            return Err(CmdError::Reported);
        }
        PolicyStatus::Warn => {
            crate::policy_misc::issue_warning(cx, policy, b"", b"Use execute_process() instead.");
        }
        PolicyStatus::Old => {}
    }
    let raw = cx.raw_args().to_vec();
    let args = cx.expand_raw_args(&raw)?;
    let Some(program) = args.first() else {
        return Err(incorrect_arguments());
    };
    let command_bytes = args.iter().try_fold(0_usize, |bytes, arg| {
        bytes.checked_add(arg.value.len().saturating_add(1))
    });
    if command_bytes.is_none_or(|bytes| bytes > cx.limits().max_value_bytes) {
        return Err(CmdError::Status("command exceeds the size limit".into()));
    }
    let invocation = Invocation::parse(&args)?;
    let print = invocation.output_name.is_empty();
    let cwd = cx.host().initial_cwd();
    let mut command = Vec::new();
    if let Some(directory) = args.get(1).filter(|_| invocation.has_working_dir) {
        let _ = cx
            .host()
            .create_dir_all(&cwd.join(to_native(&directory.value)));
        if !directory.value.is_empty() {
            command.extend_from_slice(b"cd \"");
            command.extend_from_slice(&directory.value);
            command.extend_from_slice(b"\" && ");
        }
    }
    if invocation.arguments.is_empty() {
        command.extend_from_slice(&program.value);
    } else {
        for &byte in program.value.as_slice() {
            if byte == b' ' {
                command.push(b'\\');
            }
            command.push(byte);
        }
        command.push(b' ');
        command.extend_from_slice(&invocation.arguments);
    }
    command.extend_from_slice(b" 2>&1");
    if print {
        cx.host().write_out(
            Stream::Stdout,
            &[b"running ", command.as_slice(), b"\n"].concat(),
        );
    }
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
        }],
        cwd,
        env: cx.host().env_all(),
        stdin: Input::Inherit,
        stdout: Output::Capture,
        stderr: Output::SharedWithStdout,
        timeout: Some(Duration::from_secs(120)),
        tee: print,
        default_error_mode: false,
    };
    let outcome = cx.host().run(&request, &mut |_, bytes| {
        cx.host().write_out(Stream::Stdout, bytes);
    });
    let mut result = -1;
    let mut output = Vec::new();
    match outcome {
        Ok(run) => {
            output = run.stdout;
            match run.results.first() {
                Some(ProcessResult::Exited(code)) => result = *code,
                Some(ProcessResult::TimedOut) => {
                    output.extend_from_slice(b"\nProcess terminated due to timeout.");
                }
                Some(ProcessResult::SpawnFailed(error)) => {
                    output.extend_from_slice(b"\nProcess failed because: ");
                    output.extend_from_slice(error.os_message.as_bytes());
                }
                Some(ProcessResult::Signaled(signal)) => {
                    output.extend_from_slice(
                        format!("\nProcess terminated due to: Signal {signal}").as_bytes(),
                    );
                }
                None => {}
            }
        }
        Err(error) => {
            output.extend_from_slice(b"\nProcess failed because: ");
            output.extend_from_slice(error.os_message.as_bytes());
        }
    }
    if !invocation.output_name.is_empty() {
        cx.set_var(&invocation.output_name, trim_output(&output).into());
    }
    if !invocation.result_name.is_empty() {
        cx.set_var(&invocation.result_name, result.to_string().into());
    }
    Ok(Flow::Normal)
}

fn trim_output(output: &[u8]) -> &[u8] {
    let text = |byte: &u8| !matches!(byte, b' ' | b'\n' | b'\t' | b'\r');
    match (output.iter().position(text), output.iter().rposition(text)) {
        (Some(first), Some(last)) => output.get(first..=last).unwrap_or(output),
        _ => output,
    }
}
