use rcmake_base::BString;

use crate::{quote, quoted_content};

pub(super) fn render(out: &mut BString, target: &[u8], config: &[u8], path: &[u8], install: bool) {
    out.extend_from_slice(b"set(_cmake_xcframework_location \"");
    if install {
        super::render_relative_path(out, path, false);
    } else {
        quoted_content(out, path);
    }
    out.extend_from_slice(
        br#"")
if(NOT CMAKE_VERSION VERSION_LESS "3.28" AND IS_DIRECTORY "${_cmake_xcframework_location}")
  set_property(TARGET "#,
    );
    quote(out, target);
    out.extend_from_slice(b" PROPERTY ");
    quote(out, &[b"IMPORTED_LOCATION_".as_slice(), config].concat());
    out.extend_from_slice(b" \"${_cmake_xcframework_location}\")\n");
}
