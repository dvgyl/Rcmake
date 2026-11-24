use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, ByteSlice};
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::{
    ExpandEnv, ExpandedArg,
    configure::{ConfigureOptions, configure},
};
use rcmake_host::{FileKind, Follow, ReadRange, WriteMode, path};
use rcmake_model::{CacheEntry, CacheType};

use super::{quoted, values};

#[derive(Default)]
struct FileOptions {
    copy: bool,
    expand: ConfigureOptions,
    newline: Option<bool>,
    permissions: Option<Permissions>,
    unknown: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Permissions {
    Source,
    Default,
    Explicit(u32),
}

impl Permissions {
    const fn name(self) -> &'static str {
        match self {
            Self::Source => "USE_SOURCE_PERMISSIONS",
            Self::Default => "NO_SOURCE_PERMISSIONS",
            Self::Explicit(_) => "FILE_PERMISSIONS",
        }
    }

    const fn mode(self, source: u32) -> u32 {
        match self {
            Self::Source => source,
            Self::Default => 0o644,
            Self::Explicit(mode) => mode,
        }
    }
}

impl FileOptions {
    fn parse(mut args: &[&[u8]]) -> Result<Self, CmdError> {
        let mut options = Self::default();
        while let Some((arg, tail)) = args.split_first() {
            args = tail;
            match *arg {
                b"COPYONLY" => options.copy = true,
                b"ESCAPE_QUOTES" => options.expand.escape_quotes = true,
                b"@ONLY" => options.expand.at_only = true,
                b"USE_SOURCE_PERMISSIONS" => options.set_permissions(Permissions::Source)?,
                b"NO_SOURCE_PERMISSIONS" => options.set_permissions(Permissions::Default)?,
                b"FILE_PERMISSIONS" => {
                    let mut mode = 0;
                    let mut found = false;
                    let mut invalid = Vec::new();
                    while let Some((name, rest)) = args.split_first() {
                        if matches!(
                            *name,
                            b"COPYONLY"
                                | b"ESCAPE_QUOTES"
                                | b"@ONLY"
                                | b"USE_SOURCE_PERMISSIONS"
                                | b"NO_SOURCE_PERMISSIONS"
                                | b"FILE_PERMISSIONS"
                                | b"NEWLINE_STYLE"
                        ) {
                            break;
                        }
                        if let Some(bit) = permission(name) {
                            mode |= bit;
                        } else {
                            invalid.push(*name);
                        }
                        found = true;
                        args = rest;
                    }
                    if !invalid.is_empty() {
                        return Err(CmdError::status(quoted(
                            "given invalid permission \"",
                            &invalid.join(b"\",\"".as_slice()),
                            "\".",
                        )));
                    }
                    if !found {
                        return Err(CmdError::status(
                            "given FILE_PERMISSIONS without any options. ",
                        ));
                    }
                    options.set_permissions(Permissions::Explicit(mode))?;
                }
                b"NEWLINE_STYLE" => {
                    let Some((style, rest)) = args.split_first() else {
                        return Err(CmdError::status(
                            "NEWLINE_STYLE must set a style: LF, CRLF, UNIX, DOS, or WIN32",
                        ));
                    };
                    options.newline = Some(match *style {
                        b"UNIX" | b"LF" => false,
                        b"DOS" | b"WIN32" | b"CRLF" => true,
                        _ => {
                            return Err(CmdError::status(
                                "NEWLINE_STYLE sets an unknown style, only LF, CRLF, UNIX, DOS, and WIN32 are supported",
                            ));
                        }
                    });
                    args = rest;
                }
                _ => {
                    options.unknown.extend_from_slice(b"\n ");
                    options.unknown.extend_from_slice(arg);
                }
            }
        }
        if options.copy && options.newline.is_some() {
            return Err(CmdError::status(
                "COPYONLY could not be used in combination with NEWLINE_STYLE ",
            ));
        }
        Ok(options)
    }

    fn set_permissions(&mut self, permissions: Permissions) -> Result<(), CmdError> {
        if let Some(previous) = self.permissions
            && previous.name() != permissions.name()
        {
            let (first, second) = match (previous, permissions) {
                (_, Permissions::Explicit(_)) | (Permissions::Default, Permissions::Source) => {
                    (permissions, previous)
                }
                _ => (previous, permissions),
            };
            return Err(CmdError::status(format!(
                "given both {} and {}.  Only one option allowed. ",
                first.name(),
                second.name()
            )));
        }
        self.permissions = Some(permissions);
        Ok(())
    }
}

fn permission(name: &[u8]) -> Option<u32> {
    Some(match name {
        b"OWNER_READ" => 0o400,
        b"OWNER_WRITE" => 0o200,
        b"OWNER_EXECUTE" => 0o100,
        b"GROUP_READ" => 0o040,
        b"GROUP_WRITE" => 0o020,
        b"GROUP_EXECUTE" => 0o010,
        b"WORLD_READ" => 0o004,
        b"WORLD_WRITE" => 0o002,
        b"WORLD_EXECUTE" => 0o001,
        b"SETUID" => 0o4000,
        b"SETGID" => 0o2000,
        _ => return None,
    })
}

pub(crate) fn configure_file(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [input, output, flags @ ..] = args.as_slice() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments, expected 2",
        ));
    };
    let options = FileOptions::parse(flags)?;
    if !options.unknown.is_empty() {
        let body = quoted(
            "configure_file called with unknown argument(s):\n",
            &options.unknown,
            "\n",
        );
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            body,
        );
        if cx.fatal_occurred() {
            return Err(CmdError::Reported);
        }
    }

    let source = normalize(&cx.resolve_path(input));
    let metadata = cx
        .host()
        .metadata(&source, Follow::Yes)
        .map_err(file_error)?;
    if metadata.kind == FileKind::Dir {
        return Err(CmdError::status(quoted(
            "input location\n\n  ",
            &path::from_native(&source),
            "\n\nis a directory but a file was expected.",
        )));
    }
    if metadata.kind != FileKind::File {
        return Err(CmdError::status("input must be a regular file."));
    }
    let mut dest = normalize(&cx.current_binary_dir().join(path::to_native(output)));
    if cx
        .host()
        .metadata(&dest, Follow::Yes)
        .is_ok_and(|m| m.kind == FileKind::Dir)
        && let Some(name) = source.file_name()
    {
        dest.push(name);
    }
    let range = ReadRange::All {
        limit: cx.limits().max_file_read,
    };
    let input = cx.host().read(&source, range).map_err(file_error)?;
    if !options.copy
        && [b"\xff\xfe".as_slice(), b"\xfe\xff", b"\0\0\xfe\xff"]
            .iter()
            .any(|bom| input.starts_with(bom))
    {
        return Err(crate::command_error(
            cx,
            quoted(
                "File starts with a Byte-Order-Mark that is not UTF-8:\n  ",
                &path::from_native(&source),
                "",
            ),
        ));
    }
    let content = if options.copy {
        input
    } else {
        configure(&input, &mut TemplateEnv(cx), options.expand)
            .map_err(|error| CmdError::status(error.message))?
            .into()
    };
    let content = if !options.copy && options.newline.unwrap_or(cfg!(windows)) {
        content.replace(b"\n", b"\r\n")
    } else {
        content
    };
    if !options.copy && content.len() > cx.limits().max_value_bytes {
        return Err(CmdError::status(
            "configured content exceeds the value size limit.",
        ));
    }
    let mode = options
        .permissions
        .unwrap_or(Permissions::Source)
        .mode(metadata.mode)
        & 0o7777;
    let host = cx.host();
    let dest_meta = host.metadata(&dest, Follow::Yes);
    if dest_meta
        .as_ref()
        .is_ok_and(|m| m.kind == FileKind::File && m.len == content.len() as u64)
        && host
            .read(&dest, range)
            .is_ok_and(|existing| existing == content)
    {
        if dest_meta.is_ok_and(|m| m.mode & 0o7777 != mode) {
            host.set_permissions(&dest, mode).map_err(file_error)?;
        }
    } else {
        write_configured(cx, &dest, &content, mode)?;
    }
    Ok(Flow::Normal)
}

fn write_configured(cx: &Cx<'_>, dest: &Path, content: &[u8], mode: u32) -> Result<(), CmdError> {
    let host = cx.host();
    let parent = dest
        .parent()
        .ok_or_else(|| CmdError::status("output has no parent directory."))?;
    host.create_dir_all(parent).map_err(file_error)?;
    let pid = host.system().pid;
    for index in 0..100 {
        let temp = parent.join(format!(".rcmake-configure-{pid}-{index}.tmp"));
        if temp == dest {
            continue;
        }
        match host.write(&temp, b"", WriteMode::CreateNew) {
            Ok(()) => {
                let result = host
                    .write(&temp, content, WriteMode::Truncate)
                    .and_then(|()| host.set_permissions(&temp, mode))
                    .and_then(|()| host.rename(&temp, dest));
                if result.is_err() {
                    let _ = host.remove_file(&temp);
                }
                return result.map_err(file_error);
            }
            Err(error) if error.kind == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(file_error(error)),
        }
    }
    Err(CmdError::status(
        "could not create a temporary output file.",
    ))
}

fn file_error(error: rcmake_host::HostError) -> CmdError {
    CmdError::status(format!("Problem configuring file: {error}"))
}

struct TemplateEnv<'a, 'b>(&'a mut Cx<'b>);

impl ExpandEnv for TemplateEnv<'_, '_> {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.var(name)
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.cache().get(name).map(|entry| entry.value.clone())
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.0.host().env_get(name)
    }

    fn missing_reference(&mut self, name: &[u8]) {
        self.0.warn_uninitialized(name);
    }

    fn current_line(&self) -> u32 {
        self.0.current_line()
    }
}

pub(super) fn filename_component(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let [var, input, component, flags @ ..] = args.as_slice() else {
        return Err(crate::command_error(
            cx,
            "get_filename_component called with incorrect number of arguments",
        ));
    };
    let mut flags = flags;
    let mut cache = false;
    let mut base_dir = None;
    let mut program_args = None;
    while let Some((arg, rest)) = flags.split_first() {
        flags = rest;
        match *arg {
            b"CACHE" => cache = true,
            b"BASE_DIR" | b"PROGRAM_ARGS" => {
                if let Some((value, tail)) = flags.split_first() {
                    if *arg == b"BASE_DIR" {
                        base_dir = Some(path::to_native(value));
                    } else {
                        program_args = Some(*value);
                    }
                    flags = tail;
                }
            }
            _ => {}
        }
    }
    if cache
        && cx
            .var(var)
            .is_some_and(|value| value != b"NOTFOUND" && !value.ends_with(b"-NOTFOUND"))
    {
        return Ok(Flow::Normal);
    }

    let path_input = input.replace(b"\\", b"/");
    let name = path_input
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    let mut program_tail = None;
    let out = match *component {
        b"DIRECTORY" | b"PATH" => directory(&path_input).into(),
        b"NAME" => name.into(),
        b"EXT" | b"NAME_WE" => {
            extension(name, name.iter().position(|byte| *byte == b'.'), component)
        }
        b"LAST_EXT" | b"NAME_WLE" => {
            extension(name, name.iter().rposition(|byte| *byte == b'.'), component)
        }
        b"ABSOLUTE" | b"REALPATH" => {
            let base = base_dir.unwrap_or_else(|| cx.current_source_dir());
            let input = if let Some(drive) = drive_root(&path_input).filter(|_| !cfg!(windows)) {
                let absolute = path::to_native(path_input.get(2..).unwrap_or_default());
                let clean = path::from_native(&normalize(&absolute));
                path::to_native(&[drive, clean.as_slice()].concat())
            } else if let Some(tail) = path_input.strip_prefix(b"~/")
                && let Some(home) = cx.host().env_get(b"HOME")
            {
                path::to_native(&home).join(path::to_native(tail))
            } else {
                base.join(path::to_native(&path_input))
            };
            let normalized = normalize(&input);
            let resolved = if *component == b"REALPATH" {
                cx.host().canonicalize(&normalized).unwrap_or(normalized)
            } else {
                normalized
            };
            path::from_native(&resolved)
        }
        b"PROGRAM" => {
            let (program, tail) = find_program(cx, input);
            program_tail = Some(tail);
            program
        }
        other => {
            return Err(crate::command_error(
                cx,
                quoted("get_filename_component unknown component ", other, ""),
            ));
        }
    };
    if let (Some(var), Some(tail)) = (program_args, program_tail)
        && !tail.is_empty()
    {
        filename_result(cx, var, tail, cache);
    }
    filename_result(cx, var, out, cache);
    Ok(Flow::Normal)
}

fn filename_result(cx: &mut Cx<'_>, name: &[u8], value: BString, cache: bool) {
    if cache {
        cx.cache().insert(
            name.into(),
            CacheEntry {
                value,
                ty: CacheType::String,
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            },
        );
        cx.unset_var(name);
    } else {
        cx.set_var(name, value);
    }
}

fn drive_root(input: &[u8]) -> Option<&[u8]> {
    match input {
        [drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => input.get(..2),
        _ => None,
    }
}

fn directory(input: &[u8]) -> &[u8] {
    let Some(end) = input.iter().rposition(|byte| *byte != b'/') else {
        return input
            .get(..usize::from(!input.is_empty()))
            .unwrap_or_default();
    };
    let prefix = input.get(..=end).unwrap_or_default();
    let Some(slash) = prefix.iter().rposition(|byte| *byte == b'/') else {
        return b"";
    };
    if slash == 2 && drive_root(input).is_some() {
        return input.get(..3).unwrap_or_default();
    }
    let parent = prefix.get(..slash).unwrap_or_default();
    if parent.is_empty() {
        return b"/";
    }
    parent.trim_end_with(|byte| byte == char::from(b'/'))
}

fn extension(name: &[u8], dot: Option<usize>, component: &[u8]) -> BString {
    let end = dot.unwrap_or(name.len());
    if component.starts_with(b"NAME") {
        name.get(..end).unwrap_or_default().into()
    } else {
        name.get(end..).unwrap_or_default().into()
    }
}

pub(super) fn normalize(input: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    for part in input.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match path.components().next_back() {
                Some(Component::Normal(_)) => {
                    path.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => path.push(part),
            },
            _ => path.push(part),
        }
    }
    path
}

fn find_program(cx: &Cx<'_>, input: &[u8]) -> (BString, BString) {
    let input = input.trim_ascii_start();
    let mut whole_path_search = false;
    let (name, tail) = if let Some(&quote @ (b'"' | b'\'')) = input.first() {
        let Some(quoted) = input.get(1..) else {
            return (BString::default(), BString::default());
        };
        let Some(end) = quoted.iter().position(|byte| *byte == quote) else {
            return (BString::default(), BString::default());
        };
        (quoted.get(..end), quoted.get(end + 1..))
    } else {
        let end = input
            .iter()
            .position(u8::is_ascii_whitespace)
            .unwrap_or(input.len());
        whole_path_search = end < input.len();
        (input.get(..end), input.get(end..))
    };
    if let Some(program) = name.and_then(|name| program_path(cx, name, false)) {
        (program, tail.unwrap_or_default().into())
    } else {
        let program = whole_path_search
            .then(|| program_path(cx, input, true))
            .flatten()
            .unwrap_or_default();
        (program, BString::default())
    }
}

fn program_path(cx: &Cx<'_>, name: &[u8], whole_path_search: bool) -> Option<BString> {
    if name.is_empty() {
        return None;
    }
    let name = name.replace(b"\\", b"/");
    let local = cx.resolve_path(&name);
    if cx
        .host()
        .metadata(&local, Follow::Yes)
        .is_ok_and(|metadata| {
            !whole_path_search
                || metadata.kind == FileKind::File && (cfg!(windows) || metadata.mode & 0o111 != 0)
        })
    {
        return Some(if path::to_native(&name).is_absolute() {
            path::from_native(&normalize(&local))
        } else if whole_path_search {
            path::from_native(&cx.host().canonicalize(&local).unwrap_or(local))
        } else {
            name.into()
        });
    }
    if name.contains(&b'/') {
        return None;
    }
    let paths = cx.host().env_get(b"PATH")?;
    let delimiter = if cfg!(windows) { b';' } else { b':' };
    paths.split(|byte| *byte == delimiter).find_map(|dir| {
        let candidate = normalize(&cx.resolve_path(dir).join(path::to_native(&name)));
        cx.host()
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|m| m.kind == FileKind::File && (cfg!(windows) || m.mode & 0o111 != 0))
            .then(|| path::from_native(&candidate))
    })
}
