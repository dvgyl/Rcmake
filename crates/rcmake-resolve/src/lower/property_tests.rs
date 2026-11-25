use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_host::{Host, PureHost, ReadRange};
use rcmake_model::{
    BT, DirId, GeneratorKind, ProjectModel, Target, TargetKind, TargetOrigin, UsageField,
};

use crate::{BuildStep, Invocation, resolve};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn model(system: &str, values: &[(&str, &str)]) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("/project".into(), "/build".into());
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), system.into());
    for &(name, value) in values {
        model.global.props.set(name.into(), value.into());
    }
    model
}

fn target(name: &str, kind: TargetKind, source: &str) -> Target {
    let mut target = Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    target
        .sources
        .push(BT::new(source.into(), Default::default()));
    target
}

fn has(invocation: &Invocation, word: &str) -> bool {
    invocation.argv.iter().any(|arg| arg == word)
}

#[test]
fn lowers_pie_ipo_warning_as_error_and_launcher_arguments() -> TestResult {
    let mut model = model(
        "Darwin",
        &[
            ("CMAKE_CXX_COMPILER", "c++"),
            ("CMAKE_BUILD_TYPE", "Debug"),
            ("CMAKE_CXX_COMPILE_OPTIONS_PIE", "-fPIE"),
            ("CMAKE_CXX_LINK_OPTIONS_PIE", "-fPIE;-Xlinker;-pie"),
            ("CMAKE_CXX_LINK_PIE_SUPPORTED", "1"),
            ("CMAKE_CXX_COMPILE_OPTIONS_PIC", "-fPIC"),
            ("CMAKE_CXX_COMPILE_OPTIONS_IPO", "-flto=thin"),
            ("CMAKE_CXX_LINK_OPTIONS_IPO", "-fuse-ld=lld"),
            ("CMAKE_CXX_COMPILE_OPTIONS_WARNING_AS_ERROR", "-Werror"),
            (
                "CMAKE_CXX_LINK_FLAGS",
                "-Wl,-search_paths_first -Wl,-headerpad_max_install_names",
            ),
            (
                "CMAKE_EXECUTABLE_CREATE_CXX_FLAGS",
                "-Wl,-search_paths_first -Wl,-headerpad_max_install_names",
            ),
        ],
    );
    let mut app = target("app", TargetKind::Executable, "main.cpp");
    app.policies
        .policies
        .set(PolicyId::new(69), PolicyStatus::New)?;
    app.policies
        .policies
        .set(PolicyId::new(83), PolicyStatus::New)?;
    for (name, value) in [
        ("INTERPROCEDURAL_OPTIMIZATION", "OFF"),
        ("INTERPROCEDURAL_OPTIMIZATION_DEBUG", "ON"),
        ("POSITION_INDEPENDENT_CODE", "ON"),
        ("COMPILE_WARNING_AS_ERROR", "ON"),
        ("OSX_ARCHITECTURES", "arm64"),
        (
            "CXX_COMPILER_LAUNCHER",
            "$<$<CONFIG:Debug>:/tool path/wrap;compile argument>",
        ),
        ("CXX_LINKER_LAUNCHER", "/tool path/wrap;link argument"),
    ] {
        app.props.set(name.into(), value.into());
    }
    model.add_target(app)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/project/main.cpp"), b"int main() {}")?;
    for disabled in [false, true] {
        if disabled {
            model
                .global
                .props
                .set("_RCMAKE_NO_WARNING_AS_ERROR".into(), "ON".into());
        }
        let result = resolve(&model, &host, &[])?;
        let [
            BuildStep::Compile {
                invocation: compiler,
                ..
            },
            BuildStep::Link { invocations, .. },
        ] = result.graphs[0].steps.as_slice()
        else {
            panic!("Expected one compile and one link step")
        };
        let [linker] = invocations.as_slice() else {
            panic!("Expected one link invocation")
        };
        assert_eq!(compiler.argv[..2], ["/tool path/wrap", "compile argument"]);
        assert_eq!(linker.argv[..2], ["/tool path/wrap", "link argument"]);
        for invocation in [compiler, linker] {
            for flag in ["-fPIE", "-flto=thin", "-arch", "arm64"] {
                assert!(has(invocation, flag), "Missing {flag} in {invocation:?}");
            }
            assert!(!has(invocation, "-fPIC"));
        }
        assert_eq!(has(compiler, "-Werror"), !disabled);
        assert!(has(linker, "-pie") && has(linker, "-fuse-ld=lld"));
        for flag in [
            "-Wl,-search_paths_first",
            "-Wl,-headerpad_max_install_names",
        ] {
            assert_eq!(linker.argv.iter().filter(|arg| *arg == flag).count(), 1);
        }
    }
    Ok(())
}

#[test]
fn creates_and_reuses_a_language_specific_pch() -> TestResult {
    let mut model = model(
        "Linux",
        &[
            ("CMAKE_CXX_COMPILER", "c++"),
            ("CMAKE_PCH_EXTENSION", ".gch"),
            (
                "CMAKE_CXX_COMPILE_OPTIONS_CREATE_PCH",
                "-x;c++-header;-include;<PCH_HEADER>",
            ),
            ("CMAKE_CXX_COMPILE_OPTIONS_USE_PCH", "-include;<PCH_HEADER>"),
        ],
    );
    let precompiled = if cfg!(windows) {
        "C:/headers/local.hxx"
    } else {
        "/headers/local.hxx"
    };
    let mut provider = target("provider", TargetKind::StaticLibrary, "provider.cpp");
    provider.usage.push(
        UsageField::PrecompileHeaders,
        BT::new(
            "$<$<COMPILE_LANGUAGE:CXX>:<vector$<ANGLE-R>>>".into(),
            Default::default(),
        ),
    );
    provider.usage.push(
        UsageField::PrecompileHeaders,
        BT::new(precompiled.into(), Default::default()),
    );
    let provider = model.add_target(provider)?;
    let mut consumer = target("consumer", TargetKind::Executable, "consumer.cpp");
    consumer
        .props
        .set("PRECOMPILE_HEADERS_REUSE_FROM".into(), "provider".into());
    model.add_target(consumer)?;
    let host = PureHost::default();
    for file in ["provider.cpp", "consumer.cpp"] {
        host.seed_file(&Path::new("/project").join(file), b"int main() {}")?;
    }
    let result = resolve(&model, &host, &[])?;
    let header = "/build/CMakeFiles/provider.dir/cmake_pch.hxx";
    let artifact = "CMakeFiles/provider.dir/cmake_pch.hxx.gch";
    let contents = host.read(Path::new(header), ReadRange::All { limit: 4096 })?;
    assert!(
        contents
            .windows(b"#include <vector>".len())
            .any(|word| word == b"#include <vector>")
    );
    assert!(String::from_utf8(contents)?.contains(&format!("#include \"{precompiled}\"")));
    let graph = &result.graphs[0];
    assert_eq!(graph.targets[1].depends, vec![provider.index()]);
    assert_eq!(
        result.compile_inputs.get(artifact.as_bytes()),
        Some(&vec![BString::from(header)])
    );
    for step in &graph.steps {
        if let BuildStep::Compile {
            object, invocation, ..
        } = step
        {
            assert!(has(invocation, "-include") && has(invocation, header));
            if object == artifact {
                assert!(has(invocation, "c++-header"));
            } else {
                assert_eq!(
                    result.compile_inputs.get(object),
                    Some(&vec![header.into(), artifact.into()])
                );
            }
        } else if let BuildStep::Link { inputs, .. } = step {
            assert!(!inputs.iter().any(|input| input == artifact));
        }
    }
    Ok(())
}

#[test]
fn windows_shared_library_does_not_pass_pic_flags_to_resource_compiler() -> TestResult {
    let mut model = model(
        "Windows",
        &[
            ("CMAKE_C_COMPILER", "cl.exe"),
            ("CMAKE_C_COMPILER_ID", "MSVC"),
            ("CMAKE_RC_COMPILER", "rc.exe"),
            (
                "CMAKE_RC_COMPILE_OBJECT",
                "<CMAKE_RC_COMPILER> <DEFINES> <FLAGS> /fo <OBJECT> <SOURCE>",
            ),
        ],
    );
    let mut library = target("zlib", TargetKind::SharedLibrary, "zlib.c");
    library
        .sources
        .push(BT::new("win32/zlib1.rc".into(), Default::default()));
    model.add_target(library)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/project/zlib.c"), b"void version(void) {}")?;
    host.seed_file(Path::new("/project/win32/zlib1.rc"), b"")?;

    let result = resolve(&model, &host, &[])?;
    let invocation = result.graphs[0]
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile {
                source, invocation, ..
            } if source.ends_with(b"/win32/zlib1.rc") => Some(invocation),
            _ => None,
        })
        .ok_or("Missing resource compile step")?;
    assert_eq!(
        invocation.argv.first().map(|word| word.as_slice()),
        Some(b"rc.exe".as_slice())
    );
    assert!(!has(invocation, "-fPIC"));
    assert!(!has(invocation, "-fPIE"));
    Ok(())
}

#[test]
fn apple_universal_pch_uses_one_create_per_architecture() -> TestResult {
    let mut model = model(
        "Darwin",
        &[
            ("CMAKE_CXX_COMPILER", "clang++"),
            ("CMAKE_PCH_EXTENSION", ".pch"),
            (
                "CMAKE_CXX_COMPILE_OPTIONS_CREATE_PCH",
                "-x;c++-header;-include;<PCH_HEADER>",
            ),
            ("CMAKE_CXX_COMPILE_OPTIONS_USE_PCH", "-include;<PCH_HEADER>"),
            ("CMAKE_OSX_ARCHITECTURES", "arm64;x86_64"),
            ("CMAKE_APPLE_ARCH_SYSROOTS", "/arm SDK;/intel SDK"),
        ],
    );
    let mut library = target("lib", TargetKind::StaticLibrary, "lib.cpp");
    library
        .props
        .set("OSX_ARCHITECTURES".into(), "x86_64;arm64".into());
    library.usage.push(
        UsageField::PrecompileHeaders,
        BT::new("<vector>".into(), Default::default()),
    );
    model.add_target(library)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/project/lib.cpp"), b"int one() { return 1; }")?;
    let result = resolve(&model, &host, &[])?;
    let compiles = result.graphs[0]
        .steps
        .iter()
        .filter_map(|step| match step {
            BuildStep::Compile {
                object, invocation, ..
            } => Some((object, invocation)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let plain = host.read(
        Path::new("/build/CMakeFiles/lib.dir/cmake_pch.hxx"),
        rcmake_host::ReadRange::All { limit: 4096 },
    )?;
    assert_eq!(plain, b"/* generated by CMake */\n\n#ifdef __cplusplus\n#include <vector>\n#endif // __cplusplus\n");
    assert_eq!(compiles.len(), 3);
    for ((object, invocation), (arch, sdk)) in compiles.iter().zip([
        ("x86_64", "-isysroot/intel SDK"),
        ("arm64", "-isysroot/arm SDK"),
    ]) {
        assert!(object.to_string().contains(arch));
        let args = &invocation.argv;
        assert_eq!(args.iter().filter(|arg| *arg == b"-arch").count(), 1);
        assert!(has(invocation, arch));
        assert!(has(invocation, &format!("-Xarch_{arch}")));
        assert!(has(invocation, sdk));
    }
    let (object, consumer) = compiles[2];
    assert!(has(
        consumer,
        "-include/build/CMakeFiles/lib.dir/cmake_pch_x86_64.hxx"
    ));
    assert!(has(
        consumer,
        "-include/build/CMakeFiles/lib.dir/cmake_pch_arm64.hxx"
    ));
    assert_eq!(result.compile_inputs[object].len(), 4);
    Ok(())
}

#[test]
fn linked_pch_has_two_outputs_and_reuse_links_the_provider_object_once() -> TestResult {
    let mut model = model(
        "Windows",
        &[
            ("CMAKE_CXX_COMPILER", "cl.exe"),
            ("CMAKE_CXX_COMPILER_ID", "MSVC"),
            ("CMAKE_LINK_PCH", "ON"),
            ("CMAKE_PCH_COPY_COMPILE_PDB", "ON"),
            ("CMAKE_BUILD_TYPE", "Debug"),
            (
                "CMAKE_MSVC_DEBUG_INFORMATION_FORMAT_DEFAULT",
                "$<$<CONFIG:Debug>:ProgramDatabase>",
            ),
            (
                "CMAKE_CXX_COMPILE_OPTIONS_MSVC_DEBUG_INFORMATION_FORMAT_ProgramDatabase",
                "-Zi",
            ),
            ("CMAKE_PCH_EXTENSION", ".pch"),
            ("CMAKE_CXX_OUTPUT_EXTENSION", ".obj"),
            (
                "CMAKE_CXX_COMPILE_OBJECT",
                "<CMAKE_CXX_COMPILER> <FLAGS> /Fo<OBJECT> -c <SOURCE>",
            ),
            (
                "CMAKE_CXX_COMPILE_OPTIONS_CREATE_PCH",
                "/Yc<PCH_HEADER>;/FI<PCH_HEADER>;/Fp<PCH_FILE>",
            ),
            (
                "CMAKE_CXX_COMPILE_OPTIONS_USE_PCH",
                "/Yu<PCH_HEADER>;/FI<PCH_HEADER>;/Fp<PCH_FILE>",
            ),
        ],
    );
    let mut provider = target("provider", TargetKind::ObjectLibrary, "provider.cpp");
    provider.usage.push(
        UsageField::PrecompileHeaders,
        BT::new("<vector>".into(), Default::default()),
    );
    model.add_target(provider)?;
    let mut consumer = target("consumer", TargetKind::Executable, "consumer.cpp");
    consumer
        .props
        .set("PRECOMPILE_HEADERS_REUSE_FROM".into(), "provider".into());
    model.add_target(consumer)?;
    let host = PureHost::default();
    for file in ["provider.cpp", "consumer.cpp"] {
        host.seed_file(&Path::new("/project").join(file), b"int main() {}")?;
    }
    let result = resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    let (object, pch) = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile {
                object,
                byproducts,
                invocation,
                ..
            } => {
                let pch = byproducts.first()?;
                assert!(has(invocation, &format!("/Fo{object}")));
                assert!(has(invocation, &format!("/Fp{pch}")));
                assert!(has(invocation, "-Zi"));
                Some((object, pch))
            }
            _ => None,
        })
        .ok_or("Missing precompiled header creation")?;
    assert!(object.ends_with(b".obj") && pch.ends_with(b".pch"));
    assert!(
        graph
            .steps
            .iter()
            .any(|step| matches!(step, BuildStep::Link { inputs, .. }
        if inputs.iter().filter(|path| *path == object).count() == 1 && !inputs.contains(pch)))
    );
    assert!(
        result
            .compile_inputs
            .values()
            .any(|files| files.contains(pch))
    );
    Ok(())
}
