use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{CommandSpec, HostError, Input, Output, ProcessResult, RunRequest, Stream};

use super::{ToolCtx, error, is_dir, path, split_once, usage, write};

type Environment = BTreeMap<BString, BString>;

pub(super) fn print_environment(
    env: impl IntoIterator<Item = (BString, BString)>,
    cx: &ToolCtx<'_>,
) {
    let mut entries: Vec<_> = env.into_iter().collect();
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    for (key, value) in entries {
        write(cx, Stream::Stdout, &[&key, b"=", &value, b"\n"]);
    }
}

pub(super) fn env(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let original: Environment = cx.host.env_all().into_iter().collect();
    let mut env = original.clone();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == b"--" {
            break;
        }
        if arg == b"--modify" {
            let Some(change) = args.next() else {
                return error(cx, &[b"cmake -E env: --modify missing a parameter\n"]);
            };
            if let Err(message) = modify(&mut env, &original, change, cfg!(windows)) {
                return error(
                    cx,
                    &[
                        b"CMake Error: Error: ",
                        &message,
                        b"\n\ncmake -E env: invalid parameter to --modify: ",
                        change,
                        b"\n",
                    ],
                );
            }
        } else if let Some(key) = arg.strip_prefix(b"--unset=") {
            env_remove(&mut env, key, cfg!(windows));
        } else if arg.starts_with(b"-") {
            return error(cx, &[b"cmake -E env: unknown option '", arg, b"'\n"]);
        } else if let Some((key, value)) = split_once(arg, b'=') {
            env_insert(&mut env, key, BString::from(value), cfg!(windows));
        } else {
            return run_child(
                arg,
                args.as_slice(),
                cx.cwd,
                env.into_iter().collect(),
                false,
                cx,
            );
        }
    }
    if let Some((program, args)) = args.as_slice().split_first() {
        return run_child(program, args, cx.cwd, env.into_iter().collect(), false, cx);
    }
    error(cx, &[b"cmake -E env: no command given\n"])
}

fn env_get<'a>(env: &'a Environment, key: &[u8], case_insensitive: bool) -> Option<&'a BString> {
    if let Some(value) = env.get(key) {
        return Some(value);
    }
    if case_insensitive {
        env.iter()
            .find_map(|(existing, value)| existing.eq_ignore_ascii_case(key).then_some(value))
    } else {
        None
    }
}

fn env_remove(env: &mut Environment, key: &[u8], case_insensitive: bool) {
    if case_insensitive {
        let matching: Vec<BString> = env
            .keys()
            .filter(|existing| existing.eq_ignore_ascii_case(key))
            .cloned()
            .collect();
        for existing in matching {
            env.remove(&existing);
        }
    } else {
        env.remove(key);
    }
}

fn env_insert(env: &mut Environment, key: &[u8], value: BString, case_insensitive: bool) {
    if case_insensitive {
        env_remove(env, key, true);
    }
    env.insert(BString::from(key), value);
}

fn modify(
    env: &mut Environment,
    original: &Environment,
    change: &[u8],
    windows: bool,
) -> Result<(), Vec<u8>> {
    let Some((key, rest)) = split_once(change, b'=') else {
        return Err([
            b"Missing `=` after the variable name in: ".as_slice(),
            change,
        ]
        .concat());
    };
    let Some((operation, value)) = split_once(rest, b':') else {
        return Err([b"Missing `:` after the operation in: ".as_slice(), change].concat());
    };
    let current = env_get(env, key, windows).map_or(b"".as_slice(), |value| value.as_slice());
    let path_separator = if windows { b';' } else { b':' };
    let new = match operation {
        b"reset" => env_get(original, key, windows).cloned(),
        b"set" => Some(BString::from(value)),
        b"unset" => None,
        b"string_append" => Some(BString::from([current, value].concat())),
        b"string_prepend" => Some(BString::from([value, current].concat())),
        b"path_list_append" => Some(join_value(current, value, path_separator, false)),
        b"path_list_prepend" => Some(join_value(current, value, path_separator, true)),
        b"cmake_list_append" => Some(join_value(current, value, b';', false)),
        b"cmake_list_prepend" => Some(join_value(current, value, b';', true)),
        _ => {
            return Err([
                b"Unrecognized environment manipulation argument: ".as_slice(),
                operation,
            ]
            .concat());
        }
    };
    if let Some(new) = new {
        env_insert(env, key, new, windows);
    } else {
        env_remove(env, key, windows);
    }
    Ok(())
}

fn join_value(current: &[u8], value: &[u8], separator: u8, prepend: bool) -> BString {
    if current.is_empty() {
        return BString::from(value);
    }
    let (first, last) = if prepend {
        (value, current)
    } else {
        (current, value)
    };
    BString::from([first, &[separator], last].concat())
}

pub(super) fn chdir(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let [directory, program, rest @ ..] = args else {
        return usage(cx, 1);
    };
    let dir = path(cx, directory);
    if directory.is_empty() || !is_dir(cx, &dir) {
        return error(
            cx,
            &[
                b"CMake Error: Directory does not exist for chdir command: ",
                directory,
                b"\n",
            ],
        );
    }
    run_child(program, rest, &dir, cx.host.env_all(), false, cx)
}

pub(super) fn time(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let Some((program, args)) = args.split_first() else {
        return usage(cx, 1);
    };
    let start = cx.host.monotonic();
    let status = run_child(program, args, cx.cwd, cx.host.env_all(), true, cx);
    let seconds = cx.host.monotonic().saturating_sub(start).as_secs_f64();
    write(
        cx,
        Stream::Stdout,
        &[
            b"Elapsed time (seconds): ",
            elapsed_seconds(seconds).as_bytes(),
            b"\n",
        ],
    );
    status
}

// CMake prints six significant digits without trailing zeroes.
fn elapsed_seconds(seconds: f64) -> String {
    let scientific = format!("{seconds:.5e}");
    let Some((digits, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return scientific;
    };
    if (-4..6).contains(&exponent) {
        let precision = usize::try_from(5 - exponent).unwrap_or_default();
        let fixed = format!("{seconds:.precision$}");
        return if fixed.contains('.') {
            trim_decimal(&fixed).to_owned()
        } else {
            fixed
        };
    }
    format!("{}e{exponent:+03}", trim_decimal(digits))
}

fn trim_decimal(text: &str) -> &str {
    text.trim_end_matches('0').trim_end_matches('.')
}

fn run_child(
    program: &BString,
    args: &[BString],
    cwd: &Path,
    env: Vec<(BString, BString)>,
    timed: bool,
    cx: &ToolCtx<'_>,
) -> i32 {
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
        }],
        cwd: cwd.to_path_buf(),
        env,
        stdin: Input::Inherit,
        stdout: Output::Inherit,
        stderr: Output::Inherit,
        timeout: None,
        tee: false,
        default_error_mode: false,
    };
    let result = cx.host.run(&request, &mut |_, _| {});
    match result {
        Ok(result) => match result.results.first() {
            Some(ProcessResult::Exited(status)) => return *status,
            Some(ProcessResult::SpawnFailed(err)) => child_error(err, cx),
            Some(ProcessResult::TimedOut) => error(cx, &[b"Process terminated due to timeout\n"]),
            Some(ProcessResult::Signaled(_)) => error(cx, &[b"Subprocess terminated\n"]),
            None => error(cx, &[b"No subprocess result\n"]),
        },
        Err(err) => child_error(&err, cx),
    };
    i32::from(!timed)
}

fn child_error(err: &HostError, cx: &ToolCtx<'_>) -> i32 {
    let message = match err.kind {
        ErrorKind::NotFound => "no such file or directory",
        ErrorKind::PermissionDenied => "permission denied",
        _ => &err.os_message,
    };
    error(cx, &[message.as_bytes(), b"\n"])
}

pub(super) fn sleep(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    if args.is_empty() {
        return usage(cx, 1);
    }
    let mut seconds = 0.0;
    for arg in args {
        let Some(value) = sleep_seconds(arg) else {
            return error(cx, &[b"Unknown sleep time format \"", arg, b"\".\n"]);
        };
        seconds += value;
    }
    let Ok(duration) = Duration::try_from_secs_f64(seconds) else {
        return error(cx, &[b"Sleep duration is out of range.\n"]);
    };
    cx.host.sleep(duration);
    0
}

fn sleep_seconds(arg: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(arg).ok()?.trim_ascii_start();
    let text = text.strip_suffix('s').unwrap_or(text);
    let value = text.parse::<f64>().ok().or_else(|| hex_seconds(text))?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn hex_seconds(text: &str) -> Option<f64> {
    let (sign, text) = if let Some(rest) = text.strip_prefix('-') {
        (-1.0, rest)
    } else {
        (1.0, text.strip_prefix('+').unwrap_or(text))
    };
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    let (digits, exponent) = match hex.split_once(['p', 'P']) {
        Some((digits, exponent)) => (digits, exponent.parse::<i32>().ok()?),
        None => (hex, 0),
    };
    let mut value = 0.0;
    let mut scale = 1.0;
    let mut fractional = false;
    let mut has_digit = false;
    for digit in digits.chars() {
        if digit == '.' && !fractional {
            fractional = true;
        } else {
            let digit = f64::from(digit.to_digit(16)?);
            has_digit = true;
            if fractional {
                scale /= 16.0;
                value += digit * scale;
            } else {
                value = value * 16.0 + digit;
            }
        }
    }
    has_digit.then_some(sign * value * 2.0_f64.powi(exponent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_env_modify_uses_case_insensitive_keys() {
        let mut original = Environment::new();
        original.insert("CONFIGVAR".into(), "con".into());
        let mut env = original.clone();

        assert!(modify(&mut env, &original, b"ConfigVar=string_append:fig", true).is_ok());
        assert_eq!(
            env_get(&env, b"ConfigVar", true),
            Some(&BString::from("config"))
        );
        assert_eq!(env.len(), 1);

        assert!(modify(&mut env, &original, b"configvar=reset:", true).is_ok());
        assert_eq!(
            env_get(&env, b"CONFIGVAR", true),
            Some(&BString::from("con"))
        );
        assert_eq!(env.len(), 1);

        env_remove(&mut env, b"ConfigVar", true);
        assert!(env.is_empty());
    }

    #[test]
    fn posix_env_modify_keeps_case_sensitivity() {
        let mut original = Environment::new();
        original.insert("CONFIGVAR".into(), "con".into());
        let mut env = original.clone();

        assert!(modify(&mut env, &original, b"ConfigVar=string_append:fig", false).is_ok());
        assert_eq!(
            env_get(&env, b"ConfigVar", false),
            Some(&BString::from("fig"))
        );
        assert_eq!(
            env_get(&env, b"CONFIGVAR", false),
            Some(&BString::from("con"))
        );
        assert_eq!(env.len(), 2);
    }
}
