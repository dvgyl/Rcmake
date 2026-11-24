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

pub(super) fn direct(argv: Vec<BString>, working_dir: Option<BString>) -> Invocation {
    let shell = join_words(argv.iter().map(|arg| quote(arg)));
    Invocation {
        argv,
        shell,
        working_dir,
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
pub(super) fn simple_args(shell: &[u8]) -> Option<Vec<BString>> {
    let mut args = Vec::new();
    let mut word = BString::default();
    let mut active = false;
    let mut delimiter = None;
    let mut bytes = shell.iter().copied();
    while let Some(byte) = bytes.next() {
        match (delimiter, byte) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => delimiter = None,
            (Some(b'\''), _) => word.push(byte),
            (None, b'\'' | b'"') => {
                delimiter = Some(byte);
                active = true;
            }
            (_, b'\\') => {
                let next = bytes.next()?;
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
    }
    Some(args)
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
            b"CMAKE_AR" => Some(quote(value.unwrap_or(b"ar"))),
            b"CMAKE_RANLIB" => Some(quote(value.unwrap_or(b"ranlib"))),
            b"CMAKE_COMMAND" => Some(quote(value.unwrap_or(b"cmake"))),
            b"CMAKE_LINKER" | b"CMAKE_MT" | b"CMAKE_RC_COMPILER" => Some(
                value
                    .filter(|v| !v.is_empty())
                    .map_or_else(BString::default, quote),
            ),
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
            words.push(arg1.into());
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
    fn compile_and_link_select_their_sysroots_and_preserve_tool_arguments() {
        let mut model = ProjectModel::default();
        for (key, value) in [
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
            assert_eq!(
                rules[0].argv,
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
    }
}
