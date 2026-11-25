// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::path;

use super::toolchain::Family;

pub(super) struct Flags {
    pub(super) source: Option<BString>,
    pub(super) inputs: Vec<PathBuf>,
    pub(super) depfile: Option<PathBuf>,
    pub(super) search: Vec<BString>,
    pub(super) directories: Vec<PathBuf>,
    pub(super) frameworks: bool,
    /// The command asks the driver or linker to print its commands.
    pub(super) verbose: bool,
}

pub(super) fn classify(
    args: &[BString],
    cwd: &Path,
    family: Family,
    external_source: Option<&Path>,
) -> Option<Flags> {
    let mut flags = Flags {
        source: None,
        inputs: Vec::new(),
        depfile: None,
        search: Vec::new(),
        directories: Vec::new(),
        frameworks: false,
        verbose: false,
    };
    if family == Family::Msvc {
        return msvc(args, cwd, flags, external_source);
    }
    let mut args = args.iter();
    let mut compile = false;
    let mut output = false;
    let mut system_dependencies = true;
    while let Some(arg) = args.next() {
        match arg.as_slice() {
            b"-c" => compile = true,
            b"-o" | b"-MF" | b"-MT" | b"-MQ" => {
                let value = path::to_native(args.next()?);
                if !private_path(cwd, &value) {
                    return None;
                }
                output |= arg == "-o";
                if arg == "-MF" {
                    flags.depfile = Some(cwd.join(value));
                }
            }
            b"-I" | b"-iquote" | b"-isystem" | b"-idirafter" | b"-F" | b"-iframework" | b"-L" => {
                let value = args.next()?;
                search_path(&mut flags, arg, value, cwd)?;
            }
            b"-isysroot" | b"--sysroot" | b"-arch" => {
                let value = args.next()?;
                if arg == "-arch" && !matches!(value.as_slice(), b"arm64" | b"x86_64") {
                    return None;
                }
                flags.search.extend([arg.clone(), value.clone()]);
            }
            b"-framework" => {
                if !library_name(args.next()?) {
                    return None;
                }
                flags.frameworks = true;
            }
            b"-v" | b"-Wl,-v" => flags.verbose = true,
            b"-MD" => system_dependencies = true,
            b"-MMD" => system_dependencies = false,
            b"-nostdinc" | b"-nostdinc++" => flags.search.push(arg.clone()),
            b"-Wl,-search_paths_first"
            | b"-Wl,-headerpad_max_install_names"
            | b"-Wl,--as-needed" => {}
            value
                if value.starts_with(b"-Wl,-z,")
                    && library_name(value.strip_prefix(b"-Wl,-z,")?) => {}
            value if value.starts_with(b"-l") && library_name(value.strip_prefix(b"-l")?) => {}
            value if pure_flag(value) => {
                if value.starts_with(b"-std=")
                    || value.starts_with(b"-stdlib=")
                    || value.starts_with(b"-m")
                {
                    flags.search.push(arg.clone());
                }
            }
            value if attached_search(value).is_some() => {
                let (option, value) = attached_search(value)?;
                search_path(&mut flags, option, value, cwd)?;
            }
            value if !value.starts_with(b"-") => {
                let file = path::to_native(value);
                if source_language(value).is_some()
                    && (private_path(cwd, &file) || external_source == Some(file.as_path()))
                {
                    if flags.source.replace(arg.clone()).is_some() {
                        return None;
                    }
                } else if (value.ends_with(b".o") && private_path(cwd, &file))
                    || (file.is_absolute() && library_file(value))
                {
                    flags.inputs.push(cwd.join(file));
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (system_dependencies
        && output
        && compile == flags.source.is_some()
        && (compile || !flags.inputs.is_empty()))
    .then_some(flags)
}

fn msvc(
    args: &[BString],
    cwd: &Path,
    mut flags: Flags,
    external_source: Option<&Path>,
) -> Option<Flags> {
    let mut args = args.iter();
    let mut compile = false;
    let mut output = false;
    while let Some(arg) = args.next() {
        let bytes = arg.as_slice();
        match bytes {
            b"/c" | b"-c" => compile = true,
            b"/I" | b"-I" | b"/external:I" => {
                let option = args.next()?;
                flags.directories.push(absolute_search(cwd, option)?);
            }
            b"/sourceDependencies" => {
                let file = path::to_native(args.next()?);
                if !private_path(cwd, &file) {
                    return None;
                }
                flags.depfile = Some(cwd.join(file));
            }
            b"/nologo" | b"/TP" | b"/TC" => {}
            value if value.starts_with(b"/Fo") || value.starts_with(b"-Fo") => {
                let file = path::to_native(value.get(3..)?);
                if !private_path(cwd, &file) {
                    return None;
                }
                output = true;
            }
            value if value.starts_with(b"/I") || value.starts_with(b"-I") => {
                flags
                    .directories
                    .push(absolute_search(cwd, value.get(2..)?)?);
            }
            value if value.starts_with(b"/external:I") => {
                flags
                    .directories
                    .push(absolute_search(cwd, value.strip_prefix(b"/external:I")?)?);
            }
            value if value.starts_with(b"/external:env:") => flags.search.push(value.into()),
            value
                if [b"/O".as_slice(), b"/W", b"/D", b"/EH", b"/std:", b"/Zc:"]
                    .iter()
                    .any(|prefix| value.starts_with(prefix)) => {}
            b"/MD" | b"/MDd" | b"/MT" | b"/MTd" => {}
            value
                if source_language(value).is_some()
                    && (private_path(cwd, &path::to_native(value))
                        || external_source == Some(path::to_native(value).as_path())) =>
            {
                if flags.source.replace(arg.clone()).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (compile && output && flags.source.is_some()).then_some(flags)
}

pub(super) fn pure_flag(value: &[u8]) -> bool {
    if value.contains(&0) || value.contains(&b'\n') || value.contains(&b'\r') {
        return false;
    }
    if let Some(define) = value
        .strip_prefix(b"-D")
        .or_else(|| value.strip_prefix(b"-U"))
    {
        let name = define
            .split(|&byte| byte == b'=')
            .next()
            .unwrap_or_default();
        return name.split_first().is_some_and(|(&first, rest)| {
            (first.is_ascii_alphabetic() || first == b'_')
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        }) && ![
            b"_Pragma".as_slice(),
            b".incbin",
            b"__has_embed",
            b"\"/",
            b"../",
            b"__DATE__",
            b"__TIME__",
            b"__TIMESTAMP__",
        ]
        .iter()
        .any(|needle| value.windows(needle.len()).any(|part| part == *needle));
    }
    if value.starts_with(b"-W") {
        return ![b"-Wl,".as_slice(), b"-Wa,", b"-Wp,"]
            .iter()
            .any(|prefix| value.starts_with(prefix));
    }
    if let Some(minimum) = value.strip_prefix(b"-mmacosx-version-min=") {
        return !minimum.is_empty() && minimum.iter().all(|b| b.is_ascii_digit() || *b == b'.');
    }
    matches!(
        value,
        b"-g"
            | b"-g0"
            | b"-O0"
            | b"-O1"
            | b"-O2"
            | b"-O3"
            | b"-Os"
            | b"-Oz"
            | b"-Og"
            | b"-O"
            | b"-pthread"
            | b"-fPIC"
            | b"-fPIE"
            | b"-fcolor-diagnostics"
            | b"-fno-color-diagnostics"
            | b"-fdiagnostics-color"
            | b"-fdiagnostics-color=always"
            | b"-fdiagnostics-color=never"
            | b"-fexceptions"
            | b"-fno-exceptions"
            | b"-frtti"
            | b"-fno-rtti"
            | b"-fvisibility=hidden"
            | b"-fvisibility-inlines-hidden"
            | b"-fno-common"
            | b"-fcommon"
            | b"-pedantic"
            | b"-pedantic-errors"
            | b"-pipe"
            | b"-fno-omit-frame-pointer"
            | b"-m64"
            | b"-m32"
            | b"-stdlib=libc++"
            | b"-stdlib=libstdc++"
    ) || value.strip_prefix(b"-std=").is_some_and(|name| {
        !name.is_empty()
            && name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+-".contains(byte))
    })
}

pub(super) fn attached_search(value: &[u8]) -> Option<(&'static [u8], &[u8])> {
    [
        b"-isystem".as_slice(),
        b"-iquote",
        b"-idirafter",
        b"-iframework",
        b"-I",
        b"-F",
        b"-L",
    ]
    .into_iter()
    .find_map(|prefix| {
        value
            .strip_prefix(prefix)
            .filter(|rest| !rest.is_empty())
            .map(|rest| (prefix, rest))
    })
}

fn search_path(flags: &mut Flags, arg: &[u8], value: &[u8], cwd: &Path) -> Option<()> {
    let directory = absolute_search(cwd, value)?;
    flags.directories.push(directory);
    if arg != b"-L" {
        flags.search.extend([arg.into(), value.into()]);
    }
    Some(())
}

pub(super) fn absolute_search(cwd: &Path, value: &[u8]) -> Option<PathBuf> {
    let directory = path::to_native(value);
    if value.is_empty() || value.contains(&0) || value.starts_with(b"=") {
        return None;
    }
    if !directory.is_absolute() && !private_path(cwd, &directory) && directory != Path::new(".") {
        return None;
    }
    Some(cwd.join(directory))
}

pub(super) fn source_language(name: &[u8]) -> Option<bool> {
    if name.ends_with(b".c") {
        Some(false)
    } else if [b".C".as_slice(), b".cc", b".cpp", b".cxx", b".c++"]
        .iter()
        .any(|ext| name.ends_with(ext))
    {
        Some(true)
    } else {
        None
    }
}

fn library_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-._".contains(byte))
}

pub(super) fn library_file(name: &[u8]) -> bool {
    [b".a".as_slice(), b".so", b".dylib", b".tbd", b".lib"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

pub(super) fn private_path(cwd: &Path, file: &Path) -> bool {
    cwd.join(file).strip_prefix(cwd).is_ok_and(|suffix| {
        !suffix.as_os_str().is_empty()
            && suffix
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_flags_must_keep_system_headers() {
        let cwd = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let command = |flags: &[&str]| {
            ["-c", "probe.c", "-o", "probe.o"]
                .into_iter()
                .chain(flags.iter().copied())
                .map(BString::from)
                .collect::<Vec<_>>()
        };
        for (flags, admitted) in [
            (&["-MMD"][..], false),
            (&["-MMD", "-MD"][..], true),
            (&["-MD", "-MMD"][..], false),
            (&["-MG"][..], false),
        ] {
            assert_eq!(
                classify(&command(flags), cwd, Family::Clang, None).is_some(),
                admitted
            );
        }
    }
}
