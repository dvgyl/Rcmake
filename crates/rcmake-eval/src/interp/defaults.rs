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
        let put = |this: &mut Self, k: &[u8], v: BString| this.store_var(k, v);
        if !put(self, b"CMAKE_HOST_SYSTEM_NAME", name.clone()) {
            return;
        }
        let is_windows = name.as_slice() == b"Windows";
        let platform_ok = if is_windows {
            put(self, b"WIN32", "1".into())
                && put(self, b"CMAKE_HOST_WIN32", "1".into())
                && put(self, b"CMAKE_HOST_EXECUTABLE_SUFFIX", ".exe".into())
        } else {
            put(self, b"UNIX", "1".into())
                && put(self, b"CMAKE_HOST_UNIX", "1".into())
                && put(self, b"CMAKE_HOST_EXECUTABLE_SUFFIX", BString::default())
        };
        if !platform_ok {
            return;
        }
        let os_ok = match name.as_slice() {
            b"Darwin" => {
                put(self, b"APPLE", "1".into()) && put(self, b"CMAKE_HOST_APPLE", "1".into())
            }
            b"Linux" => {
                put(self, b"LINUX", "1".into()) && put(self, b"CMAKE_HOST_LINUX", "1".into())
            }
            b"SunOS" => put(self, b"CMAKE_HOST_SOLARIS", "1".into()),
            b"AIX" => put(self, b"AIX", "1".into()) && put(self, b"CMAKE_HOST_AIX", "1".into()),
            b"OpenBSD" | b"FreeBSD" | b"NetBSD" => {
                put(self, b"BSD", name.clone()) && put(self, b"CMAKE_HOST_BSD", name.clone())
            }
            b"DragonFly" => {
                put(self, b"BSD", "DragonFlyBSD".into())
                    && put(self, b"CMAKE_HOST_BSD", "DragonFlyBSD".into())
            }
            _ => true,
        };
        if !os_ok {
            return;
        }
        let to_b = rcmake_host::path::from_native_cmake;
        let command = to_b(&self.config.cmake_command);
        let bin_dir = filename_path(&command);
        let exe_suffix: &[u8] = if is_windows { b".exe" } else { b"" };
        let cmake_root = to_b(&self.config.cmake_root);
        let cwd = to_b(&self.config.cwd);
        let fixed: [(&[u8], BString); 14] = [
            (
                b"CMAKE_MAJOR_VERSION",
                CMAKE_VERSION_MAJOR.to_string().into(),
            ),
            (
                b"CMAKE_MINOR_VERSION",
                CMAKE_VERSION_MINOR.to_string().into(),
            ),
            (
                b"CMAKE_PATCH_VERSION",
                CMAKE_VERSION_PATCH.to_string().into(),
            ),
            (b"CMAKE_TWEAK_VERSION", "0".into()),
            (b"CMAKE_VERSION", CMAKE_VERSION_STR.into()),
            (b"CMAKE_FILES_DIRECTORY", "/CMakeFiles".into()),
            (b"CMAKE_COMMAND", command),
            (
                b"CMAKE_CTEST_COMMAND",
                cat(&[&bin_dir, b"/ctest", exe_suffix]),
            ),
            (
                b"CMAKE_CPACK_COMMAND",
                cat(&[&bin_dir, b"/cpack", exe_suffix]),
            ),
            (b"CMAKE_ROOT", cmake_root),
            (b"CMAKE_SOURCE_DIR", cwd.clone()),
            (b"CMAKE_BINARY_DIR", cwd.clone()),
            (b"CMAKE_CURRENT_SOURCE_DIR", cwd.clone()),
            (b"CMAKE_CURRENT_BINARY_DIR", cwd),
        ];
        for (k, v) in fixed {
            if !put(self, k, v) {
                break;
            }
        }
    }
}
