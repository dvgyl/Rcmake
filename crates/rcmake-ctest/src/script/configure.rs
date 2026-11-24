use rcmake_base::list;
use rcmake_eval::{CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, Stream, path};

use super::{
    HandlerResult, directory, error, handler_process, prior_start, process_code, split_command,
    verbose, with_capture,
};

pub(super) fn ctest_configure(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[b"SOURCE", b"OPTIONS"],
        &[b"APPEND"],
        |cx, args| {
            let previous = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                previous.as_ref().map(|start| start.build.as_path()),
            )?;
            let source = directory(
                cx,
                args.value(b"SOURCE"),
                b"CTEST_SOURCE_DIRECTORY",
                previous.as_ref().map(|start| start.source.as_path()),
            )?;
            if !cx
                .host()
                .metadata(&source.join("CMakeLists.txt"), Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
            {
                return Err(error(
                    "called with invalid source directory. CTEST_SOURCE_DIRECTORY must be set to a directory that contains CMakeLists.txt.",
                ));
            }
            let custom = cx.var_or_empty(b"CTEST_CONFIGURE_COMMAND");
            let command = if custom.is_empty() {
                let generator = cx.var_or_empty(b"CTEST_CMAKE_GENERATOR");
                if generator.is_empty() {
                    return Err(error(
                        "requires CTEST_CMAKE_GENERATOR or CTEST_CONFIGURE_COMMAND",
                    ));
                }
                let mut command = vec![
                    cx.var_or_empty(b"CMAKE_COMMAND"),
                    [b"-S", path::from_native_cmake(&source).as_slice()]
                        .concat()
                        .into(),
                    [b"-B", path::from_native_cmake(&build).as_slice()]
                        .concat()
                        .into(),
                    [b"-G", generator.as_slice()].concat().into(),
                ];
                if let Some(options) = args.value(b"OPTIONS") {
                    command.extend(list::split(options, list::Empty::Drop));
                }
                for (option, var) in [
                    ("-A", b"CTEST_CMAKE_GENERATOR_PLATFORM".as_slice()),
                    ("-T", b"CTEST_CMAKE_GENERATOR_TOOLSET"),
                ] {
                    let value = cx.var_or_empty(var);
                    if !value.is_empty() {
                        command.extend([option.into(), value]);
                    }
                }
                command
            } else {
                split_command(cx, &custom)?
            };
            cx.host()
                .create_dir_all(&build)
                .map_err(|err| error(err.to_string()))?;
            if verbose() && !args.flag(b"QUIET") {
                cx.host().write_out(Stream::Stdout, b"Configure project\n");
            }
            let output = handler_process(cx, args, build, command)?;
            let code = process_code(&output).unwrap_or(-1);
            if code != 0 {
                cx.host()
                    .write_out(Stream::Stderr, b"Error(s) when configuring the project\n");
            }
            Ok(HandlerResult {
                code,
                capture_error: code != 0,
            })
        },
    )
}
