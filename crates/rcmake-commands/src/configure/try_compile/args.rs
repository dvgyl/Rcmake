use std::collections::BTreeMap;
use std::path::Component;

use rcmake_eval::CmdError;
use rcmake_host::path;

#[derive(Clone, Copy)]
pub(super) enum Source<'a> {
    File(&'a [u8]),
    Content(&'a [u8], &'a [u8]),
    Variable(&'a [u8], &'a [u8]),
    Copy(&'a [u8], &'a [u8]),
}

pub(super) enum Signature<'a> {
    Sources {
        base: Option<&'a [u8]>,
        sources: Vec<Source<'a>>,
    },
    Project {
        source: &'a [u8],
        binary: Option<&'a [u8]>,
        target: Option<&'a [u8]>,
    },
}

#[derive(Default)]
pub(super) struct Options<'a> {
    values: BTreeMap<&'a [u8], Vec<&'a [u8]>>,
}

impl<'a> Options<'a> {
    pub(super) fn has(&self, key: &[u8]) -> bool {
        self.values.contains_key(key)
    }

    pub(super) fn many(&self, key: &[u8]) -> &[&'a [u8]] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }

    pub(super) fn one(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.many(key).first().copied()
    }
}

fn keyword(arg: &[u8]) -> bool {
    option_arity(arg).is_some()
        || matches!(
            arg,
            b"SOURCES" | b"SOURCE_FROM_CONTENT" | b"SOURCE_FROM_VAR" | b"SOURCE_FROM_FILE"
        )
}

#[derive(Clone, Copy)]
enum Arity {
    Flag,
    One,
    List,
}

fn option_arity(arg: &[u8]) -> Option<Arity> {
    Some(match arg {
        b"NO_CACHE" | b"NO_LOG" => Arity::Flag,
        b"CMAKE_FLAGS" | b"COMPILE_DEFINITIONS" | b"LINK_OPTIONS" | b"LINK_LIBRARIES" | b"ARGS" => {
            Arity::List
        }
        b"OUTPUT_VARIABLE"
        | b"COPY_FILE"
        | b"COPY_FILE_ERROR"
        | b"LOG_DESCRIPTION"
        | b"LINKER_LANGUAGE"
        | b"__CMAKE_INTERNAL"
        | b"SOURCE_DIR"
        | b"BINARY_DIR"
        | b"TARGET"
        | b"SOURCES_TYPE"
        | b"COMPILE_OUTPUT_VARIABLE"
        | b"RUN_OUTPUT_VARIABLE"
        | b"RUN_OUTPUT_STDOUT_VARIABLE"
        | b"RUN_OUTPUT_STDERR_VARIABLE"
        | b"WORKING_DIRECTORY"
        | b"C_STANDARD"
        | b"C_STANDARD_REQUIRED"
        | b"C_EXTENSIONS"
        | b"CXX_STANDARD"
        | b"CXX_STANDARD_REQUIRED"
        | b"CXX_EXTENSIONS" => Arity::One,
        _ => return None,
    })
}

pub(super) fn required<'a>(
    args: &mut impl Iterator<Item = &'a [u8]>,
    name: &[u8],
) -> Result<&'a [u8], CmdError> {
    args.next().ok_or_else(|| {
        CmdError::status(format!(
            "{} requires a value.",
            String::from_utf8_lossy(name)
        ))
    })
}

pub(super) fn parse<'a>(
    args: impl Iterator<Item = &'a [u8]>,
    run: bool,
) -> Result<(Signature<'a>, Options<'a>), CmdError> {
    let mut args = args.peekable();
    let first = required(&mut args, b"Source or project")?;
    let mut options = Options::default();
    let mut sources = Vec::new();
    let mut base = None;
    let mut project = None;
    if first == b"PROJECT" && !run {
        required(&mut args, b"PROJECT")?;
        project = Some((None, None, None));
    } else if !keyword(first) {
        base = Some(first);
        let source = required(&mut args, b"Source or project")?;
        if source == b"SOURCES" {
            sources
                .extend(std::iter::from_fn(|| args.next_if(|arg| !keyword(arg))).map(Source::File));
        } else if !run && args.peek().is_some_and(|arg| !keyword(arg)) {
            args.next();
            let target = args.next_if(|arg| !keyword(arg));
            project = Some((Some(source), base, target));
        } else {
            sources.push(Source::File(source));
        }
    } else {
        parse_item(first, &mut args, &mut options, &mut sources)?;
    }
    while let Some(arg) = args.next() {
        parse_item(arg, &mut args, &mut options, &mut sources)?;
    }
    if options.has(b"COPY_FILE_ERROR") && !options.has(b"COPY_FILE") {
        return Err(CmdError::status("COPY_FILE_ERROR requires COPY_FILE."));
    }
    if run
        && (options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE"))
        && (options.has(b"RUN_OUTPUT_STDOUT_VARIABLE")
            || options.has(b"RUN_OUTPUT_STDERR_VARIABLE"))
    {
        return Err(CmdError::status(
            "Use either combined or separate run output variables.",
        ));
    }
    let signature = if let Some((source, binary, target)) = project {
        let source = source
            .or_else(|| options.one(b"SOURCE_DIR"))
            .ok_or_else(|| CmdError::status("PROJECT requires SOURCE_DIR."))?;
        if !sources.is_empty() || options.has(b"COPY_FILE") {
            return Err(CmdError::status(
                "Source files and COPY_FILE require the source file signature.",
            ));
        }
        Signature::Project {
            source,
            binary: binary.or_else(|| options.one(b"BINARY_DIR")),
            target: target.or_else(|| options.one(b"TARGET")),
        }
    } else {
        if sources.is_empty() {
            return Err(CmdError::status("Specify at least one source file."));
        }
        Signature::Sources { base, sources }
    };
    Ok((signature, options))
}

fn parse_item<'a, I: Iterator<Item = &'a [u8]>>(
    arg: &'a [u8],
    args: &mut std::iter::Peekable<I>,
    opts: &mut Options<'a>,
    sources: &mut Vec<Source<'a>>,
) -> Result<(), CmdError> {
    match arg {
        b"SOURCES" => {
            sources.extend(std::iter::from_fn(|| args.next_if(|v| !keyword(v))).map(Source::File))
        }
        b"SOURCE_FROM_CONTENT" | b"SOURCE_FROM_VAR" | b"SOURCE_FROM_FILE" => {
            let name = required(args, arg)?;
            let value = required(args, arg)?;
            let native = path::to_native(name);
            let mut parts = native.components();
            if !matches!(parts.next(), Some(Component::Normal(_)))
                || parts.next().is_some()
                || name.contains(&b'/')
                || name.contains(&b'\\')
                || name.eq_ignore_ascii_case(b"CMakeLists.txt")
            {
                return Err(CmdError::status(
                    "SOURCE_FROM_* requires a file name without directory components.",
                ));
            }
            sources.push(match arg {
                b"SOURCE_FROM_CONTENT" => Source::Content(name, value),
                b"SOURCE_FROM_VAR" => Source::Variable(name, value),
                _ => Source::Copy(name, value),
            });
        }
        _ => {
            let values = match option_arity(arg) {
                Some(Arity::Flag) => Vec::new(),
                Some(Arity::One) => vec![required(args, arg)?],
                Some(Arity::List) => std::iter::from_fn(|| args.next_if(|v| !keyword(v))).collect(),
                None => {
                    return Err(CmdError::status(format!(
                        "Unknown argument: {}",
                        String::from_utf8_lossy(arg)
                    )));
                }
            };
            opts.values.entry(arg).or_default().extend(values);
        }
    }
    Ok(())
}
