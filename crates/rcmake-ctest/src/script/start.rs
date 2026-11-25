use std::path::Path;

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, IncludeOptions, args::ArgCursor};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, Stream, WriteMode};

use super::{
    SESSION, Start, configuration, directory, error, prior_start, process_code, run_process,
    split_command, verbose,
};
use crate::dart::{self, Model, Tag};

pub(super) fn ctest_start(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let StartArgs {
        append,
        quiet,
        track,
        model,
        source,
        build,
    } = StartArgs::parse(args)?;
    let previous = prior_start();
    let source = directory(
        cx,
        source,
        b"CTEST_SOURCE_DIRECTORY",
        previous.as_ref().map(|start| start.source.as_path()),
    )?;
    let build = directory(
        cx,
        build,
        b"CTEST_BINARY_DIRECTORY",
        previous.as_ref().map(|start| start.build.as_path()),
    )?;
    let temporary = build.join("Testing/Temporary");
    let log = temporary.join("LastStart.log");
    cx.host()
        .create_dir_all(&temporary)
        .map_err(|err| error(err.to_string()))?;
    cx.host()
        .write(&log, b"", WriteMode::Truncate)
        .map_err(|err| error(err.to_string()))?;
    checkout(cx, &source, &log)?;
    if !cx
        .host()
        .metadata(&source, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Err(error(format!(
            "given source path \n\n  {}\n\nwhich is not an existing directory.  Set CTEST_CHECKOUT_COMMAND to a command line to create it.",
            source.display()
        )));
    }
    for directory in [&build, &source] {
        let config = directory.join("CTestConfig.cmake");
        if cx.host().metadata(&config, Follow::Yes).is_ok() {
            cx.include_file(&config, IncludeOptions::default())?;
            break;
        }
    }
    let config = configuration(cx, &build, Some(&source))?;
    let host = cx.host();
    let log = !quiet && verbose();
    if log {
        let name = if config.build_name.is_empty() {
            "(empty)".into()
        } else {
            config.build_name.to_string()
        };
        let model = model.map_or("with to-be-determined model".into(), |model| {
            format!("with model {}", model.name())
        });
        let group = track
            .as_ref()
            .map_or_else(String::new, |track| format!("   Group: {track}\n"));
        let message = format!(
            "Run dashboard {model}\n   Source directory: {}\n   Build directory: {}\n{group}   Site: {}\n   Build name: {name}\n",
            source.display(),
            build.display(),
            config.site
        );
        host.write_out(Stream::Stdout, message.as_bytes());
    }
    let tag = if append {
        let mut tag = Tag::read_with_model(host, &build, model).map_err(|err| {
            if err.os_message == dart::TAG_MISSING_MODEL {
                host.write_out(Stream::Stderr, format!("{}\n", err.os_message).as_bytes());
                error("unknown error.")
            } else {
                error(err.to_string())
            }
        })?;
        if model.is_some_and(|model| model != tag.model) {
            host.write_out(
                Stream::Stderr,
                b"Model given in TAG does not match model given in ctest_start()\n",
            );
        }
        if let Some(track) = track.filter(|track| !track.is_empty() && *track != tag.track) {
            host.write_out(
                Stream::Stderr,
                b"Group given in TAG does not match group given in ctest_start()\n",
            );
            tag.track = track;
        }
        if log {
            host.write_out(
                Stream::Stdout,
                format!("  Use existing tag: {} - {}\n", tag.stamp(), tag.track).as_bytes(),
            );
        }
        tag
    } else {
        if model == Some(Model::Nightly) && config.nightly_start.is_empty() {
            host.write_out(Stream::Stderr, b"WARNING: No nightly start time found please set in CTestConfig.cmake or DartConfig.cmake\n");
            return Err(error("unknown error."));
        }
        Tag::create(
            host,
            &build,
            host.now(),
            model.unwrap_or(Model::Experimental),
            track.as_deref(),
            &config.nightly_start,
        )
        .map_err(|err| error(err.to_string()))?
    };
    if log {
        host.write_out(
            Stream::Stdout,
            format!("   Use {} tag: {}\n", tag.track, tag.stamp()).as_bytes(),
        );
    }
    SESSION.with_borrow_mut(|session| {
        if let Some(session) = session {
            session.start = Some(Start { source, build, tag });
        }
    });
    Ok(Flow::Normal)
}

fn checkout(cx: &mut Cx<'_>, source: &Path, log: &Path) -> Result<(), CmdError> {
    let Some(command) = cx
        .var(b"CTEST_CHECKOUT_COMMAND")
        .or_else(|| cx.var(b"CTEST_CVS_CHECKOUT"))
    else {
        return Ok(());
    };
    let cwd = source.parent().unwrap_or(source);
    if cx.host().create_dir_all(cwd).is_err() {
        cx.host().write_out(
            Stream::Stderr,
            format!("Cannot create directory: {}\n", cwd.display()).as_bytes(),
        );
        return Err(error("unknown error."));
    }
    let mut record = BString::from("--- Begin Initial Checkout ---\n");
    record.extend_from_slice(&command);
    record.push(b'\n');
    let argv = split_command(cx, &command)?;
    let succeeded = argv.split_first().is_some_and(|(program, args)| {
        let host = cx.host();
        let Ok(output) = run_process(
            host,
            cwd.to_owned(),
            program.clone(),
            args.to_vec(),
            host.env_all(),
            false,
        ) else {
            return false;
        };
        for (prefix, bytes) in [(b"co-out> ", &output.stdout), (b"co-err> ", &output.stderr)] {
            for line in bytes.split_inclusive(|&byte| byte == b'\n') {
                record.extend_from_slice(prefix);
                record.extend_from_slice(line);
                if !line.ends_with(b"\n") {
                    record.push(b'\n');
                }
            }
        }
        process_code(&output) == Some(0)
    });
    record.extend_from_slice(b"--- End Initial Checkout ---\n");
    cx.host()
        .write(log, &record, WriteMode::Truncate)
        .map_err(|err| error(err.to_string()))?;
    if succeeded {
        Ok(())
    } else {
        cx.host()
            .write_out(Stream::Stderr, b"Initial checkout failed!\n");
        Err(error("unknown error."))
    }
}

struct StartArgs<'a> {
    append: bool,
    quiet: bool,
    track: Option<String>,
    model: Option<Model>,
    source: Option<&'a BString>,
    build: Option<&'a BString>,
}

impl<'a> StartArgs<'a> {
    fn parse(args: &'a [ExpandedArg]) -> Result<Self, CmdError> {
        let mut append = false;
        let mut quiet = false;
        let mut track = None;
        let mut positional = Vec::new();
        let mut args = ArgCursor::new(args);
        while let Some(arg) = args.next_arg().map(|arg| &arg.value) {
            match arg.as_slice() {
                b"APPEND" => append = true,
                b"QUIET" => quiet = true,
                b"TRACK" | b"GROUP" => {
                    track = Some(
                        args.next_arg()
                            .map(|arg| &arg.value)
                            .filter(|value| {
                                !matches!(
                                    value.as_slice(),
                                    b"APPEND" | b"QUIET" | b"TRACK" | b"GROUP"
                                )
                            })
                            .ok_or_else(|| error(format!("{arg} argument missing group name")))?
                            .to_string(),
                    );
                }
                _ => positional.push(arg),
            }
        }
        let model = positional.first().and_then(|model| Model::parse(model));
        let paths = positional
            .get(usize::from(model.is_some())..)
            .unwrap_or_default();
        if model.is_none() && !append {
            return Err(error(
                "no test model specified and APPEND not specified.  Specify either a test model or the APPEND argument",
            ));
        }
        if paths.len() > 2 {
            return Err(error("Too many arguments"));
        }
        Ok(Self {
            append,
            quiet,
            track,
            model,
            source: paths.first().copied(),
            build: paths.get(1).copied(),
        })
    }
}
