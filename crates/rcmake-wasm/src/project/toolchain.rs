use std::path::Path;

use rcmake_base::CMAKE_VERSION_STR;
use rcmake_host::{HostResult, PureHost};

use super::virtual_path;

pub(super) fn seed(host: &PureHost, build: &Path, cmake_root: &Path) -> HostResult<()> {
    let platform = build.join("CMakeFiles").join(CMAKE_VERSION_STR);
    let modules = cmake_root.join("Modules");
    host.seed_file(
        &platform.join("CMakeSystem.cmake"),
        br#"
set(CMAKE_SYSTEM Linux)
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_SYSTEM_LOADED 1)
set(UNIX 1)
set(CMAKE_CROSSCOMPILING FALSE)
set(CMAKE_SIZEOF_VOID_P 8)
"#,
    )?;
    for (name, content) in [
        ("CMakeDetermineSystem.cmake", ""),
        (
            "CMakeNinjaFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /usr/bin/ninja CACHE FILEPATH \"Ninja executable\")",
        ),
        (
            "CMakeSystemSpecificInitialize.cmake",
            "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)",
        ),
        (
            "CMakeSystemSpecificInformation.cmake",
            r#"
set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)
set(CMAKE_EXECUTABLE_SUFFIX "")
set(CMAKE_STATIC_LIBRARY_PREFIX lib)
set(CMAKE_STATIC_LIBRARY_SUFFIX .a)
set(CMAKE_SHARED_LIBRARY_PREFIX lib)
set(CMAKE_SHARED_LIBRARY_SUFFIX .so)
set(CMAKE_SHARED_MODULE_PREFIX lib)
set(CMAKE_SHARED_MODULE_SUFFIX .so)
set(CMAKE_AR /usr/bin/llvm-ar)
set(CMAKE_RANLIB /usr/bin/llvm-ranlib)
"#,
        ),
        (
            "GNUInstallDirs.cmake",
            include_str!("../../../../modules/cmake-4.5/Modules/GNUInstallDirs.cmake"),
        ),
    ] {
        host.seed_file(&modules.join(name), content.as_bytes())?;
    }
    for (lang, compiler, extensions, standards, default) in [
        ("C", "clang", "c", &["90", "99", "11", "17", "23"][..], "17"),
        (
            "CXX",
            "clang++",
            "C;cc;cpp;cxx;c++;CPP",
            &["98", "11", "14", "17", "20", "23", "26"][..],
            "17",
        ),
    ] {
        let prefix = format!("CMAKE_{lang}");
        let executable = virtual_path(&format!("/usr/bin/{compiler}"));
        host.seed_file(&executable, b"")?;
        let executable = executable.display();
        let features = standards
            .iter()
            .map(|standard| format!("{}_std_{standard}", lang.to_ascii_lowercase()))
            .collect::<Vec<_>>()
            .join(";");
        let compiler = format!(
            r#"
set({prefix}_COMPILER "{executable}")
set({prefix}_COMPILER_ID Clang)
set({prefix}_COMPILER_VERSION 19.1.0)
set({prefix}_COMPILER_LOADED 1)
set({prefix}_COMPILER_WORKS TRUE)
set({prefix}_SOURCE_FILE_EXTENSIONS "{extensions}")
set({prefix}_IGNORE_EXTENSIONS "h;hh;hpp;hxx")
set({prefix}_COMPILE_FEATURES "{features}")
set({prefix}_STANDARD_DEFAULT {default})
set({prefix}_EXTENSIONS_DEFAULT ON)
"#
        );
        host.seed_file(
            &platform.join(format!("CMake{lang}Compiler.cmake")),
            compiler.as_bytes(),
        )?;
        let mut information = format!(
            r#"
set({prefix}_INFORMATION_LOADED 1)
set({prefix}_OUTPUT_EXTENSION .o)
set({prefix}_COMPILE_OBJECT "<{prefix}_COMPILER> <DEFINES> <INCLUDES> <FLAGS> -o <OBJECT> -c <SOURCE>")
set({prefix}_LINK_EXECUTABLE "<{prefix}_COMPILER> <FLAGS> <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>")
set({prefix}_CREATE_SHARED_LIBRARY "<{prefix}_COMPILER> <CMAKE_SHARED_LIBRARY_{lang}_FLAGS> <LANGUAGE_COMPILE_FLAGS> <LINK_FLAGS> <SONAME_FLAG><TARGET_SONAME> -o <TARGET> <OBJECTS> <LINK_LIBRARIES>")
set({prefix}_CREATE_SHARED_MODULE "${{{prefix}_CREATE_SHARED_LIBRARY}}")
set({prefix}_ARCHIVE_CREATE "<CMAKE_AR> qc <TARGET> <LINK_FLAGS> <OBJECTS>")
set({prefix}_ARCHIVE_APPEND "<CMAKE_AR> q <TARGET> <LINK_FLAGS> <OBJECTS>")
set({prefix}_ARCHIVE_FINISH "<CMAKE_RANLIB> <TARGET>")
set({prefix}_COMPILE_OPTIONS_PIC -fPIC)
set({prefix}_DEPFILE_FLAGS "-MD -MT <DEP_TARGET> -MF <DEP_FILE>")
set(CMAKE_SHARED_LIBRARY_{lang}_FLAGS -shared)
set(CMAKE_SHARED_LIBRARY_SONAME_{lang}_FLAG "-Wl,-soname,")
set({prefix}_FLAGS_DEBUG "-g" CACHE STRING "{lang} Debug flags")
set({prefix}_FLAGS_RELEASE "-O3 -DNDEBUG" CACHE STRING "{lang} Release flags")
set({prefix}_FLAGS_RELWITHDEBINFO "-O2 -g -DNDEBUG" CACHE STRING "{lang} RelWithDebInfo flags")
set({prefix}_FLAGS_MINSIZEREL "-Os -DNDEBUG" CACHE STRING "{lang} MinSizeRel flags")
"#
        );
        use std::fmt::Write as _;
        for standard in standards {
            let dialect = if lang == "CXX" { "c++" } else { "c" };
            let extension = if lang == "CXX" { "gnu++" } else { "gnu" };
            let _ = writeln!(
                information,
                "set({prefix}{standard}_STANDARD_COMPILE_OPTION -std={dialect}{standard})\nset({prefix}{standard}_EXTENSION_COMPILE_OPTION -std={extension}{standard})"
            );
        }
        host.seed_file(
            &modules.join(format!("CMake{lang}Information.cmake")),
            information.as_bytes(),
        )?;
    }
    Ok(())
}
