use std::path::Path;

use rcmake_host::{HostResult, PureHost};

pub(crate) fn seed(host: &PureHost) -> HostResult<()> {
    let modules = Path::new("/usr/local/share/cmake-4.5/Modules");
    for (name, contents) in [
        ("CMakeDetermineSystem.cmake", ""),
        ("CMakeSystemSpecificInitialize.cmake", ""),
        ("CMakeSystemSpecificInformation.cmake", ""),
        (
            "CMakeNinjaFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /tools/ninja CACHE FILEPATH \"\")\n",
        ),
        (
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /tools/make CACHE FILEPATH \"\")\n",
        ),
    ] {
        host.seed_file(&modules.join(name), contents.as_bytes())?;
    }
    Ok(())
}
