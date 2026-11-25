use std::collections::HashMap;

use jiff::{Timestamp, fmt::strtime, tz::TimeZone};

use rcmake_base::{BString, truth};
use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyStatus, format_paragraphs};
use rcmake_gen_common::{
    GenError,
    export::{
        FileGenerateContext, FileGenerateInput, FileGenerateNewline, FileGeneratePermissions,
        FileGenerateRequest, write_generated_files,
    },
};
use rcmake_genex::{ConfigId, EvalContext, EvalSite, GenexHost, evaluate, parse};
use rcmake_host::{Host, Stream, path::to_native};
use rcmake_model::{DirId, ProjectModel, PropertyMap};
use rcmake_resolve::{
    GenerationResult,
    file_sets::{file_generate_contexts, file_generate_policy},
    usage::{ObjectPaths, UsageResolver},
};

mod call_site;

use call_site::CallSitePolicies;

pub(super) fn write(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
) -> Result<(), GenError> {
    rcmake_commands::print_deferred_properties(model, result, host).map_err(GenError::Invalid)?;
    let props = &model.global.props;
    let Some(count) = props.get(b"_RCMAKE_FILE_GENERATE_COUNT") else {
        return Ok(());
    };
    let count = index(count).ok_or_else(invalid_record)?;
    let dirs: Vec<_> = model
        .dirs()
        .map(|(id, dir)| (id, to_native(&dir.source_dir), to_native(&dir.binary_dir)))
        .collect();
    let cwd = host.initial_cwd();
    let mut requests = Vec::new();
    let mut request_dirs = Vec::new();
    for request in 0..count {
        let section = format!("_RCMAKE_FILE_GENERATE_{request}");
        let number = |key: &str| {
            props
                .get(format!("{section}_{key}").as_bytes())
                .and_then(|value| index(value))
                .ok_or_else(invalid_record)
        };
        let dir = number("DIRECTORY")?;
        let (id, source, binary) = dirs.get(dir).ok_or_else(invalid_record)?;
        let args = (0..number("ARGC")?)
            .map(|arg| {
                props
                    .get(format!("{section}_ARG{arg}").as_bytes())
                    .map(|value| value.as_slice())
                    .ok_or_else(invalid_record)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let [b"OUTPUT", output, input_kind, input, options @ ..] = args.as_slice() else {
            return Err(invalid_record());
        };
        let policy = file_generate_policy(model, *id, request);
        let use_directory = policy == PolicyStatus::New;
        let mut request = FileGenerateRequest {
            input_base: if use_directory { source } else { &cwd },
            output_base: if use_directory { binary } else { &cwd },
            output,
            input: match *input_kind {
                b"INPUT" => FileGenerateInput::File(input),
                b"CONTENT" => FileGenerateInput::Content(input),
                _ => return Err(invalid_record()),
            },
            condition: None,
            target: None,
            permissions: FileGeneratePermissions::Default,
            newline: FileGenerateNewline::Native,
        };
        apply_options(&mut request, options)?;
        if policy == PolicyStatus::Warn {
            if !output.windows(2).any(|part| part == b"$<") {
                warn_relative_path(model, host, *id, "OUTPUT", output);
            }
            if let FileGenerateInput::File(input) = request.input {
                warn_relative_path(model, host, *id, "INPUT", input);
            }
        }
        let cmp0189 = props
            .get(format!("{section}_CMP0189").as_bytes())
            .map(|value| value.as_slice() == b"New");
        request_dirs.push((*id, origin(props, &section), policy, cmp0189));
        requests.push(request);
    }

    let request_dirs: HashMap<_, _> = requests
        .iter()
        .map(std::ptr::from_ref)
        .zip(request_dirs)
        .collect();
    let eval_contexts = file_generate_contexts(model, &result.configs);
    let contexts = eval_contexts
        .iter()
        .map(|context| FileGenerateContext {
            configuration: &context.config.0,
            language: context
                .language
                .as_ref()
                .map(|language| language.as_slice()),
        })
        .collect::<Vec<_>>();
    let mut usage = UsageResolver::new(model);
    let objects = ObjectPaths::new(&result.targets);
    let root = &model.dir(DirId::ROOT).source_dir;
    let timestamp = |format: Option<&[u8]>, utc| format_timestamp(host, format, utc);
    let mut condition_result: Option<BString> = None;
    let mut request_dir = None;
    let evaluate = |value: &[u8],
                    request: &FileGenerateRequest<'_>,
                    context: FileGenerateContext<'_>| {
        let (dir, backtrace, policy, cmp0189) = request_dirs
            .get(&std::ptr::from_ref(request))
            .ok_or_else(invalid_record)?;
        request_dir = Some(*dir);
        let mut genex_host = usage
            .host(*dir)
            .with_object_paths(&objects)
            .with_clock(host, &timestamp);
        let error = |message| GenError::Write {
            path: request.output.into(),
            message,
        };
        let target = request
            .target
            .map(|name| {
                genex_host.find_target(name).ok_or_else(|| {
                    error(format!("Unknown target \"{}\".", String::from_utf8_lossy(name)).into())
                })
            })
            .transpose()?;
        let policies: Vec<(&'static [u8], bool)> = cmp0189
            .map(|new| (b"CMP0189".as_slice(), new))
            .into_iter()
            .collect();
        let mut genex_host = CallSitePolicies {
            inner: &mut genex_host,
            policies: &policies,
        };
        let ctx = EvalContext {
            config: ConfigId(context.configuration.into()),
            language: context.language.map(BString::from),
            current: target,
            head: target,
            site: EvalSite {
                backtrace: backtrace.clone(),
                property: None,
            },
            ..EvalContext::default()
        };
        let evaluate_value = |value: &[u8], host: &mut dyn GenexHost| {
            parse(value)
                .and_then(|ast| evaluate(&ast, &ctx, host))
                .map(|(value, _)| value)
                .map_err(|err| GenError::Invalid(err.at(backtrace, root).to_string().into()))
        };
        // The shared writer asks for the output name before the condition.
        // Check the condition first and pass its result back to the writer.
        if let Some(result) = condition_result.take() {
            return Ok(result);
        }
        if std::ptr::eq(value, request.output) {
            if let Some(condition) = request.condition {
                let condition_value = evaluate_value(condition, &mut genex_host)?;
                match condition_value.as_slice() {
                    b"0" => {
                        condition_result = Some(condition_value);
                        return Ok(BString::default());
                    }
                    b"1" => condition_result = Some(condition_value),
                    _ => {
                        let message = format!(
                            "Evaluation file condition \"{}\" did not evaluate to valid content. Got \"{condition_value}\".",
                            BString::from(condition),
                        );
                        return Err(generation_error(model, *dir, &message));
                    }
                }
            }
            let output = evaluate_value(value, &mut genex_host)?;
            if *policy == PolicyStatus::Warn {
                warn_relative_path(model, host, *dir, "OUTPUT", &output);
            }
            return Ok(output);
        }
        let content = evaluate_value(value, &mut genex_host)?;
        Ok(terminate_content(request, content))
    };
    let generated = write_generated_files(&requests, &contexts, evaluate, host);
    generated.map_err(|error| collision_error(model, request_dir, error))?;
    Ok(())
}

fn terminate_content(request: &FileGenerateRequest<'_>, mut content: BString) -> BString {
    if matches!(request.input, FileGenerateInput::Content(_))
        && !matches!(request.newline, FileGenerateNewline::Native)
        && !content.is_empty()
        && !content.ends_with(b"\n")
    {
        content.push(b'\n');
    }
    content
}

fn listfile(model: &ProjectModel, dir: DirId) -> BString {
    use rcmake_base::path_lex::Lexical;

    let source = Lexical::new(&model.dir(dir).source_dir);
    let root = Lexical::new(&model.dir(DirId::ROOT).source_dir);
    let path = source
        .relative(root)
        .unwrap_or_else(|| model.dir(dir).source_dir.clone());
    if path == "." {
        return "CMakeLists.txt".into();
    }
    Lexical::new(&path).append(b"CMakeLists.txt")
}

fn warn_relative_path(model: &ProjectModel, host: &dyn Host, dir: DirId, role: &str, path: &[u8]) {
    if path.is_empty()
        || to_native(path).is_absolute()
        || model
            .definition(dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let file = listfile(model, dir);
    let body = format!(
        "Policy CMP0070 is not set: Define file(GENERATE) behavior for relative paths.  Run \"cmake --help-policy CMP0070\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n\nfile(GENERATE) given relative {role} path:\n\n  {}\n\nThis is not defined behavior unless CMP0070 is set to NEW.  For compatibility with older versions of CMake, the previous undefined behavior will be used.",
        BString::from(path)
    );
    let body = format_paragraphs(body.as_bytes(), 2, 77);
    host.write_out(
        Stream::Stderr,
        format!("CMake Warning (policy) in {file}:\n{body}\nThis warning is for project developers.  Use -Wno-author or -Wno-policy to\nsuppress it.\n\n").as_bytes(),
    );
}

fn generation_error(model: &ProjectModel, dir: DirId, message: &str) -> GenError {
    let file = listfile(model, dir);
    let body = format_paragraphs(message.as_bytes(), 2, 77);
    GenError::Invalid(format!("\nCMake Error in {file}:\n{body}\n\n").into())
}

fn collision_error(model: &ProjectModel, dir: Option<DirId>, error: GenError) -> GenError {
    match error {
        GenError::Write { path, message }
            if message == "more than one file(GENERATE) request uses the same output path" =>
        {
            GenError::Invalid(format!(
                "\nCMake Error: Files to be generated by multiple different commands: \"{path}\"\n\n"
            ).into())
        }
        GenError::Write { path, message }
            if message == "file(GENERATE) produced different content for the same output path"
                && let Some(dir) = dir =>
        {
            generation_error(model, dir, &format!(
                "Evaluation file to be written multiple times with different content. This is generally caused by the content evaluating the configuration type, language, or location of object files:\n {path}"
            ))
        }
        error => error,
    }
}

fn format_timestamp(host: &dyn Host, format: Option<&[u8]>, utc: bool) -> Option<BString> {
    let format = format
        .filter(|format| !format.is_empty())
        .unwrap_or(if utc {
            b"%Y-%m-%dT%H:%M:%SZ"
        } else {
            b"%Y-%m-%dT%H:%M:%S"
        });
    let (secs, nanos) = if let Some(epoch) = host
        .env_get(b"SOURCE_DATE_EPOCH")
        .filter(|epoch| !epoch.is_empty())
    {
        let secs = rcmake_base::parse_source_date_epoch(&epoch)?;
        if secs == -1 {
            return Some(BString::default());
        }
        (secs, 0)
    } else {
        let now = host.now();
        (now.secs, i32::try_from(now.nanos).ok()?)
    };
    let instant = Timestamp::new(secs, nanos).ok()?;
    let zone = if utc { TimeZone::UTC } else { host.time_zone() };
    let zoned = instant.to_zoned(zone);
    let mut result = BString::default();
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            result.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'%') | None => result.push(b'%'),
            Some(b'f') => result.extend_from_slice(format!("{:06}", nanos / 1000).as_bytes()),
            Some(b's') => result.extend_from_slice(secs.to_string().as_bytes()),
            Some(b'Z') if utc => result.extend_from_slice(b"GMT"),
            Some(b'Z') => result.extend_from_slice(
                zoned
                    .time_zone()
                    .to_offset_info(instant)
                    .abbreviation()
                    .as_bytes(),
            ),
            Some(
                spec @ (b'd' | b'H' | b'I' | b'j' | b'm' | b'b' | b'B' | b'M' | b'S' | b'U' | b'V'
                | b'w' | b'a' | b'A' | b'y' | b'Y' | b'z'),
            ) => result.extend_from_slice(strtime::format([b'%', spec], &zoned).ok()?.as_bytes()),
            Some(spec) => result.extend_from_slice(&[b'%', spec]),
        }
    }
    Some(result)
}

fn origin(props: &PropertyMap, section: &str) -> Backtrace {
    let frames: Vec<_> = (0..)
        .map_while(|frame| {
            let key = format!("{section}_FRAME_{frame}");
            let get = |field: &str| props.get(format!("{key}_{field}").as_bytes());
            Some(Frame {
                file: get("FILE")?.as_slice().into(),
                line: u32::try_from(index(get("LINE")?)?).ok()?,
                name: get("NAME").map(|name| name.as_slice().into()),
                kind: if get("DEFERRED").is_some() {
                    FrameKind::Deferred
                } else {
                    FrameKind::Command
                },
            })
        })
        .collect();
    frames
        .into_iter()
        .rev()
        .fold(Backtrace::default(), |trace, frame| trace.push(frame))
}

fn index(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn invalid_record() -> GenError {
    GenError::Write {
        path: "file(GENERATE)".into(),
        message: "Invalid file generation request.".into(),
    }
}

fn apply_options<'a>(
    request: &mut FileGenerateRequest<'a>,
    options: &[&'a [u8]],
) -> Result<(), GenError> {
    let mut options = options.iter().copied().peekable();
    while let Some(option) = options.next() {
        match option {
            b"CONDITION" => request.condition = Some(options.next().ok_or_else(invalid_record)?),
            b"TARGET" => request.target = Some(options.next().ok_or_else(invalid_record)?),
            b"NEWLINE_STYLE" => {
                request.newline = match options.next() {
                    Some(b"LF" | b"UNIX") => FileGenerateNewline::Lf,
                    Some(b"CRLF" | b"DOS" | b"WIN32") => FileGenerateNewline::CrLf,
                    _ => return Err(invalid_record()),
                };
            }
            b"NO_SOURCE_PERMISSIONS" => request.permissions = FileGeneratePermissions::NoSource,
            b"USE_SOURCE_PERMISSIONS" => request.permissions = FileGeneratePermissions::Default,
            b"FILE_PERMISSIONS" => {
                let mut mode = 0;
                while let Some(bits) = options.peek().and_then(|value| permission(value)) {
                    mode |= bits;
                    options.next();
                }
                request.permissions = FileGeneratePermissions::Explicit(mode);
            }
            _ => return Err(invalid_record()),
        }
    }
    Ok(())
}

fn permission(value: &[u8]) -> Option<u32> {
    Some(match value {
        b"SETUID" => 0o4000,
        b"SETGID" => 0o2000,
        b"OWNER_READ" => 0o400,
        b"OWNER_WRITE" => 0o200,
        b"OWNER_EXECUTE" => 0o100,
        b"GROUP_READ" => 0o040,
        b"GROUP_WRITE" => 0o020,
        b"GROUP_EXECUTE" => 0o010,
        b"WORLD_READ" => 0o004,
        b"WORLD_WRITE" => 0o002,
        b"WORLD_EXECUTE" => 0o001,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn explicit_newlines_terminate_nonempty_generated_content_once() {
        let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let mut request = FileGenerateRequest {
            output: b"file",
            input: FileGenerateInput::Content(b"$<1:value>"),
            input_base: work,
            output_base: work,
            condition: None,
            target: None,
            permissions: FileGeneratePermissions::Default,
            newline: FileGenerateNewline::CrLf,
        };
        assert_eq!(terminate_content(&request, "value".into()), "value\n");
        assert_eq!(terminate_content(&request, "value\n".into()), "value\n");
        assert!(terminate_content(&request, BString::default()).is_empty());
        request.newline = FileGenerateNewline::Native;
        assert_eq!(terminate_content(&request, "value".into()), "value");
    }
}
