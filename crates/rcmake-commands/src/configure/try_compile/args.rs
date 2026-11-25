use std::collections::{BTreeMap, BTreeSet};
use std::path::Component;

use rcmake_base::BString;
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
    pub(super) unknown: Vec<&'a [u8]>,
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

#[derive(Clone, Copy)]
struct Keywords {
    run: bool,
    project: bool,
    legacy: bool,
}

impl Keywords {
    fn contains(self, arg: &[u8]) -> bool {
        self.arity(arg).is_some() || (!self.project && source_keyword(arg))
    }

    fn arity(self, arg: &[u8]) -> Option<Arity> {
        if self.project && STANDARD_OPTIONS.contains(&arg) {
            return None;
        }
        match arg {
            b"SOURCE_DIR" | b"BINARY_DIR" | b"TARGET" if !self.project => None,
            b"ARGS"
            | b"COMPILE_OUTPUT_VARIABLE"
            | b"RUN_OUTPUT_VARIABLE"
            | b"RUN_OUTPUT_STDOUT_VARIABLE"
            | b"RUN_OUTPUT_STDERR_VARIABLE"
            | b"WORKING_DIRECTORY"
                if !self.run =>
            {
                None
            }
            b"OUTPUT_VARIABLE" if self.run && !self.legacy => None,
            b"COMPILE_DEFINITIONS"
            | b"LINK_OPTIONS"
            | b"LINK_LIBRARIES"
            | b"COPY_FILE"
            | b"COPY_FILE_ERROR"
            | b"LINKER_LANGUAGE"
            | b"SOURCES_TYPE"
                if self.project =>
            {
                None
            }
            _ => option_arity(arg),
        }
    }
}

fn source_keyword(arg: &[u8]) -> bool {
    matches!(
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
        | b"WORKING_DIRECTORY" => Arity::One,
        _ if STANDARD_OPTIONS.contains(&arg) => Arity::One,
        _ => return None,
    })
}

pub(super) const STANDARD_OPTIONS: &[&[u8]] = &[
    b"C_STANDARD",
    b"C_STANDARD_REQUIRED",
    b"C_EXTENSIONS",
    b"CXX_STANDARD",
    b"CXX_STANDARD_REQUIRED",
    b"CXX_EXTENSIONS",
    b"OBJC_STANDARD",
    b"OBJC_STANDARD_REQUIRED",
    b"OBJC_EXTENSIONS",
    b"OBJCXX_STANDARD",
    b"OBJCXX_STANDARD_REQUIRED",
    b"OBJCXX_EXTENSIONS",
];

pub(super) fn parse<'a>(
    first: &'a [u8],
    args: impl Iterator<Item = &'a [u8]>,
    run: bool,
) -> Result<(Signature<'a>, Options<'a>), BString> {
    let mut args = args.peekable();
    let new_project = first == b"PROJECT" && !run;
    let legacy = !new_project && !source_keyword(first);
    let keywords = Keywords {
        run,
        project: new_project,
        legacy,
    };
    let mut options = Options::default();
    let mut sources = Vec::new();
    let mut errors = BTreeMap::new();
    let mut base = None;
    let mut project = None;
    if new_project {
        args.next();
        project = Some((None, None, None));
    } else if legacy {
        base = Some(first);
        let source = args.next().ok_or("Specify at least one source file.")?;
        if source_keyword(source) {
            parse_item(
                source,
                &mut args,
                keywords,
                &mut options,
                &mut sources,
                &mut errors,
            )?;
        } else if !run && args.peek().is_some_and(|arg| !keywords.contains(arg)) {
            args.next();
            let target = args.next_if(|arg| !keywords.contains(arg));
            project = Some((Some(source), base, target));
        } else {
            sources.push(Source::File(source));
        }
    } else {
        parse_item(
            first,
            &mut args,
            keywords,
            &mut options,
            &mut sources,
            &mut errors,
        )?;
    }
    while let Some(arg) = args.next() {
        parse_item(
            arg,
            &mut args,
            keywords,
            &mut options,
            &mut sources,
            &mut errors,
        )?;
    }
    if !errors.is_empty() {
        let mut text = errors
            .into_iter()
            .map(|(keyword, reasons)| {
                let reasons = reasons.into_iter().collect::<Vec<_>>().join("\n  ");
                format!(
                    "Error after keyword \"{}\":\n  {reasons}",
                    String::from_utf8_lossy(keyword)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        return Err(text.into());
    }
    if options.one(b"COPY_FILE").is_some_and(<[u8]>::is_empty) {
        return Err("COPY_FILE must be followed by a file path".into());
    }
    if options
        .one(b"COPY_FILE_ERROR")
        .is_some_and(<[u8]>::is_empty)
    {
        return Err("COPY_FILE_ERROR must be followed by a variable name".into());
    }
    if options.has(b"COPY_FILE_ERROR") && !options.has(b"COPY_FILE") {
        return Err("COPY_FILE_ERROR may be used only with COPY_FILE".into());
    }
    if run
        && (options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE"))
        && (options.has(b"RUN_OUTPUT_STDOUT_VARIABLE")
            || options.has(b"RUN_OUTPUT_STDERR_VARIABLE"))
    {
        return Err("Use either combined or separate run output variables.".into());
    }
    let signature = if let Some((source, binary, target)) = project {
        let source = source
            .or_else(|| options.one(b"SOURCE_DIR"))
            .filter(|source| !source.is_empty())
            .ok_or("No <srcdir> specified.")?;
        for key in [b"COMPILE_DEFINITIONS".as_slice(), b"COPY_FILE"] {
            if options.has(key) {
                return Err(format!(
                    "{} allowed only in source file signature",
                    String::from_utf8_lossy(key)
                )
                .into());
            }
        }
        Signature::Project {
            source,
            binary: binary.or_else(|| options.one(b"BINARY_DIR")),
            target: target.or_else(|| options.one(b"TARGET")),
        }
    } else {
        if sources.is_empty() {
            return Err("Specify at least one source file.".into());
        }
        Signature::Sources { base, sources }
    };
    Ok((signature, options))
}

fn parse_item<'a, I: Iterator<Item = &'a [u8]>>(
    arg: &'a [u8],
    args: &mut std::iter::Peekable<I>,
    keywords: Keywords,
    opts: &mut Options<'a>,
    sources: &mut Vec<Source<'a>>,
    errors: &mut BTreeMap<&'a [u8], BTreeSet<&'static str>>,
) -> Result<(), BString> {
    let mut values = || args.next_if(|value| !keywords.contains(value));
    match arg {
        _ if !keywords.contains(arg) => opts.unknown.push(arg),
        b"SOURCES" => {
            if let Some(first) = values() {
                sources.extend(
                    std::iter::once(first)
                        .chain(std::iter::from_fn(values))
                        .map(Source::File),
                );
            } else {
                errors
                    .entry(arg)
                    .or_default()
                    .insert("missing required value");
            }
        }
        b"SOURCE_FROM_CONTENT" | b"SOURCE_FROM_VAR" | b"SOURCE_FROM_FILE" => {
            let (Some(name), Some(value), None) = (values(), values(), values()) else {
                return Err(format!(
                    "{} requires exactly two arguments",
                    String::from_utf8_lossy(arg)
                )
                .into());
            };
            let native = path::to_native(name);
            let mut parts = native.components();
            if !matches!(parts.next(), Some(Component::Normal(_)))
                || parts.next().is_some()
                || name.contains(&b'/')
                || name.contains(&b'\\')
                || name.eq_ignore_ascii_case(b"CMakeLists.txt")
            {
                return Err(format!(
                    "{} given invalid filename \"{}\"",
                    String::from_utf8_lossy(arg),
                    String::from_utf8_lossy(name)
                )
                .into());
            }
            sources.push(match arg {
                b"SOURCE_FROM_CONTENT" => Source::Content(name, value),
                b"SOURCE_FROM_VAR" => Source::Variable(name, value),
                _ => Source::Copy(name, value),
            });
        }
        _ => {
            let entry = opts.values.entry(arg).or_default();
            match keywords.arity(arg) {
                Some(Arity::One) => match values() {
                    Some(b"") if arg == b"LOG_DESCRIPTION" => {
                        errors
                            .entry(arg)
                            .or_default()
                            .insert("empty string not allowed");
                    }
                    Some(value) => {
                        entry.clear();
                        entry.push(value);
                    }
                    None => {
                        errors
                            .entry(arg)
                            .or_default()
                            .insert("missing required value");
                    }
                },
                Some(Arity::List) => entry.extend(std::iter::from_fn(values)),
                _ => {}
            }
        }
    }
    Ok(())
}
