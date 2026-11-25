use indexmap::IndexMap;
use rcmake_base::BString;

use super::install::{cmake_quote, cmake_quoted_content};

pub(super) enum RuntimeEdit {
    Elf(ElfRpathEdit),
    MachO(MachORpathEdit),
}

pub(super) struct ElfRpathEdit {
    pub(super) old: BString,
    pub(super) new: BString,
    pub(super) strip: bool,
    pub(super) escape_new: bool,
}

pub(super) struct MachORpathEdit {
    pub(super) tool: BString,
    pub(super) install_name: Option<BString>,
    pub(super) changes: IndexMap<BString, BString>,
    pub(super) old: Vec<BString>,
    pub(super) new: Vec<BString>,
}

impl RuntimeEdit {
    pub(super) fn check(&self, dir: &[u8], name: &[u8], configured: bool) -> Option<BString> {
        let Self::Elf(edit) = self else { return None };
        let path = installed_file(dir, name);
        Some(elf_guard(&path, configured, |command, indent| {
            command.extend_from_slice(b"file(RPATH_CHECK");
            argument(command, indent, b"FILE ", &path);
            argument(
                command,
                indent,
                b"RPATH ",
                &quote_rpath(&edit.new, edit.escape_new),
            );
            command.push(b')');
        }))
    }

    pub(super) fn render(&self, dir: &[u8], name: &[u8], configured: bool) -> BString {
        let path = installed_file(dir, name);
        let mut command = BString::default();
        match self {
            Self::Elf(edit) => {
                return elf_guard(&path, configured, |command, indent| {
                    command.extend_from_slice(b"file(RPATH_CHANGE");
                    argument(command, indent, b"FILE ", &path);
                    argument(
                        command,
                        indent,
                        b"OLD_RPATH ",
                        &quote_rpath(&edit.old, true),
                    );
                    argument(
                        command,
                        indent,
                        b"NEW_RPATH ",
                        &quote_rpath(&edit.new, edit.escape_new),
                    );
                    if edit.strip {
                        argument(command, indent, b"INSTALL_REMOVE_ENVIRONMENT_RPATH", b"");
                    }
                    command.push(b')');
                });
            }
            Self::MachO(edit) => {
                if edit.install_name.is_some() || !edit.changes.is_empty() {
                    command.extend_from_slice(b"execute_process(COMMAND ");
                    cmake_quote(&mut command, &edit.tool);
                    if let Some(name) = &edit.install_name {
                        command.extend_from_slice(b" -id ");
                        command.extend_from_slice(&escaped_name(name));
                    }
                    for (old, new) in &edit.changes {
                        command.extend_from_slice(b" -change ");
                        cmake_quote(&mut command, old);
                        command.push(b' ');
                        command.extend_from_slice(&escaped_name(new));
                    }
                    command.push(b' ');
                    command.extend_from_slice(&path);
                    command.extend_from_slice(b" COMMAND_ERROR_IS_FATAL ANY)");
                }
                let remove = edit.old.iter().filter(|path| !edit.new.contains(path));
                let add = edit.new.iter().filter(|path| !edit.old.contains(path));
                let mut arguments = BString::default();
                for (flag, path) in remove
                    .map(|path| (b"\n  -delete_rpath ".as_slice(), path))
                    .chain(add.map(|path| (b"\n  -add_rpath ".as_slice(), path)))
                {
                    arguments.extend_from_slice(flag);
                    cmake_quote(&mut arguments, path);
                }
                if !arguments.is_empty() {
                    if !command.is_empty() {
                        command.push(b'\n');
                    }
                    command.extend_from_slice(b"execute_process(COMMAND ");
                    cmake_quote(&mut command, &edit.tool);
                    command.extend_from_slice(&arguments);
                    command.extend_from_slice(b"\n  ");
                    command.extend_from_slice(&path);
                    command.push(b')');
                }
            }
        }
        guard(&path, &command, configured)
    }
}

fn escaped_name(name: &[u8]) -> BString {
    let mut out = BString::from("\"");
    let mut remaining = name;
    let variable = b"${CMAKE_INSTALL_PREFIX}";
    while let Some(index) = remaining
        .windows(variable.len())
        .position(|part| part == variable)
    {
        let (left, right) = remaining.split_at(index);
        cmake_quoted_content(&mut out, left);
        out.extend_from_slice(variable);
        remaining = right.strip_prefix(variable).unwrap_or_default();
    }
    cmake_quoted_content(&mut out, remaining);
    out.push(b'"');
    out
}

fn quote_rpath(value: &[u8], escape: bool) -> BString {
    if !escape || !value.iter().any(|byte| b"\"$\\".contains(byte)) {
        return [b"\"".as_slice(), value, b"\""].concat().into();
    }
    let mut suffix = BString::from("]");
    while value
        .windows(suffix.len())
        .enumerate()
        .any(|(index, part)| {
            part == suffix.as_slice()
                && value
                    .get(index + suffix.len())
                    .is_none_or(|&byte| byte == b']')
        })
    {
        suffix.push(b'=');
    }
    let equals = suffix.strip_prefix(b"]").unwrap_or_default();
    let mut out = BString::from("[");
    out.extend_from_slice(equals);
    out.push(b'[');
    if value.starts_with(b"\n") || value.starts_with(b"\r\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(value);
    out.extend_from_slice(&suffix);
    out.push(b']');
    out
}

fn argument(out: &mut BString, indent: &[u8], keyword: &[u8], value: &[u8]) {
    for part in [b"\n".as_slice(), indent, b"       ", keyword, value] {
        out.extend_from_slice(part);
    }
}

fn elf_guard(path: &[u8], configured: bool, body: impl FnOnce(&mut BString, &[u8])) -> BString {
    let indent = if configured {
        b"    ".as_slice()
    } else {
        b"  "
    };
    let mut out: BString = [
        b"if(EXISTS ".as_slice(),
        path,
        b" AND\n",
        indent,
        b"   NOT IS_SYMLINK ",
        path,
        b")\n",
        indent,
        b"  ",
    ]
    .concat()
    .into();
    body(&mut out, indent);
    for part in [b"\n".as_slice(), indent, b"endif()"] {
        out.extend_from_slice(part);
    }
    out
}

fn installed_file(dir: &[u8], name: &[u8]) -> BString {
    let mut path = BString::from("\"$ENV{DESTDIR}");
    if !super::artifacts::model_path(dir).absolute() && !dir.starts_with(b"/") {
        path.extend_from_slice(b"${CMAKE_INSTALL_PREFIX}/");
    }
    cmake_quoted_content(&mut path, dir);
    if !dir.is_empty() && !dir.ends_with(b"/") {
        path.push(b'/');
    }
    cmake_quoted_content(&mut path, name);
    path.push(b'"');
    path
}

fn guard(path: &[u8], command: &[u8], configured: bool) -> BString {
    let indent = if configured {
        b"    ".as_slice()
    } else {
        b"  "
    };
    let mut out = [
        b"if(EXISTS ".as_slice(),
        path,
        b" AND NOT IS_SYMLINK ",
        path,
        b")\n",
    ]
    .concat();
    for line in command.split(|&byte| byte == b'\n') {
        out.extend_from_slice(&[indent, b"  ", line, b"\n"].concat());
    }
    out.extend_from_slice(indent);
    out.extend_from_slice(b"endif()");
    out.into()
}

/// Translate a runtime directory taken from a linker input.
pub(super) fn translated_link_directory(
    model: &rcmake_model::ProjectModel,
    target: &rcmake_model::Target,
    directory: &[u8],
) -> Option<BString> {
    let get = |key| super::definition(model, target.dir, key);
    let sysroot = get(b"CMAKE_SYSROOT_LINK".as_slice()).or_else(|| get(b"CMAKE_SYSROOT"));
    if let Some(suffix) = sysroot
        .filter(|root| !root.is_empty() && *root != b"/")
        .and_then(|root| prefix_suffix(directory, root))
    {
        return Some(super::join(b"/", suffix));
    }
    let staging = get(b"CMAKE_STAGING_PREFIX").filter(|prefix| !prefix.is_empty())?;
    let suffix = prefix_suffix(directory, staging)?;
    let prefix =
        get(b"CMAKE_INSTALL_PREFIX").map_or(b"/usr/local".as_slice(), |value| value.as_slice());
    Some(super::join(prefix, suffix))
}

fn prefix_suffix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let suffix = path.strip_prefix(super::without_trailing_slashes(prefix))?;
    if suffix.is_empty() {
        Some(suffix)
    } else {
        suffix.strip_prefix(b"/")
    }
}

pub(super) fn external_link_directories<'a>(
    model: &'a rcmake_model::ProjectModel,
    target: &'a rcmake_model::Target,
    libraries: &'a [BString],
) -> impl Iterator<Item = BString> + 'a {
    use rcmake_base::path_lex::Lexical;
    use rcmake_model::DirId;

    let root = model.dir(DirId::ROOT);
    libraries.iter().filter_map(move |library| {
        let path = Lexical::new(library);
        let directory = if super::is_darwin(model, target.dir) {
            framework_runtime_directory(model, target, library)
        } else {
            None
        }
        .or_else(|| super::link::driver::shared_library(path.filename()).then(|| path.parent()))?;
        if !path.absolute()
            || directory.is_empty()
            || [
                Lexical::new(&root.source_dir),
                Lexical::new(&root.binary_dir),
            ]
            .iter()
            .any(|root| root.prefix(Lexical::new(directory)))
        {
            return None;
        }
        Some(
            translated_link_directory(model, target, directory).unwrap_or_else(|| directory.into()),
        )
    })
}

pub(super) fn framework_runtime_directory<'a>(
    model: &rcmake_model::ProjectModel,
    target: &rcmake_model::Target,
    path: &'a [u8],
) -> Option<&'a [u8]> {
    use rcmake_base::{list, path_lex::Lexical};

    let suffix = b".framework/";
    let framework = if path.ends_with(b".framework") {
        path
    } else {
        let index = path.windows(suffix.len()).position(|part| part == suffix)?;
        path.get(..index + suffix.len() - 1)?
    };
    let parent = Lexical::new(framework).parent();
    if parent.ends_with(b"/System/Library/Frameworks")
        || super::definition(
            model,
            target.dir,
            b"CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
        )
        .is_some_and(|paths| list::iter(paths, list::Empty::Drop).any(|path| path == parent))
    {
        None
    } else {
        Some(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_edit_for_staged_install_path() {
        let edit = RuntimeEdit::MachO(MachORpathEdit {
            install_name: None,
            changes: IndexMap::new(),
            tool: "/tools/install name tool".into(),
            old: vec!["/tmp/lib".into()],
            new: vec!["@loader_path/../lib".into()],
        });
        let output = edit.render(b"relative lib", b"libsample.1.dylib", false);
        assert_eq!(output, br#"if(EXISTS "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib" AND NOT IS_SYMLINK "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib")
    execute_process(COMMAND "/tools/install name tool"
      -delete_rpath "/tmp/lib"
      -add_rpath "@loader_path/../lib"
      "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib")
  endif()"#.as_slice());
    }
}
