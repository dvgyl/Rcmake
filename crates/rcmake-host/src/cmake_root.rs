use std::path::{Path, PathBuf};

use rcmake_base::BString;

use crate::{FileKind, Follow, Host, path::to_native};

/// Find the CMake executable and its data directory.
///
/// Check the environment override first. Then check beside the canonical and
/// direct executable paths, in that order. If necessary, check the direct and
/// canonical paths from `argv0`. Resolve relative paths against the initial
/// working directory. A valid data directory contains
/// `Modules/CMakeDetermineSystem.cmake`.
///
/// Return the executable path with no data directory if all candidates fail.
#[must_use]
pub fn discover_cmake_root(host: &dyn Host, argv0: Option<&BString>) -> (PathBuf, Option<PathBuf>) {
    let root_override = host
        .env_get(b"RCMAKE_VENDORED_DATA_ROOT")
        .filter(|root| !root.is_empty())
        .map(|root| host.initial_cwd().join(to_native(&root)))
        .filter(|root| valid_root(host, root));
    let exe = host.current_exe().ok();
    let canonical_exe = exe.as_deref().and_then(|path| host.canonicalize(path).ok());
    let argv_path = argv0.map(|arg| host.initial_cwd().join(to_native(arg)));
    let fallback = || {
        canonical_exe
            .as_ref()
            .or(exe.as_ref())
            .or(argv_path.as_ref())
            .cloned()
            .unwrap_or_else(|| host.initial_cwd().join("cmake"))
    };
    if let Some(root) = root_override {
        return (fallback(), Some(root));
    }

    let installed_root = |path: &Path| {
        let root = path.parent()?.parent()?.join("share/cmake-4.5");
        valid_root(host, &root).then(|| (path.to_path_buf(), Some(root)))
    };
    if let Some(paths) = canonical_exe
        .iter()
        .chain(exe.iter())
        .chain(argv_path.iter())
        .find_map(|path| installed_root(path))
    {
        return paths;
    }
    if let Some(paths) = argv_path
        .as_deref()
        .and_then(|path| host.canonicalize(path).ok())
        .and_then(|path| installed_root(&path))
    {
        return paths;
    }
    (fallback(), None)
}

fn valid_root(host: &dyn Host, root: &Path) -> bool {
    host.metadata(
        &root.join("Modules/CMakeDetermineSystem.cmake"),
        Follow::Yes,
    )
    .is_ok_and(|metadata| metadata.kind == FileKind::File)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostResult, PureHost};

    #[test]
    fn canonical_executable_wins_over_shim_and_invalid_override() -> HostResult<()> {
        let shim = PathBuf::from("/shim/bin/cmake");
        let exe = PathBuf::from("/relocated/install/bin/rcmake");
        let root = PathBuf::from("/relocated/install/share/cmake-4.5");
        let host = PureHost::new(PathBuf::from("/work"), shim.clone());
        host.seed_file(&exe, b"")?;
        host.create_dir_all(Path::new("/shim/bin"))?;
        host.symlink(&exe, &shim)?;
        host.seed_file(&root.join("Modules/CMakeDetermineSystem.cmake"), b"")?;
        host.seed_file(
            Path::new("/shim/share/cmake-4.5/Modules/CMakeDetermineSystem.cmake"),
            b"",
        )?;
        host.create_dir_all(Path::new("/work/data/Modules"))?;
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));

        assert_eq!(discover_cmake_root(&host, None), (exe.clone(), Some(root)));
        let override_root = PathBuf::from("/work/data");
        host.seed_file(
            &override_root.join("Modules/CMakeDetermineSystem.cmake"),
            b"",
        )?;
        assert_eq!(discover_cmake_root(&host, None), (exe, Some(override_root)));
        Ok(())
    }

    #[test]
    fn follows_relative_argv_shim_when_current_executable_has_no_data() -> HostResult<()> {
        let exe = PathBuf::from("/relocated/bin/rcmake");
        let root = PathBuf::from("/relocated/share/cmake-4.5");
        let host = PureHost::new(PathBuf::from("/work"), PathBuf::from("/other/bin/cmake"));
        host.seed_file(&exe, b"")?;
        host.create_dir_all(Path::new("/work/shim/bin"))?;
        host.symlink(&exe, Path::new("/work/shim/bin/cmake"))?;
        host.seed_file(&root.join("Modules/CMakeDetermineSystem.cmake"), b"")?;

        assert_eq!(
            discover_cmake_root(&host, Some(&"shim/bin/cmake".into())),
            (exe, Some(root))
        );
        Ok(())
    }
}
