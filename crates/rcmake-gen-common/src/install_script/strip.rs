use rcmake_base::{BString, path_lex::Lexical};
use rcmake_resolve::InstallAction;

use super::{arguments, quote, quoted_content};

pub(super) fn render(
    out: &mut BString,
    action: &InstallAction,
    indent: &[u8],
    program: &[u8],
    flags: &[BString],
    name: &[u8],
) {
    let destination = &action.destination;
    let mut path = BString::from("\"$ENV{DESTDIR}");
    if !destination.starts_with(b"/") && !Lexical::windows(destination).absolute() {
        path.extend_from_slice(b"${CMAKE_INSTALL_PREFIX}/");
    }
    quoted_content(&mut path, destination);
    if !destination.is_empty() && !destination.ends_with(b"/") {
        path.push(b'/');
    }
    quoted_content(&mut path, name);
    path.push(b'"');
    for part in [
        b"if(EXISTS ".as_slice(),
        &path,
        b" AND NOT IS_SYMLINK ",
        &path,
        b")\n",
        indent,
        b"  if(CMAKE_INSTALL_DO_STRIP)\n",
        indent,
        b"    execute_process(COMMAND ",
    ] {
        out.extend_from_slice(part);
    }
    quote(out, program);
    arguments(out, flags);
    for part in [
        b" ".as_slice(),
        &path,
        b")\n",
        indent,
        b"  endif()\n",
        indent,
        b"endif()\n",
    ] {
        out.extend_from_slice(part);
    }
}
