use rcmake_base::BString;
use rcmake_resolve::ExportPlan;

use crate::{quote, quoted_content};

pub(super) fn render(
    out: &mut BString,
    plan: &ExportPlan,
    target: &[u8],
    config: &[u8],
    path: &[u8],
) {
    out.extend_from_slice(b"set(_cmake_xcframework_location \"");
    if plan.install {
        super::render_relative_path(out, plan, path, false);
    } else {
        quoted_content(out, path);
    }
    out.extend_from_slice(
        br#"")
if(NOT CMAKE_VERSION VERSION_LESS "3.28" AND IS_DIRECTORY "${_cmake_xcframework_location}")
  set_property(TARGET "#,
    );
    super::target_argument(out, target);
    out.extend_from_slice(b" PROPERTY ");
    quote(out, &[b"IMPORTED_LOCATION_".as_slice(), config].concat());
    out.extend_from_slice(b" \"${_cmake_xcframework_location}\")\n");
}
