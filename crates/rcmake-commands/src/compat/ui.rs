use rcmake_base::{BString, truth};
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native, to_native};
use rcmake_model::{BT, PropOrigin, PropScope, TargetBinding};
use rcmake_syntax::{ArgKind, Argument};

use super::incorrect_arguments;
use crate::custom::{new_command, record};

const FLTK_TARGET_CHECK: &[u8] = b"\0";

pub(super) fn fltk(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [target, inputs @ ..] = args else {
        return Err(incorrect_arguments());
    };
    if inputs.is_empty() {
        return Err(incorrect_arguments());
    }
    if target.value == FLTK_TARGET_CHECK
        && let [actual] = inputs
        && cx.running_deferred()
    {
        check_fltk_target(cx, &actual.value);
        return Ok(Flow::Normal);
    }
    let fluid = required(cx, b"FLTK_FLUID_EXECUTABLE");
    let backtrace = cx.backtrace();
    let binary = from_native(&cx.current_binary_dir());
    crate::dirs::record_include_directories(cx, &[BT::new(binary, backtrace)], &[], false);
    let mut sources = BString::default();
    for arg in inputs {
        if source(cx, &arg.value).is_none() {
            continue;
        }
        let source_dir = from_native(&cx.current_source_dir());
        let input = BString::from([source_dir.as_slice(), b"/", &arg.value].concat());
        let base = file_stem(&arg.value, false);
        let header = output(cx, b"", base, b".h");
        let source = output(cx, b"", base, b".cxx");
        add_rule(
            cx,
            [&source, &header],
            [&input, &fluid],
            &[&fluid, b"-c", b"-h", &header, b"-o", &source, &input],
            None,
        )?;
        append(&mut sources, &source);
    }
    let var = [target.value.as_slice(), b"_FLTK_UI_SRCS"].concat();
    cx.set_var(&var, sources);
    if has_fltk_target(cx, &target.value) {
        return Ok(Flow::Normal);
    }
    let line = cx.current_line();
    let before_generate = [FLTK_TARGET_CHECK, &target.value].map(|text| Argument {
        kind: ArgKind::Bracket,
        text: text.into(),
        line,
        span: 0..0,
    });
    let id = cx.defer_new_id();
    if !cx.defer_call(cx.current_dir_id(), id, b"fltk_wrap_ui", &before_generate) {
        check_fltk_target(cx, &target.value);
    }
    Ok(Flow::Normal)
}

fn check_fltk_target(cx: &mut Cx<'_>, target: &[u8]) {
    if !has_fltk_target(cx, target) {
        let source = from_native(&cx.current_source_dir());
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            [
                b"FLTK_WRAP_UI was called with a target that was never created: ".as_slice(),
                target,
                b".  The problem was found while processing the source directory: ",
                &source,
                b".  This FLTK_WRAP_UI call will be ignored.",
            ]
            .concat()
            .into(),
        );
    }
}

fn has_fltk_target(cx: &mut Cx<'_>, target: &[u8]) -> bool {
    let dir = cx.current_dir_id();
    let project = cx.project();
    let Some(TargetBinding::Target(id)) = project.lookup_target(dir, target) else {
        return false;
    };
    project.target(id).is_some_and(|target| target.dir == dir)
}

pub(super) fn qt_cpp(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [_, out, inputs @ ..] = args else {
        return Err(incorrect_arguments());
    };
    if inputs.is_empty() {
        return Err(incorrect_arguments());
    }
    let moc = required(cx, b"QT_MOC_EXECUTABLE");
    let mut sources = cx.var_or_empty(&out.value);
    for arg in inputs {
        let Some(input) = source(cx, &arg.value) else {
            continue;
        };
        let generated = output(cx, b"moc_", file_stem(&arg.value, true), b".cxx");
        add_rule(
            cx,
            [&generated],
            [&moc, &input],
            &[&moc, b"-o", &generated, &input],
            Some("Qt Wrapped File"),
        )?;
        if let Some(value) = source_property(cx, &input, b"ABSTRACT") {
            let dir = cx.current_dir_id();
            let backtrace = cx.backtrace();
            cx.project()
                .set_prop(
                    &PropScope::Source {
                        dir,
                        path: generated.clone(),
                    },
                    b"ABSTRACT",
                    Some(value),
                    PropOrigin {
                        dir,
                        backtrace: &backtrace,
                    },
                )
                .map_err(|error| CmdError::Status(error.to_string().into()))?;
        }
        append(&mut sources, &generated);
    }
    cx.set_var(&out.value, sources);
    Ok(Flow::Normal)
}

pub(super) fn qt_ui(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [_, headers_var, sources_var, inputs @ ..] = args else {
        return Err(incorrect_arguments());
    };
    if inputs.is_empty() {
        return Err(incorrect_arguments());
    }
    let uic = required(cx, b"QT_UIC_EXECUTABLE");
    let moc = required(cx, b"QT_MOC_EXECUTABLE");
    let mut headers = cx.var_or_empty(&headers_var.value);
    let mut sources = cx.var_or_empty(&sources_var.value);
    for arg in inputs {
        let Some(input) = source(cx, &arg.value) else {
            continue;
        };
        let base = file_stem(&arg.value, true);
        let header = output(cx, b"", base, b".h");
        let cpp = output(cx, b"", base, b".cxx");
        let moc_cpp = output(cx, b"moc_", base, b".cxx");
        add_rule(
            cx,
            [&header],
            [&input],
            &[&uic, b"-o", &header, &input],
            None,
        )?;
        add_rule(
            cx,
            [&cpp],
            [&input, &header],
            &[&uic, b"-impl", &header, b"-o", &cpp, &input],
            None,
        )?;
        add_rule(
            cx,
            [&moc_cpp],
            [&header],
            &[&moc, b"-o", &moc_cpp, &header],
            None,
        )?;
        append(&mut headers, &header);
        append(&mut sources, &cpp);
        append(&mut sources, &moc_cpp);
    }
    cx.set_var(&sources_var.value, sources);
    cx.set_var(&headers_var.value, headers);
    Ok(Flow::Normal)
}

fn required(cx: &mut Cx<'_>, name: &[u8]) -> BString {
    if let Some(value) = cx.var(name) {
        return value;
    }
    cx.issue(
        MessageKind::Error,
        Effect::SetError,
        None,
        [
            b"Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\n"
                .as_slice(),
            name,
        ]
        .concat()
        .into(),
    );
    BString::default()
}

fn source(cx: &mut Cx<'_>, arg: &[u8]) -> Option<BString> {
    let path = to_native(arg);
    let mut input = from_native(&cx.current_source_dir().join(&path));
    if !path.is_absolute() {
        let generated = from_native(&cx.current_binary_dir().join(&path));
        if source_property(cx, &generated, b"GENERATED").is_some_and(|value| truth::is_on(&value)) {
            input = generated;
        }
    }
    let excluded =
        source_property(cx, &input, b"WRAP_EXCLUDE").is_some_and(|value| truth::is_on(&value));
    (!excluded).then_some(input)
}

fn source_property(cx: &mut Cx<'_>, path: &[u8], property: &[u8]) -> Option<BString> {
    let dir = cx.current_dir_id();
    cx.project().get_prop(
        &PropScope::Source {
            dir,
            path: path.into(),
        },
        property,
    )
}

fn file_stem(path: &[u8], last_extension: bool) -> &[u8] {
    let file = path
        .rsplit(|byte| matches!(byte, b'/' | b'\\'))
        .next()
        .unwrap_or_default();
    let dot = |byte: &u8| *byte == b'.';
    let end = if last_extension {
        file.iter().rposition(dot)
    } else {
        file.iter().position(dot)
    };
    file.get(..end.unwrap_or(file.len())).unwrap_or(file)
}

fn output(cx: &Cx<'_>, prefix: &[u8], base: &[u8], suffix: &[u8]) -> BString {
    let name = [prefix, base, suffix].concat();
    from_native(&cx.current_binary_dir().join(to_native(&name)))
}

fn append(list: &mut BString, value: &[u8]) {
    if !list.is_empty() {
        list.push(b';');
    }
    list.extend_from_slice(value);
}

fn add_rule(
    cx: &mut Cx<'_>,
    outputs: impl IntoIterator<Item = impl AsRef<[u8]>>,
    depends: impl IntoIterator<Item = impl AsRef<[u8]>>,
    command: &[&[u8]],
    comment: Option<&str>,
) -> Result<(), CmdError> {
    let mut body = new_command(cx);
    let entry = |path: &[u8]| BT::new(path.into(), body.backtrace.clone());
    body.outputs = outputs
        .into_iter()
        .map(|path| entry(path.as_ref()))
        .collect();
    body.depends = depends
        .into_iter()
        .map(|path| entry(path.as_ref()))
        .collect();
    body.command_lines = vec![command.iter().copied().map(BString::from).collect()];
    body.comment = comment.map(BString::from);
    record(cx, body)?;
    Ok(())
}
