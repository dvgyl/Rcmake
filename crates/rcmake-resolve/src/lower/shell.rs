//! Process arguments and POSIX shell command lines.

use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_model::{DirId, ProjectModel};

use super::{definition, expand_rule};
use crate::Invocation;

pub(super) fn join_words(words: impl IntoIterator<Item = BString>) -> BString {
    let mut result = BString::default();
    for word in words.into_iter().filter(|word| !word.is_empty()) {
        if !result.is_empty() {
            result.push(b' ');
        }
        result.extend_from_slice(&word);
    }
    result
}

/// Quote an argument as CMake does for a POSIX shell.
#[must_use]
pub fn cmake_posix_quote(argument: &[u8]) -> BString {
    let quoted = argument.is_empty()
        || argument
            .iter()
            .any(|byte| b" \t\r\n'`;#&$()~<>|*^\\".contains(byte));
    let mut result = BString::default();
    if quoted {
        result.push(b'"');
    }
    for &byte in argument {
        if matches!(byte, b'\\' | b'"' | b'`' | b'$') {
            result.push(b'\\');
        }
        result.push(byte);
    }
    if quoted {
        result.push(b'"');
    }
    result
}

/// Return whether CMake passes a custom command argument to the shell as an operator.
#[must_use]
pub fn custom_shell_operator(argument: &[u8]) -> bool {
    matches!(
        argument,
        b"<" | b">"
            | b"<<"
            | b">>"
            | b"|"
            | b"||"
            | b"&&"
            | b"&>"
            | b"1>"
            | b"2>"
            | b"2>&1"
            | b"1>&2"
    )
}

pub(super) fn ninja_custom(
    argv: Vec<BString>,
    working_dir: Option<BString>,
    verbatim: bool,
) -> Invocation {
    let shell = join_words(argv.iter().enumerate().map(|(index, arg)| {
        if index == 0 {
            return cmake_posix_quote(arg);
        }
        if custom_shell_operator(arg) {
            return arg.clone();
        }
        if verbatim {
            return cmake_posix_quote(arg);
        }
        let mut escaped = BString::default();
        for &byte in arg.iter() {
            if byte == b' ' {
                escaped.push(b'\\');
            }
            escaped.push(byte);
        }
        escaped
    }));
    Invocation {
        argv,
        shell,
        working_dir,
        link_response: None,
    }
}

pub(super) fn windows_host(model: &ProjectModel, dir: DirId) -> bool {
    definition(model, dir, b"CMAKE_HOST_SYSTEM_NAME").map_or_else(
        || {
            definition(model, dir, b"CMAKE_HOST_WIN32")
                .map_or(cfg!(windows), |value| truth::is_on(value))
        },
        |value| value.eq_ignore_ascii_case(b"Windows"),
    )
}

pub(super) fn windows_arguments(line: &[u8]) -> Vec<BString> {
    let mut args = Vec::new();
    let mut input = line.iter().copied().peekable();
    while let Some(byte) = input.peek() {
        if byte.is_ascii_whitespace() {
            input.next();
            continue;
        }
        let mut word = BString::default();
        let mut quoted = false;
        while let Some(byte) = input.peek().copied() {
            if !quoted && byte.is_ascii_whitespace() {
                break;
            }
            let mut slashes = 0;
            while input.next_if_eq(&b'\\').is_some() {
                slashes += 1;
            }
            if input.next_if_eq(&b'"').is_some() {
                word.extend(std::iter::repeat_n(b'\\', slashes / 2));
                if slashes % 2 == 1 || (quoted && input.next_if_eq(&b'"').is_some()) {
                    word.push(b'"');
                } else {
                    quoted = !quoted;
                }
            } else {
                word.extend(std::iter::repeat_n(b'\\', slashes));
                if slashes == 0
                    && let Some(byte) = input.next()
                {
                    word.push(byte);
                }
            }
        }
        args.push(word);
    }
    args
}

pub(super) fn raw_flags(model: &ProjectModel, dir: DirId, value: &[u8]) -> BString {
    if windows_host(model, dir) {
        join_words(windows_arguments(value).iter().map(|arg| quote(arg)))
    } else {
        value.into()
    }
}

pub(super) fn direct(argv: Vec<BString>, working_dir: Option<BString>) -> Invocation {
    direct_with_operators(argv, working_dir, |_| false)
}

pub(super) fn make_custom(
    argv: Vec<BString>,
    working_dir: Option<BString>,
    verbatim: bool,
) -> Invocation {
    direct_with_operators(argv, working_dir, |arg| {
        custom_shell_operator(arg) || (!verbatim && arg == b";")
    })
}

fn direct_with_operators(
    argv: Vec<BString>,
    working_dir: Option<BString>,
    is_operator: impl Fn(&[u8]) -> bool,
) -> Invocation {
    let shell = join_words(argv.iter().enumerate().map(|(index, arg)| {
        if index != 0 && is_operator(arg) {
            arg.clone()
        } else {
            quote(arg)
        }
    }));
    Invocation {
        argv,
        shell,
        working_dir,
        link_response: None,
    }
}

pub(super) fn command(shell: BString) -> Invocation {
    let argv = simple_args(&shell)
        .filter(|args| {
            args.first()
                .is_some_and(|program| !is_shell_prefix(program))
        })
        .unwrap_or_else(|| vec!["sh".into(), "-c".into(), shell.clone()]);
    Invocation {
        argv,
        shell,
        working_dir: None,
        link_response: None,
    }
}

fn is_shell_prefix(program: &[u8]) -> bool {
    matches!(
        program,
        b"!" | b"." | b":" | b"cd" | b"exec" | b"exit" | b"export" | b"set" | b"umask" | b"unset"
    ) || program.iter().position(|&b| b == b'=').is_some_and(|end| {
        program.get(..end).is_some_and(|name| {
            name.first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        })
    })
}

/// Read a simple shell command. Require the shell when an expansion or operator is present.
pub fn simple_args(shell: &[u8]) -> Option<Vec<BString>> {
    parse_simple_args(shell, |_, _| {})
}

pub(super) fn simple_args_spanned(
    shell: &[u8],
) -> Option<(Vec<BString>, Vec<std::ops::Range<usize>>)> {
    let mut ranges = Vec::new();
    let args = parse_simple_args(shell, |start, end| ranges.push(start..end))?;
    Some((args, ranges))
}

fn parse_simple_args(shell: &[u8], mut span: impl FnMut(usize, usize)) -> Option<Vec<BString>> {
    let mut args = Vec::new();
    let mut start = 0;
    let mut word = BString::default();
    let mut active = false;
    let mut delimiter = None;
    let mut bytes = shell.iter().copied().enumerate();
    while let Some((index, byte)) = bytes.next() {
        if !active {
            start = index;
        }
        match (delimiter, byte) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => delimiter = None,
            (Some(b'\''), _) => word.push(byte),
            (None, b'\'' | b'"') => {
                delimiter = Some(byte);
                active = true;
            }
            (_, b'\\') => {
                let (_, next) = bytes.next()?;
                if next != b'\n' {
                    if delimiter.is_some() && !matches!(next, b'$' | 0x60 | 0x5c | b'"') {
                        word.push(byte);
                    }
                    word.push(next);
                    active = true;
                }
            }
            (_, b'$' | b'`')
            | (None, b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'*' | b'?' | b'[' | b'\n') =>
            {
                return None;
            }
            (None, b'#' | b'~') if !active => return None,
            (None, b' ' | b'\t') => {
                if active {
                    args.push(std::mem::take(&mut word));
                    span(start, index);
                    active = false;
                }
            }
            _ => {
                word.push(byte);
                active = true;
            }
        }
    }
    if delimiter.is_some() {
        return None;
    }
    if active {
        args.push(word);
        span(start, shell.len());
    }
    Some(args)
}

/// Read simple commands separated by an unquoted `&&`.
pub fn rule_command_chain(shell: &[u8]) -> Option<Vec<Vec<BString>>> {
    let mut input = shell.iter().copied().enumerate().peekable();
    let mut quote = None;
    let mut start = 0;
    let mut commands = Vec::new();
    while let Some((index, byte)) = input.next() {
        match (quote, byte) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => quote = None,
            (Some(b'\''), _) => {}
            (None, b'\'' | b'"') => quote = Some(byte),
            (_, b'\\') => {
                input.next()?;
            }
            (None, b'&') if input.peek().is_some_and(|(_, byte)| *byte == b'&') => {
                let (next, _) = input.next()?;
                let args = simple_args(shell.get(start..index)?)?;
                if args.first().is_none_or(|program| is_shell_prefix(program)) {
                    return None;
                }
                commands.push(args);
                start = next + 1;
            }
            _ => {}
        }
    }
    let args = simple_args(shell.get(start..)?)?;
    if args.first().is_none_or(|program| is_shell_prefix(program)) {
        return None;
    }
    commands.push(args);
    Some(commands)
}

pub(super) fn rule_invocations(
    model: &ProjectModel,
    dir: DirId,
    template: &[u8],
    lookup: impl Fn(&[u8]) -> Option<BString>,
) -> Vec<Invocation> {
    let expander = RuleExpander {
        model,
        dir,
        // Compile and scan callers bind SOURCE. Link and archive callers bind TARGET.
        sysroot: if lookup(b"SOURCE").is_some() {
            b"CMAKE_SYSROOT_COMPILE"
        } else {
            b"CMAKE_SYSROOT_LINK"
        },
    };
    list::split(template, list::Empty::Drop)
        .into_iter()
        .map(|rule| {
            expand_rule(&rule, |key| {
                lookup(key).or_else(|| {
                    if key == b"CUDA_COMPILE_MODE" {
                        Some(expander.cuda_compile_mode(lookup(b"TARGET_NAME")))
                    } else {
                        expander.value(key)
                    }
                })
            })
        })
        .filter(|shell| shell.iter().any(|b| !b.is_ascii_whitespace()))
        .map(command)
        .collect()
}

struct RuleExpander<'a> {
    model: &'a ProjectModel,
    dir: DirId,
    sysroot: &'static [u8],
}

impl RuleExpander<'_> {
    fn definition(&self, key: &[u8]) -> Option<&[u8]> {
        definition(self.model, self.dir, key).map(|value| value.as_slice())
    }

    fn cuda_compile_mode(&self, target_name: Option<BString>) -> BString {
        let target = target_name
            .and_then(|name| simple_args(&name))
            .and_then(|args| self.model.find_target(self.dir, args.first()?))
            .and_then(|id| self.model.target(id));
        let enabled = |name: &[u8]| {
            target
                .and_then(|target| target.props.get(name))
                .is_some_and(|value| truth::is_on(value))
        };
        let clang = self.definition(b"CMAKE_CUDA_COMPILER_ID") == Some(b"Clang".as_slice());
        let rdc = enabled(b"CUDA_SEPARABLE_COMPILATION").then(|| {
            self.definition(b"_CMAKE_CUDA_RDC_FLAG")
                .unwrap_or(if clang { b"-fgpu-rdc" } else { b"-rdc=true" })
        });
        let (mode, fallback): (&[u8], &[u8]) = [
            (
                b"PTX".as_slice(),
                if clang {
                    b"--cuda-device-only -S".as_slice()
                } else {
                    b"-ptx"
                },
            ),
            (b"CUBIN", b"-cubin"),
            (b"FATBIN", b"-fatbin"),
            (b"OPTIX", b"-optix-ir"),
        ]
        .into_iter()
        .find(|(mode, _)| enabled(&[b"CUDA_", *mode, b"_COMPILATION"].concat()))
        .unwrap_or((b"WHOLE", b"-c"));
        let flag = self
            .definition(&[b"_CMAKE_CUDA_", mode, b"_FLAG"].concat())
            .unwrap_or(fallback);
        join_words(rdc.into_iter().chain([flag]).map(BString::from))
    }

    fn value(&self, key: &[u8]) -> Option<BString> {
        let value = self.definition(key);
        match key {
            b"CMAKE_AR" => {
                let msvc = [b"C".as_slice(), b"CXX"]
                    .into_iter()
                    .any(|language| super::artifacts::is_msvc(self.model, self.dir, language));
                Some(quote(
                    value
                        .filter(|value| !truth::is_off(value))
                        .unwrap_or(if msvc { b"lib.exe" } else { b"ar" }),
                ))
            }
            b"CMAKE_RANLIB" => Some(quote(value.unwrap_or(b"ranlib"))),
            b"CMAKE_COMMAND" => Some(quote(value.unwrap_or(b"cmake"))),
            b"CMAKE_RC_COMPILER"
                if self.sysroot == b"CMAKE_SYSROOT_COMPILE"
                    && value.is_some_and(|value| !truth::is_off(value)) =>
            {
                Some(self.compiler(key, b"CMAKE_RC"))
            }
            b"CMAKE_LINKER" | b"CMAKE_MT" | b"CMAKE_RC_COMPILER" => {
                let fallback = match key {
                    b"CMAKE_LINKER" => b"link.exe".as_slice(),
                    b"CMAKE_MT" => b"mt.exe",
                    _ => b"rc.exe",
                };
                Some(quote(
                    value
                        .filter(|value| !truth::is_off(value))
                        .unwrap_or(fallback),
                ))
            }
            b"MANIFESTS" => Some(BString::default()),
            _ => {
                if let Some(prefix) = key
                    .strip_suffix(b"_COMPILER")
                    .filter(|key| key.starts_with(b"CMAKE_"))
                {
                    return Some(self.compiler(key, prefix));
                }
                value.map(BString::from)
            }
        }
    }

    fn compiler(&self, key: &[u8], prefix: &[u8]) -> BString {
        let lookup = |suffix: &[u8]| self.definition(&[prefix, suffix].concat());
        let mut words = vec![quote(self.definition(key).unwrap_or({
            if matches!(key, b"CMAKE_CXX_COMPILER" | b"CMAKE_OBJCXX_COMPILER") {
                b"c++"
            } else {
                b"cc"
            }
        }))];
        if let Some(arg1) = lookup(b"_COMPILER_ARG1") {
            words.push(raw_flags(self.model, self.dir, arg1));
        }
        let sysroot = self
            .definition(self.sysroot)
            .or_else(|| self.definition(b"CMAKE_SYSROOT"));
        for (option, value) in [
            (
                b"_COMPILE_OPTIONS_TARGET".as_slice(),
                lookup(b"_COMPILER_TARGET"),
            ),
            (
                b"_COMPILE_OPTIONS_EXTERNAL_TOOLCHAIN",
                lookup(b"_COMPILER_EXTERNAL_TOOLCHAIN"),
            ),
            (b"_COMPILE_OPTIONS_SYSROOT", sysroot),
        ] {
            if let (Some(option), Some(value)) = (
                lookup(option).filter(|v| !v.is_empty()),
                value.filter(|v| !v.is_empty()),
            ) {
                let value = quote(value);
                words.push([option, value.as_slice()].concat().into());
            }
        }
        join_words(words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_cuda_whole_and_target_compile_modes() -> Result<(), rcmake_model::ModelError> {
        use rcmake_model::{Target, TargetKind, TargetOrigin};

        let mut model = ProjectModel::default();
        let id = model.add_target(Target::new(
            "kernels".into(),
            TargetKind::ObjectLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        ))?;
        let compile = |model: &ProjectModel, name: Option<&str>| {
            rule_invocations(model, DirId::ROOT, b"nvcc <CUDA_COMPILE_MODE>", |key| {
                (key == b"TARGET_NAME")
                    .then_some(name)
                    .flatten()
                    .map(|name| quote(name.as_bytes()))
            })
            .remove(0)
            .argv
        };
        assert_eq!(compile(&model, None), ["nvcc", "-c"]);
        let modes = ["PTX", "CUBIN", "FATBIN", "OPTIX"];
        for (mode, flag) in modes
            .into_iter()
            .zip(["-ptx", "-cubin", "-fatbin", "-optix-ir"])
        {
            let target = model
                .target_mut(id)
                .ok_or(rcmake_model::ModelError::InvalidValue)?;
            target
                .props
                .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
            for candidate in modes {
                let enabled = candidate == mode || candidate == "OPTIX";
                target.props.set(
                    format!("CUDA_{candidate}_COMPILATION").into(),
                    if enabled { "ON".into() } else { "OFF".into() },
                );
            }
            assert_eq!(
                compile(&model, Some("kernels")),
                ["nvcc", "-rdc=true", flag]
            );
        }
        Ok(())
    }

    #[test]
    fn compile_and_link_select_their_sysroots_and_preserve_tool_arguments()
    -> Result<(), &'static str> {
        let mut model = ProjectModel::default();
        for (key, value) in [
            ("CMAKE_HOST_SYSTEM_NAME", "Linux"),
            ("CMAKE_C_COMPILER", "/cc tools/cc"),
            ("CMAKE_C_COMPILER_ARG1", "-mthumb -DARG='two words'"),
            ("CMAKE_C_COMPILER_TARGET", "arm-none-eabi"),
            ("CMAKE_C_COMPILE_OPTIONS_TARGET", "--target="),
            ("CMAKE_C_COMPILER_EXTERNAL_TOOLCHAIN", "/gcc tools"),
            (
                "CMAKE_C_COMPILE_OPTIONS_EXTERNAL_TOOLCHAIN",
                "--gcc-toolchain=",
            ),
            ("CMAKE_SYSROOT", "/sysroot"),
            ("CMAKE_SYSROOT_COMPILE", "/compile root"),
            ("CMAKE_SYSROOT_LINK", "/link root"),
            ("CMAKE_C_COMPILE_OPTIONS_SYSROOT", "--sysroot="),
            ("CMAKE_LINKER", "/link tools/link"),
            ("CMAKE_MT", "/link tools/mt"),
        ] {
            model
                .dir_mut(DirId::ROOT)
                .props
                .set(key.into(), value.into());
        }
        for (source, expected) in [
            (true, "--sysroot=/compile root"),
            (false, "--sysroot=/link root"),
        ] {
            let rules = rule_invocations(
                &model,
                DirId::ROOT,
                b"<CMAKE_C_COMPILER> <CMAKE_LINKER> <CMAKE_MT>",
                |key| (source && key == b"SOURCE").then(BString::default),
            );
            let [rule] = rules.as_slice() else {
                return Err("Expected one compiler invocation.");
            };
            assert_eq!(
                rule.argv,
                [
                    "/cc tools/cc",
                    "-mthumb",
                    "-DARG=two words",
                    "--target=arm-none-eabi",
                    "--gcc-toolchain=/gcc tools",
                    expected,
                    "/link tools/link",
                    "/link tools/mt",
                ]
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;

    #[test]
    fn verbatim_custom_command_quotes_only_non_operators() {
        for (args, expected) in [
            (vec!["tool", "&"], "tool \"&\""),
            (vec!["tool", "&>", "log"], "tool &> log"),
            (vec!["tool", "1>>", "2>>"], "tool \"1>>\" \"2>>\""),
        ] {
            let invocation =
                ninja_custom(args.into_iter().map(BString::from).collect(), None, true);
            assert_eq!(invocation.shell, expected);
        }
    }
}
