//! `cmake_instrumentation()` (ADR-013): record an instrumentation query in
//! [`rcmake_model::GlobalState::instrumentation`].

use rcmake_base::{BString, ByteSlice as _};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_host::path;
use rcmake_instrument::{
    API_VERSION, CustomContentType, DATA_VERSION, DataVersion, Hook, InstrumentOption, Query, query,
};
use rcmake_model::{DirId, InstrumentationContent, InstrumentationRequest};

pub(crate) fn register(registry: &mut Registry) {
    crate::compat::project_command!(registry, "cmake_instrumentation", cmake_instrumentation);
}

fn cmake_instrumentation(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if !cx.defer_active() {
        return Err(crate::command_error(
            cx,
            "Cannot invoke cmake_instrumentation() within an initial cache file.",
        ));
    }
    let (mut request, query) = parse(cx, args)?;
    request.backtrace = cx.backtrace();
    let build = path::to_native(&cx.project().dir(DirId::ROOT).binary_dir);
    if cx.global().instrumentation.is_empty() {
        query::clear_generated_queries(cx.host(), &build)
            .map_err(|error| CmdError::status(error.to_string()))?;
    }
    query::write_generated_query(cx.host(), &build, &query)
        .map_err(|error| CmdError::status(error.to_string()))?;
    cx.global().instrumentation.push(request);
    Ok(Flow::Normal)
}

fn keyword(arg: &ExpandedArg) -> bool {
    matches!(
        arg.value.as_slice(),
        b"API_VERSION" | b"DATA_VERSION" | b"HOOKS" | b"OPTIONS" | b"CALLBACK" | b"CUSTOM_CONTENT"
    )
}

fn parse(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
) -> Result<(InstrumentationRequest, Query), CmdError> {
    if args.is_empty() {
        return Err(CmdError::status("must be called with arguments."));
    }
    if let Some(key) = args.iter().enumerate().find_map(|(index, arg)| {
        (keyword(arg) && args.get(index + 1).is_none_or(keyword)).then_some(&arg.value)
    }) {
        return Err(crate::command_error(
            cx,
            format!("Error after keyword {key:?}:\n\n  missing required value"),
        ));
    }
    let mut request = InstrumentationRequest::default();
    let mut query = Query::default();
    let mut rest = args;
    while let Some((key, tail)) = rest.split_first() {
        if !keyword(key) {
            return Err(CmdError::status(format!(
                "given unknown argument {:?}.",
                key.value
            )));
        }
        let count = tail.iter().take_while(|arg| !keyword(arg)).count();
        let (values, next) = tail.split_at(count);
        rest = next;
        match key.value.as_slice() {
            b"API_VERSION" | b"DATA_VERSION" => {
                if let [value] = values {
                    if key.value == b"API_VERSION" {
                        request.api_version = value.value.clone();
                    } else {
                        request.data_version = value.value.clone();
                    }
                } else if let Some(extra) = values.get(1) {
                    return Err(CmdError::status(format!(
                        "given unknown argument {:?}.",
                        extra.value
                    )));
                }
            }
            b"HOOKS" => {
                for arg in values {
                    let hook = Hook::from_name(&arg.value).ok_or_else(|| {
                        CmdError::status(format!("given invalid argument to HOOKS {:?}", arg.value))
                    })?;
                    request.hooks.push(arg.value.clone());
                    query.hooks.push(hook);
                }
            }
            b"OPTIONS" => {
                for arg in values {
                    let option = InstrumentOption::from_name(&arg.value).ok_or_else(|| {
                        CmdError::status(format!(
                            "given invalid argument to OPTIONS {:?}",
                            arg.value
                        ))
                    })?;
                    request.options.push(arg.value.clone());
                    query.options.push(option);
                }
            }
            b"CALLBACK" => request.callbacks.push(callback(values)),
            b"CUSTOM_CONTENT" => request.custom_content.push(custom_content(values)?),
            _ => {
                return Err(CmdError::status(format!(
                    "given unknown argument {:?}.",
                    key.value
                )));
            }
        }
    }
    if number(&request.api_version) != Some(API_VERSION) {
        return Err(CmdError::status(format!(
            "given an unsupported API_VERSION {:?} (the only currently supported version is {API_VERSION}).",
            request.api_version
        )));
    }
    query.version = data_version(&request.data_version)?;
    query.callbacks.clone_from(&request.callbacks);
    Ok((request, query))
}

fn data_version(value: &BString) -> Result<DataVersion, CmdError> {
    let malformed = || {
        CmdError::status(format!(
            "given a malformed DATA_VERSION {value:?}.  A numeric major or major.minor version is required."
        ))
    };
    let (major, minor) = value.split_once_str(b".").unwrap_or((value, b""));
    let major = number(major)
        .filter(|major| *major > 0)
        .ok_or_else(malformed)?;
    let minor = if minor.is_empty() {
        0
    } else {
        number(minor).ok_or_else(malformed)?
    };
    if major != DATA_VERSION.major || minor > DATA_VERSION.minor {
        return Err(CmdError::status(format!(
            "given an unsupported DATA_VERSION {value:?} (the maximum currently supported version is {}.{}).",
            DATA_VERSION.major, DATA_VERSION.minor
        )));
    }
    Ok(DataVersion { major, minor })
}

fn number(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn custom_content(values: &[ExpandedArg]) -> Result<InstrumentationContent, CmdError> {
    let [name, ty, content] = values else {
        return Err(CmdError::status("CUSTOM_CONTENT expected 3 arguments"));
    };
    match CustomContentType::from_name(&ty.value) {
        Some(CustomContentType::Json) => {
            rcmake_json::parse(&content.value, &rcmake_json::ReadOptions::default()).map_err(
                |error| {
                    let line = content
                        .value
                        .lines()
                        .nth(error.line.saturating_sub(1) as usize)
                        .unwrap_or_default();
                    let padding = (error.column.saturating_sub(1) as usize).min(line.len());
                    CmdError::status(format!(
                        "failed to parse custom content as JSON:\n\n {}\n\n{}\n\n{}^",
                        error.message,
                        BString::from(line),
                        " ".repeat(padding)
                    ))
                },
            )?;
        }
        None => {
            return Err(CmdError::status(format!(
                "got an invalid type for CUSTOM_CONTENT: {}",
                ty.value
            )));
        }
        _ => {}
    }
    Ok(InstrumentationContent {
        name: name.value.clone(),
        ty: ty.value.clone(),
        content: content.value.clone(),
    })
}

fn callback(args: &[ExpandedArg]) -> BString {
    let mut command = BString::default();
    for (i, arg) in args.iter().enumerate() {
        if i != 0 {
            command.push(b' ');
        }
        let quote = arg.value.contains(&b' ');
        if quote {
            command.push(b'"');
        }
        command.extend_from_slice(&arg.value);
        if quote {
            command.push(b'"');
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{Host, HostResult, PureHost, ReadRange, Stream};

    use super::*;

    fn configure(code: &str, expected: i32) -> HostResult<(Arc<PureHost>, Interp)> {
        let host = Arc::new(PureHost::default());
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/cmake".into(),
                cmake_command: "/cmake/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), code.as_bytes())?;
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(
            outcome.exit_code,
            expected,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        Ok((host, interp))
    }

    #[test]
    fn writes_numbered_queries_and_records_custom_content() -> HostResult<()> {
        let (host, mut interp) = configure(
            r#"
cmake_minimum_required(VERSION 4.5)
cmake_instrumentation(API_VERSION 1 DATA_VERSION 1.2
  HOOKS postGenerate manual postGenerate
  OPTIONS processMetrics trace
  CALLBACK [=["/path/tool"]=] "two words" CALLBACK other
  CUSTOM_CONTENT greeting STRING hello
  CUSTOM_CONTENT enabled BOOL OFF
  CUSTOM_CONTENT members LIST "a;;b"
  CUSTOM_CONTENT object JSON [=[{"x":1}]=])
cmake_instrumentation(DATA_VERSION 1 API_VERSION 1)
"#,
            0,
        )?;
        let generated = Path::new("/work/build/.cmake/instrumentation/v1/query/generated");
        let content = host.read(
            &generated.join("query-0.json"),
            ReadRange::All { limit: 4096 },
        )?;
        let parsed = query::parse_query(&content).expect("generated query");
        assert_eq!(parsed.version, DATA_VERSION);
        assert_eq!(
            parsed.callbacks,
            [BString::from(r#""/path/tool" "two words""#), "other".into()]
        );
        assert_eq!(parsed.hooks, [Hook::PostGenerate, Hook::Manual]);
        assert_eq!(
            parsed.options,
            [InstrumentOption::ProcessMetrics, InstrumentOption::Trace]
        );
        let mut cx = interp.cx();
        let requests = &cx.global().instrumentation;
        assert_eq!(requests.len(), 2);
        let entries: Vec<_> = requests[0]
            .custom_content
            .iter()
            .map(|entry| {
                (
                    entry.name.as_slice(),
                    entry.ty.as_slice(),
                    entry.content.as_slice(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                (
                    b"greeting".as_slice(),
                    b"STRING".as_slice(),
                    b"hello".as_slice()
                ),
                (b"enabled", b"BOOL", b"OFF"),
                (b"members", b"LIST", b"a;;b"),
                (b"object", b"JSON", br#"{"x":1}"#),
            ]
        );
        let content = host.read(
            &generated.join("query-1.json"),
            ReadRange::All { limit: 4096 },
        )?;
        assert_eq!(
            query::parse_query(&content)
                .expect("second query")
                .version
                .minor,
            0
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_arguments_before_writing() -> HostResult<()> {
        for (args, message) in [
            ("API_VERSION 0 DATA_VERSION 1", "unsupported API_VERSION"),
            ("API_VERSION 1", "malformed DATA_VERSION"),
            ("API_VERSION 1 DATA_VERSION 1.3", "unsupported DATA_VERSION"),
            ("API_VERSION 1 DATA_VERSION 1.x", "malformed DATA_VERSION"),
            (
                "API_VERSION 1 DATA_VERSION 1 HOOKS bogus",
                "invalid argument to HOOKS",
            ),
            (
                "API_VERSION 1 DATA_VERSION 1 OPTIONS bogus",
                "invalid argument to OPTIONS",
            ),
            (
                "API_VERSION 1 DATA_VERSION 1 CALLBACK",
                "missing required value",
            ),
            (
                "API_VERSION 1 DATA_VERSION 1 CUSTOM_CONTENT k text v",
                "invalid type for CUSTOM_CONTENT",
            ),
            (
                "API_VERSION 1 DATA_VERSION 1 CUSTOM_CONTENT k JSON bogus",
                "failed to parse custom content as JSON",
            ),
            (
                "API_VERSION 1 DATA_VERSION 1 CUSTOM_CONTENT k STRING",
                "CUSTOM_CONTENT expected 3 arguments",
            ),
        ] {
            let script =
                format!("cmake_minimum_required(VERSION 4.5)\ncmake_instrumentation({args})\n");
            let (host, mut interp) = configure(&script, 1)?;
            let stderr = host.stream_bytes(Stream::Stderr);
            assert!(
                String::from_utf8_lossy(&stderr).contains(message),
                "{stderr}"
            );
            assert!(interp.cx().global().instrumentation.is_empty());
            assert!(
                host.read_dir(Path::new(
                    "/work/build/.cmake/instrumentation/v1/query/generated"
                ))
                .is_err()
            );
        }
        Ok(())
    }
}
