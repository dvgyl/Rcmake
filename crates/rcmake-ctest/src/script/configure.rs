use rcmake_base::{ByteSlice as _, list};
use rcmake_commands::GeneratorKind;
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
        &[b"SOURCE", b"OPTIONS", b"PRESET", b"PRESETS_FILE"],
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
            let preset =
                super::preset::select(cx, args, &source, rcmake_presets::PresetKind::Configure)?;
            let custom = cx.var_or_empty(b"CTEST_CONFIGURE_COMMAND");
            let cwd = if preset.is_some() {
                source.clone()
            } else {
                build.clone()
            };
            let command = if custom.is_empty() || preset.is_some() {
                let generator = cx.var_or_empty(b"CTEST_CMAKE_GENERATOR");
                if generator.is_empty() && preset.is_none() {
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
                ];
                if let Some(preset) = preset {
                    preset.append_to(&mut command);
                } else {
                    command.push([b"-G", generator.as_slice()].concat().into());
                }
                let options = args.value(b"OPTIONS");
                if let Some(options) = options {
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
                let config = cx.var_or_empty(b"CTEST_CONFIGURATION_TYPE");
                if !config.is_empty()
                    && !GeneratorKind::from_name(&generator)
                        .is_some_and(GeneratorKind::multi_config)
                    && !options.is_some_and(|options| {
                        options.contains_str(b"CMAKE_BUILD_TYPE=")
                            || options.contains_str(b"CMAKE_BUILD_TYPE:STRING=")
                    })
                {
                    command.push(
                        [b"-DCMAKE_BUILD_TYPE:STRING=".as_slice(), &config]
                            .concat()
                            .into(),
                    );
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
            let output = handler_process(cx, args, cwd, &build, command)?;
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
