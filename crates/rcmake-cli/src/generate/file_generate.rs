use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_gen_common::{
    GenError,
    export::{
        FileGenerateContext, FileGenerateInput, FileGenerateNewline, FileGeneratePermissions,
        FileGenerateRequest, write_generated_files,
    },
};
use rcmake_genex::{ConfigId, EvalContext, GenexHost, evaluate, parse};
use rcmake_host::{Host, path::to_native};
use rcmake_model::ProjectModel;
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

pub(super) fn write(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
) -> Result<(), GenError> {
    let props = &model.global.props;
    let Some(count) = props.get(b"_RCMAKE_FILE_GENERATE_COUNT") else {
        return Ok(());
    };
    let count = index(count).ok_or_else(invalid_record)?;
    let dirs: Vec<_> = model
        .dirs()
        .map(|(id, dir)| {
            (
                id,
                dir,
                to_native(&dir.source_dir),
                to_native(&dir.binary_dir),
            )
        })
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
        let (id, directory, source, binary) = dirs.get(dir).ok_or_else(invalid_record)?;
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
        let use_directory = directory
            .final_policies
            .as_ref()
            .is_some_and(|policies| policies.get(PolicyId::new(70)) == PolicyStatus::New);
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
        request_dirs.push(*id);
        requests.push(request);
    }

    let request_dirs: HashMap<_, _> = requests
        .iter()
        .map(std::ptr::from_ref)
        .zip(request_dirs)
        .collect();
    let mut languages: Vec<_> = result
        .metadata
        .toolchains
        .keys()
        .map(|lang| Some(lang.as_slice()))
        .collect();
    if languages.is_empty() {
        languages.push(None);
    }
    let contexts = result
        .configs
        .iter()
        .flat_map(|config| {
            languages.iter().map(|&language| FileGenerateContext {
                configuration: &config.0,
                language,
            })
        })
        .collect::<Vec<_>>();
    let mut usage = UsageResolver::new(model);
    let evaluate = |value: &[u8],
                    request: &FileGenerateRequest<'_>,
                    context: FileGenerateContext<'_>| {
        let dir = request_dirs
            .get(&std::ptr::from_ref(request))
            .ok_or_else(invalid_record)?;
        let mut host = usage.host(*dir);
        let error = |message| GenError::Write {
            path: request.output.into(),
            message,
        };
        let target = request
            .target
            .map(|name| {
                host.find_target(name).ok_or_else(|| {
                    error(format!("Unknown target \"{}\".", String::from_utf8_lossy(name)).into())
                })
            })
            .transpose()?;
        let ctx = EvalContext {
            config: ConfigId(context.configuration.into()),
            language: context.language.map(BString::from),
            current: target,
            head: target,
            ..EvalContext::default()
        };
        parse(value)
            .and_then(|ast| evaluate(&ast, &ctx, &mut host))
            .map(|(value, _)| value)
            .map_err(|err| error(err.to_string().into()))
    };
    write_generated_files(&requests, &contexts, evaluate, host)?;
    Ok(())
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
