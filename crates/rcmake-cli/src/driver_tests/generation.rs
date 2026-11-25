use super::*;

fn assert_generation_finished(host: &PureHost) {
    let stdout = host.stream_bytes(Stream::Stdout);
    assert!(stdout.to_string().contains("-- Generating done"));
    assert!(
        !stdout
            .to_string()
            .contains("-- Build files have been written")
    );
    for path in ["build.ninja", "cmake_install.cmake", "CMakeCache.txt"] {
        assert!(
            host.metadata(
                Path::new("/work/out").join(path).as_path(),
                rcmake_host::Follow::Yes
            )
            .is_ok(),
            "{path}"
        );
    }
}

#[test]
fn install_export_errors_follow_directory_order() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(Example NONE)
install(EXPORT Standard DESTINATION lib)
add_subdirectory(child)
install(EXPORT_ANDROID_MK Android DESTINATION lib)
install(PACKAGE_INFO Example EXPORT Package)
install(SBOM Example EXPORTS Document)
"#,
    )
    .unwrap();
    host.seed_file(
        Path::new("/work/child/CMakeLists.txt"),
        b"install(EXPORT Child DESTINATION lib)\n",
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
        1
    );
    let stderr = String::from_utf8(host.stream_bytes(Stream::Stderr).to_vec()).unwrap();
    let errors: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("CMake Error:"))
        .collect();
    assert_eq!(
        errors,
        [
            r#"CMake Error: INSTALL(EXPORT) given unknown export "Standard""#,
            r#"CMake Error: INSTALL(EXPORT_ANDROID_MK) given unknown export "Android""#,
            r#"CMake Error: INSTALL(PACKAGE_INFO) given unknown export "Package""#,
            r#"CMake Error: INSTALL(EXPORT) given unknown export "Child""#,
        ]
    );
    assert_generation_finished(&host);
}

#[test]
fn ninja_reports_each_target_with_a_missing_scoped_compile_rule() {
    let host = project_host();
    let modules = Path::new("/usr/local/share/cmake-4.5/Modules");
    for (name, contents) in [
        (
            "CMakeDetermineCXXCompiler.cmake",
            r#"file(WRITE "${CMAKE_PLATFORM_INFO_DIR}/CMakeCXXCompiler.cmake" "set(CMAKE_CXX_COMPILER /bin/c++)\nset(CMAKE_CXX_COMPILER_LOADED 1)\nset(CMAKE_CXX_SOURCE_FILE_EXTENSIONS cxx)\n")"#,
        ),
        (
            "CMakeCXXInformation.cmake",
            r#"set(CMAKE_CXX_INFORMATION_LOADED 1)
set(CMAKE_CXX_COMPILE_OBJECT "<CMAKE_CXX_COMPILER> <FLAGS> -c <SOURCE> -o <OBJECT>")"#,
        ),
        (
            "CMakeTestCXXCompiler.cmake",
            "set(CMAKE_CXX_COMPILER_WORKS TRUE)",
        ),
    ] {
        host.seed_file(&modules.join(name), contents.as_bytes())
            .unwrap();
    }
    for (name, contents) in [
        ("/bin/c++", ""),
        ("/work/a.cxx", "int main() { return 0; }"),
        ("/work/b.cxx", "void second() {}"),
        ("/work/lang/CMakeLists.txt", "enable_language(CXX)"),
        (
            "/work/CMakeLists.txt",
            r#"
cmake_minimum_required(VERSION 4.5)
cmake_policy(SET CMP0220 OLD)
project(Scoped NONE)
add_subdirectory(lang)
add_executable(first a.cxx b.cxx)
add_executable(second a.cxx)
"#,
        ),
    ] {
        host.seed_file(Path::new(name), contents.as_bytes())
            .unwrap();
    }
    let code = invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]);
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr
            .matches("Missing variable is:\nCMAKE_CXX_COMPILE_OBJECT\n")
            .count(),
        2,
        "{stderr}"
    );
    assert_generation_finished(&host);
}

#[test]
fn configure_saves_a_new_cache_before_a_resolver_error() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Example NONE)\nset(SAVED yes CACHE STRING \"Saves before generation\")\nadd_executable(app missing.c)\n",
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
        1
    );
    let cache = host
        .read(
            Path::new("/work/out/CMakeCache.txt"),
            ReadRange::All { limit: 16384 },
        )
        .unwrap();
    let cache = BString::from(cache).to_string();
    assert!(cache.contains("SAVED:STRING=yes\n"));
    assert!(cache.contains("CMAKE_NUMBER_OF_MAKEFILES:INTERNAL=1\n"));
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("CMake Generate step failed.")
    );
}

#[test]
fn configure_saves_the_cache_when_codegen_name_is_reserved() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Example NONE)\nadd_custom_target(codegen)\n",
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
        1
    );
    let cache = host
        .read(
            Path::new("/work/out/CMakeCache.txt"),
            ReadRange::All { limit: 16384 },
        )
        .unwrap();
    assert!(
        BString::from(cache)
            .to_string()
            .contains("CMAKE_NUMBER_OF_MAKEFILES:INTERNAL=1\n")
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains(r#"The target name "codegen" is reserved."#)
    );
}

#[test]
fn reconfigure_preserves_removal_of_extra_generator_cache_entry() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Example NONE)\n",
    )
    .unwrap();
    let args = ["cmake", "-S", ".", "-B", "out", "-G", "Ninja"];
    let extra_generator = b"CMAKE_EXTRA_GENERATOR";
    let load_cache = || crate::cache::load(host.as_ref(), Path::new("/work/out")).unwrap();
    assert_eq!(invoke(&host, &args), 0);
    assert!(load_cache().contains(extra_generator));

    let mut unset_args = args.to_vec();
    unset_args.push("-UCMAKE_EXTRA_GENERATOR");
    assert_eq!(invoke(&host, &unset_args), 0);
    assert!(!load_cache().contains(extra_generator));

    assert_eq!(invoke(&host, &args), 0);
    assert!(!load_cache().contains(extra_generator));
}
