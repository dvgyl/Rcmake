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
