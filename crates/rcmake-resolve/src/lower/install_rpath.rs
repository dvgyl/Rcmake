use rcmake_base::{BString, list};

use super::install::{cmake_quote, cmake_quoted_content};

pub(super) struct MachORpathEdit {
    pub(super) file_name: BString,
    pub(super) tool: BString,
    pub(super) old: Vec<BString>,
    pub(super) new: Vec<BString>,
}

impl MachORpathEdit {
    pub(super) fn render(&self, dir: &[u8], configured: bool) -> BString {
        let mut path = BString::from("\"$ENV{DESTDIR}");
        if !rcmake_host::path::to_native(dir).is_absolute() {
            path.extend_from_slice(b"${CMAKE_INSTALL_PREFIX}/");
        }
        cmake_quoted_content(&mut path, dir);
        if !dir.ends_with(b"/") {
            path.push(b'/');
        }
        cmake_quoted_content(&mut path, &self.file_name);
        path.push(b'"');

        let indent = if configured {
            b"    ".as_slice()
        } else {
            b"  "
        };
        let mut out = BString::from("if(EXISTS ");
        out.extend_from_slice(&path);
        out.extend_from_slice(b" AND NOT IS_SYMLINK ");
        out.extend_from_slice(&path);
        out.extend_from_slice(b")\n");
        out.extend_from_slice(indent);
        let action = if self.new.is_empty() {
            b"  file(RPATH_REMOVE FILE ".as_slice()
        } else {
            b"  file(RPATH_CHANGE FILE "
        };
        out.extend_from_slice(action);
        out.extend_from_slice(&path);
        out.extend_from_slice(b" OLD_RPATH ");
        cmake_quote(&mut out, &runtime_list(&self.old));
        if !self.new.is_empty() {
            out.extend_from_slice(b" NEW_RPATH ");
            cmake_quote(&mut out, &runtime_list(&self.new));
        }
        out.extend_from_slice(b" INSTALL_NAME_TOOL ");
        cmake_quote(&mut out, &self.tool);
        out.extend_from_slice(b")\n");
        out.extend_from_slice(indent);
        out.extend_from_slice(b"endif()");
        out
    }
}

fn runtime_list(paths: &[BString]) -> BString {
    list::join(paths.iter().map(|path| {
        let mut encoded = Vec::new();
        for &byte in path.as_slice() {
            if byte == b';' {
                encoded.push(b'\\');
            }
            encoded.push(byte);
        }
        encoded
    }))
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
        let edit = MachORpathEdit {
            file_name: "libsample.1.dylib".into(),
            tool: "/tools/install name tool".into(),
            old: vec!["/tmp/lib".into()],
            new: vec!["@loader_path/../lib".into()],
        };
        let output = edit.render(b"relative lib", false);
        assert_eq!(output, br#"if(EXISTS "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib" AND NOT IS_SYMLINK "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib")
    file(RPATH_CHANGE FILE "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/relative lib/libsample.1.dylib" OLD_RPATH "/tmp/lib" NEW_RPATH "@loader_path/../lib" INSTALL_NAME_TOOL "/tools/install name tool")
  endif()"#.as_slice());
    }
}
