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
    let mut provider = target("provider", TargetKind::StaticLibrary, "provider.cpp");
    provider.usage.push(
        UsageField::PrecompileHeaders,
        BT::new(
            "$<$<COMPILE_LANGUAGE:CXX>:<vector$<ANGLE-R>>>".into(),
            Default::default(),
        ),
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
