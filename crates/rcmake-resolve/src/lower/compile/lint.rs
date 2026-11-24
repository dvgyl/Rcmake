//! Select analyzers and construct the co-compile command for one source.

use rcmake_base::{BString, ByteSlice, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::EvalContext;
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target};

use super::super::{
    definition, join, launcher,
    shell::{direct, join_words, simple_args},
};
use crate::{
    Invocation, ResolveError,
    file_sets::clang_tidy_fixes_file,
    usage::{UsageResolver, handle},
};

pub(super) fn launch(
    model: &ProjectModel,
    target: &Target,
    compile: &launcher::Command<'_>,
    invocation: Invocation,
    skip_file_set_linting: Option<bool>,
    usage: &mut UsageResolver<'_>,
) -> Result<Invocation, ResolveError> {
    let launchers = compile.tool_prefix(model, target, b"_COMPILER_LAUNCHER", usage)?;
    let args = if let Some(source) = compile.source
        && matches!(
            model.generator,
            Some(
                GeneratorKind::Ninja
                    | GeneratorKind::NinjaMultiConfig
                    | GeneratorKind::UnixMakefiles
            )
        )
        && !skip_linting(model, target, source, skip_file_set_linting)
    {
        analyzers(model, target, compile, source, usage)?
    } else {
        Vec::new()
    };
    let co_compile = !args.is_empty();
    let mut prefix = Vec::new();
    if !co_compile {
        prefix.extend(launchers);
    } else {
        prefix.extend([
            definition(model, target.dir, b"CMAKE_COMMAND")
                .cloned()
                .unwrap_or_else(|| "cmake".into()),
            "-E".into(),
            "__run_co_compile".into(),
        ]);
        if !launchers.is_empty() {
            prefix.push(option(b"launcher", &escaped_list(&launchers)));
        }
        prefix.extend(args);
        prefix.push("--".into());
    }
    if prefix.is_empty() {
        return Ok(invocation);
    }
    let has_simple_argv =
        simple_args(&invocation.shell).is_some_and(|args| args == invocation.argv);
    let prefix_shell = has_simple_argv.then(|| {
        join_words(prefix.iter().enumerate().map(|(index, arg)| {
            if co_compile
                && index >= 3
                && let Some((key, value)) = arg.split_once_str(b"=")
            {
                [key, b"=", &quote(value)].concat().into()
            } else {
                quote(arg)
            }
        }))
    });
    prefix.extend(invocation.argv);
    let mut command = direct(prefix, invocation.working_dir);
    if let Some(prefix_shell) = prefix_shell {
        command.shell = join_words([prefix_shell, invocation.shell]);
    }
    Ok(command)
}

pub(super) fn rule_launcher(
    model: &ProjectModel,
    target: &Target,
    compile: &launcher::Command<'_>,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<BString>, ResolveError> {
    let property = b"RULE_LAUNCH_COMPILE";
    let value = target
        .props
        .get(property)
        .or_else(|| {
            model
                .ancestors(target.dir)
                .find_map(|dir| model.dir(dir).props.get(property))
        })
        .or_else(|| model.global.props.get(property));
    value
        .map(|value| evaluate_property(model, target, compile, property, value, usage))
        .transpose()
        .map(|value| value.filter(|value| !value.is_empty()))
}

fn evaluate_property(
    model: &ProjectModel,
    target: &Target,
    compile: &launcher::Command<'_>,
    property: &[u8],
    value: &BString,
    usage: &mut UsageResolver<'_>,
) -> Result<BString, ResolveError> {
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(value.clone());
    };
    let context = EvalContext {
        config: compile.config.clone(),
        head: Some(handle(id)),
        language: Some(compile.language.clone()),
        ..EvalContext::default()
    };
    usage.eval_ide_property(id, property, value, &context)
}

fn skip_linting(
    model: &ProjectModel,
    target: &Target,
    source: &BString,
    file_set: Option<bool>,
) -> bool {
    let property = b"SKIP_LINTING";
    file_set.unwrap_or_else(|| {
        crate::sources::properties(model.dir(target.dir), &Lexical::new(source).normalize())
            .get(property)
            .filter(|value| {
                !value.is_empty()
                    && value.as_slice() != b"NOTFOUND"
                    && !value.ends_with(b"-NOTFOUND")
            })
            .or_else(|| target.props.get(property))
            .is_some_and(|value| truth::is_on(value))
    })
}

fn analyzers(
    model: &ProjectModel,
    target: &Target,
    compile: &launcher::Command<'_>,
    source: &BString,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    let lang = compile.language.as_slice();
    let tools: &[(&[u8], &[u8])] = match lang {
        b"C" | b"CXX" => &[
            (b"iwyu", b"_INCLUDE_WHAT_YOU_USE"),
            (b"tidy", b"_CLANG_TIDY"),
            (b"pvs-studio", b"_PVS_STUDIO"),
            (b"cpplint", b"_CPPLINT"),
            (b"cppcheck", b"_CPPCHECK"),
            (b"icstat", b"_ICSTAT"),
        ],
        b"OBJC" | b"OBJCXX" => &[(b"tidy", b"_CLANG_TIDY")],
        _ => &[],
    };
    let mut args = Vec::new();
    let mut needs_source = false;
    for &(name, suffix) in tools {
        let key = BString::from([lang, suffix].concat());
        let Some(value) = target.props.get(&key) else {
            continue;
        };
        let mut command = evaluate_property(model, target, compile, &key, value, usage)?;
        if command.is_empty() {
            continue;
        }
        match name {
            b"iwyu" => {
                if !command
                    .windows(b"--driver-mode=".len())
                    .any(|arg| arg == b"--driver-mode=")
                {
                    let mode =
                        driver_mode(model, target, lang, b"_INCLUDE_WHAT_YOU_USE_DRIVER_MODE");
                    command.extend_from_slice(b";--driver-mode=");
                    command.extend_from_slice(mode);
                }
            }
            b"tidy" => {
                let mode = driver_mode(model, target, lang, b"_CLANG_TIDY_DRIVER_MODE");
                command.extend_from_slice(b";--extra-arg-before=--driver-mode=");
                command.extend_from_slice(mode);
                if let Some(path) = clang_tidy_fixes_file(model, target, lang, compile.output) {
                    command.push(b';');
                    command.extend_from_slice(&escaped(&option(b"export-fixes", &path)));
                }
            }
            b"icstat" => {
                for (prefix, default) in [
                    (
                        b"--checks=".as_slice(),
                        b";--checks=cstat_sel_checks.txt".as_slice(),
                    ),
                    (b"--db=", b";--db=cstat.db"),
                ] {
                    if !command.windows(prefix.len()).any(|part| part == prefix) {
                        command.extend_from_slice(default);
                    }
                }
                command.extend_from_slice(b";analyze");
            }
            b"pvs-studio" => {
                let compiler =
                    definition(model, target.dir, &[b"CMAKE_", lang, b"_COMPILER"].concat());
                let flag: &[u8] = if lang == b"C" { b";--cc;" } else { b";--cxx;" };
                command.extend_from_slice(flag);
                if let Some(compiler) = compiler {
                    command.extend_from_slice(&escaped(compiler));
                }
            }
            _ => {}
        }
        args.push(option(name, &command));
        if name == b"pvs-studio" {
            let object = join(&model.dir(DirId::ROOT).binary_dir, compile.output);
            args.push(option(b"object", &object));
        }
        needs_source |= name != b"iwyu";
    }
    if needs_source {
        args.push(option(b"source", source));
    }
    Ok(args)
}

fn driver_mode<'a>(
    model: &'a ProjectModel,
    target: &Target,
    language: &[u8],
    suffix: &[u8],
) -> &'a [u8] {
    definition(model, target.dir, &[b"CMAKE_", language, suffix].concat())
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                if language == b"C" {
                    b"gcc".as_slice()
                } else {
                    b"g++"
                }
            },
            |value| value.as_slice(),
        )
}

fn option(name: &[u8], value: &[u8]) -> BString {
    [b"--", name, b"=", value].concat().into()
}

fn escaped(value: &[u8]) -> BString {
    let mut result = BString::default();
    for &byte in value {
        if byte == b';' {
            result.push(b'\\');
        }
        result.push(byte);
    }
    result
}

fn escaped_list(args: &[BString]) -> BString {
    list::join(args.iter().map(|arg| escaped(arg)))
}
