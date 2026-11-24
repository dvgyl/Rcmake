use std::io::ErrorKind;
use std::path::Path;

use rcmake_base::path_lex::Lexical;
use rcmake_eval::{Cx, Flow};
use rcmake_host::{
    Follow,
    path::{from_native, to_native},
};

use super::copy::{self, Options, permissions};
use super::file::binary_destination;
use super::{Result, arg, error, status};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Message {
    #[default]
    Always,
    Lazy,
    Never,
}

struct Install<'a> {
    inputs: Vec<&'a [u8]>,
    destination: &'a [u8],
    source_base: Option<&'a [u8]>,
    rename: Option<&'a [u8]>,
    message: Message,
    optional: bool,
    directory: bool,
    executable: bool,
    copy: Options,
}

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let install = parse(cx, args)?;
    let unstaged = binary_destination(cx, install.destination);
    let destdir = cx
        .host()
        .env_get(b"DESTDIR")
        .filter(|value| !value.is_empty());
    let destination = match &destdir {
        Some(prefix) => {
            let normalized = from_native(&unstaged);
            let unstaged = normalized
                .strip_prefix(b"/")
                .ok_or_else(|| error("INSTALL cannot use DESTDIR with this destination."))?;
            to_native(prefix).join(to_native(unstaged))
        }
        None => unstaged.clone(),
    };
    let mut manifest = cx.var_or_empty(b"CMAKE_INSTALL_MANIFEST_FILES");
    let mut budget = cx.limits().regex_steps;
    let result = install.inputs.iter().try_for_each(|&input| {
        let host = cx.host();
        if input.is_empty() {
            if install.directory {
                let current = host.metadata(&destination, Follow::Yes).is_ok();
                host.create_dir_all(&destination)
                    .map_err(|err| error(err.to_string()))?;
                host.set_permissions(&destination, install.copy.modes.get(true).unwrap_or(0o755))
                    .map_err(|err| error(err.to_string()))?;
                display(cx, &destination, current, install.message);
            }
            return Ok(());
        }
        let relative = to_native(input);
        let source = match install.source_base {
            Some(base) => cx.resolve_path(base).join(&relative),
            None => cx.resolve_path(input),
        };
        if let Err(err) = host.metadata(&source, Follow::No) {
            if install.optional && err.kind == ErrorKind::NotFound {
                return Ok(());
            }
            return Err(error(format!(
                "INSTALL cannot find \"{}\": {}.",
                source.display(),
                err.os_message
            )));
        }
        let target = if let Some(rename) = install.rename {
            destination.join(to_native(rename))
        } else if install.source_base.is_some() {
            destination.join(&relative)
        } else if input.ends_with(b"/") || Lexical::new(input).filename() == b"." {
            destination.clone()
        } else {
            destination.join(source.file_name().unwrap_or_default())
        };
        copy::copy_entry(
            cx,
            &source,
            &target,
            &install.copy,
            &mut budget,
            0,
            &mut |path: &Path, directory, current| {
                display(cx, path, current, install.message);
                if !directory {
                    let relative = path.strip_prefix(&destination).unwrap_or(path);
                    let entry = from_native(&unstaged.join(relative));
                    if !manifest.is_empty() {
                        manifest.push(b';');
                    }
                    manifest.extend_from_slice(&entry);
                }
            },
        )
    });
    super::set(cx, b"CMAKE_INSTALL_MANIFEST_FILES", manifest)?;
    result?;
    Ok(Flow::Normal)
}

fn display(cx: &Cx<'_>, target: &Path, current: bool, message: Message) {
    if message == Message::Never || current && message == Message::Lazy {
        return;
    }
    let label = if current {
        b"Up-to-date: ".as_slice()
    } else {
        b"Installing: ".as_slice()
    };
    status(cx, &[label, &from_native(target)].concat());
}

fn parse<'a>(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Install<'a>> {
    let mut install = Install {
        inputs: Vec::new(),
        destination: b"",
        source_base: None,
        rename: None,
        message: Message::Always,
        optional: false,
        directory: false,
        executable: false,
        copy: Options::for_install(
            cx.host()
                .env_get(b"CMAKE_INSTALL_ALWAYS")
                .is_some_and(|value| !value.is_empty()),
        ),
    };
    let mut args = args.iter().copied().peekable();
    let mut files = true;
    let mut matching = false;
    let mut message = None;
    while let Some(option) = args.next() {
        match option {
            b"PATTERN" | b"REGEX" => {
                install
                    .copy
                    .match_rule(cx, option, arg(&mut args, option)?)?;
                matching = true;
            }
            b"EXCLUDE" | b"PERMISSIONS" if matching => {
                install.copy.match_option(option, &mut args)?
            }
            _ if matching => {
                return Err(error(
                    [
                        b"option ",
                        option,
                        b" may not appear after PATTERN or REGEX.",
                    ]
                    .concat(),
                ));
            }
            b"FILES" | b"PROGRAMS" | b"DIRECTORY" => {
                files = true;
                if option == b"PROGRAMS" {
                    install.executable = true;
                }
                if option == b"DIRECTORY" {
                    install.directory = true;
                }
                continue;
            }
            b"FILES_FROM_DIR" => install.source_base = Some(arg(&mut args, option)?),
            b"DESTINATION" => install.destination = arg(&mut args, option)?,
            b"RENAME" => install.rename = Some(arg(&mut args, option)?),
            b"TYPE" => match arg(&mut args, option)? {
                b"DIRECTORY" => install.directory = true,
                b"PROGRAM" | b"EXECUTABLE" => install.executable = true,
                b"MODULE" | b"MODULE_LIBRARY" | b"SHARED_LIBRARY" => {
                    install.executable = !cx.var_is_on(b"CMAKE_INSTALL_SO_NO_EXE");
                }
                b"FILE" | b"STATIC_LIBRARY" => {}
                kind => return Err(error([b"INSTALL given unknown TYPE ", kind].concat())),
            },
            b"FILE_PERMISSIONS" | b"PERMISSIONS" => {
                install.copy.modes.files = Some(permissions(&mut args)?)
            }
            b"DIRECTORY_PERMISSIONS" => {
                install.copy.modes.directories = Some(permissions(&mut args)?)
            }
            b"USE_SOURCE_PERMISSIONS" => install.copy.no_source_permissions = false,
            b"NO_SOURCE_PERMISSIONS" => install.copy.no_source_permissions = true,
            b"FOLLOW_SYMLINK_CHAIN" => install.copy.follow_chain = true,
            b"FILES_MATCHING" => install.copy.files_matching = true,
            b"OPTIONAL" => install.optional = true,
            b"MESSAGE_ALWAYS" | b"MESSAGE_LAZY" | b"MESSAGE_NEVER" => {
                let mode = match option {
                    b"MESSAGE_LAZY" => Message::Lazy,
                    b"MESSAGE_NEVER" => Message::Never,
                    _ => Message::Always,
                };
                if message
                    .replace(mode)
                    .is_some_and(|previous| previous != mode)
                {
                    return Err(error(
                        "INSTALL options MESSAGE_ALWAYS, MESSAGE_LAZY, and MESSAGE_NEVER are mutually exclusive.",
                    ));
                }
                install.message = mode;
            }
            _ if files => {
                install.inputs.push(option);
                continue;
            }
            _ => {
                return Err(error(
                    [b"called with unknown argument \"", option, b"\"."].concat(),
                ));
            }
        }
        files = false;
    }
    if install.destination.is_empty() {
        return Err(error("INSTALL given no DESTINATION."));
    }
    if install.source_base.is_some() {
        if install
            .inputs
            .iter()
            .any(|input| to_native(input).is_absolute())
        {
            return Err(error(
                "option FILES_FROM_DIR requires all files to be specified as relative paths.",
            ));
        }
        if install.rename.is_some() {
            return Err(error(
                "INSTALL option RENAME may not be combined with FILES_FROM_DIR.",
            ));
        }
    }
    if let Some(rename) = install.rename
        && (install.inputs.len() != 1 || install.directory || to_native(rename).is_absolute())
    {
        return Err(error("INSTALL given invalid RENAME for this file list."));
    }
    if install.executable
        && install.copy.no_source_permissions
        && install.copy.modes.files.is_none()
    {
        install.copy.modes.files = Some(0o755);
    }
    Ok(install)
}
