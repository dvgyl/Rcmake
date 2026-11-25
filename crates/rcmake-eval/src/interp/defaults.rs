use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
};

use crate::Interp;
use crate::util::{cat, filename_path};

/// Host system name as CMake reports it (`cmSystemTools::GetSystemName`).
fn host_system_name(sysname: &[u8]) -> BString {
    let lower = sysname.to_ascii_lowercase();
    if lower.starts_with(b"windows") {
        return BString::from("Windows");
    }
    for (prefix, name) in [
        (&b"cygwin"[..], "CYGWIN"),
        (b"msys", "MSYS"),
        (b"mingw", "MINGW"),
    ] {
        if lower.starts_with(prefix) {
            return BString::from(name);
        }
    }
    BString::from(sysname)
}

impl Interp {
    /// Define the variables that CMake defines in every new directory/script session
    /// (`cmStateSnapshot::SetDefaultDefinitions` plus script-mode paths).
    pub(crate) fn init_default_vars(&mut self) {
        let sys = self.config.host.system();
        let name = host_system_name(&sys.sysname);
        let mut defs: Vec<(&[u8], BString)> = Vec::new();
        defs.push((b"CMAKE_HOST_SYSTEM_NAME", name.clone()));
        if name.as_slice() == b"Windows" {
            defs.push((b"WIN32", "1".into()));
            defs.push((b"CMAKE_HOST_WIN32", "1".into()));
            defs.push((b"CMAKE_HOST_EXECUTABLE_SUFFIX", ".exe".into()));
        } else {
            defs.push((b"UNIX", "1".into()));
            defs.push((b"CMAKE_HOST_UNIX", "1".into()));
            defs.push((b"CMAKE_HOST_EXECUTABLE_SUFFIX", BString::default()));
        }
        match name.as_slice() {
            b"Darwin" => {
                defs.push((b"APPLE", "1".into()));
                defs.push((b"CMAKE_HOST_APPLE", "1".into()));
            }
            b"Linux" => {
                defs.push((b"LINUX", "1".into()));
                defs.push((b"CMAKE_HOST_LINUX", "1".into()));
            }
            b"SunOS" => defs.push((b"CMAKE_HOST_SOLARIS", "1".into())),
            b"AIX" => {
                defs.push((b"AIX", "1".into()));
                defs.push((b"CMAKE_HOST_AIX", "1".into()));
            }
            b"OpenBSD" | b"FreeBSD" | b"NetBSD" => {
                defs.push((b"BSD", name.clone()));
                defs.push((b"CMAKE_HOST_BSD", name.clone()));
            }
            b"DragonFly" => {
                defs.push((b"BSD", "DragonFlyBSD".into()));
                defs.push((b"CMAKE_HOST_BSD", "DragonFlyBSD".into()));
            }
            _ => {}
        }
        defs.push((
            b"CMAKE_MAJOR_VERSION",
            CMAKE_VERSION_MAJOR.to_string().into(),
        ));
        defs.push((
            b"CMAKE_MINOR_VERSION",
            CMAKE_VERSION_MINOR.to_string().into(),
        ));
        defs.push((
            b"CMAKE_PATCH_VERSION",
            CMAKE_VERSION_PATCH.to_string().into(),
        ));
        defs.push((b"CMAKE_TWEAK_VERSION", "0".into()));
        defs.push((b"CMAKE_VERSION", CMAKE_VERSION_STR.into()));
        defs.push((b"CMAKE_FILES_DIRECTORY", "/CMakeFiles".into()));

        let to_b = rcmake_host::path::from_native_cmake;
        let command = to_b(&self.config.cmake_command);
        let bin_dir = filename_path(&command);
        let exe_suffix: &[u8] = if name.as_slice() == b"Windows" {
            b".exe"
        } else {
            b""
        };
        defs.push((b"CMAKE_COMMAND", command));
        defs.push((
            b"CMAKE_CTEST_COMMAND",
            cat(&[&bin_dir, b"/ctest", exe_suffix]),
        ));
        defs.push((
            b"CMAKE_CPACK_COMMAND",
            cat(&[&bin_dir, b"/cpack", exe_suffix]),
        ));
        defs.push((b"CMAKE_ROOT", to_b(&self.config.cmake_root)));
        let cwd = to_b(&self.config.cwd);
        for var in [
            &b"CMAKE_SOURCE_DIR"[..],
            b"CMAKE_BINARY_DIR",
            b"CMAKE_CURRENT_SOURCE_DIR",
            b"CMAKE_CURRENT_BINARY_DIR",
        ] {
            defs.push((var, cwd.clone()));
        }
        for (k, v) in defs {
            self.vars.set(k, v);
        }
    }
}
