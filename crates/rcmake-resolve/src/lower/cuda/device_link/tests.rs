use rcmake_diag::{Backtrace, PolicyMap};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, EvalContext, EvalSite, GenexError, GenexHost, InterfaceView,
    TargetHandle, UsageTraversal, evaluate, parse,
};
use rcmake_model::{BT, DirId, TargetOrigin};

use super::*;

fn target(name: &str, kind: TargetKind) -> Target {
    Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    )
}

fn item(value: &str, kind: ResolvedLinkKind) -> ResolvedLinkItem {
    ResolvedLinkItem {
        scope: rcmake_model::UsageScope::Private,
        text: BT::new(value.into(), Backtrace::default()),
        kind,
        feature: None,
        direct: true,
    }
}

#[test]
fn unresolved_static_library_requires_device_link() -> Result<(), rcmake_model::ModelError> {
    let mut model = ProjectModel::default();
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "CXX;CUDA".into());
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_CUDA_COMPILER_HAS_DEVICE_LINK_PHASE".into(),
        "ON".into(),
    );
    let mut library = target("kernels", TargetKind::StaticLibrary);
    library
        .props
        .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
    let id = model.add_target(library)?;
    let mut app = target("app", TargetKind::Executable);
    let items = [item("kernels", ResolvedLinkKind::Library(id))];
    let languages = [BString::from("CUDA")];
    assert_eq!(
        device_link_language(&model, &app, &languages, &items),
        Some(b"CUDA".as_slice())
    );
    assert_eq!(device_link_language(&model, &app, &[], &items), None);
    app.props
        .set("CUDA_RESOLVE_DEVICE_SYMBOLS".into(), "OFF".into());
    assert_eq!(device_link_language(&model, &app, &languages, &items), None);
    app.props
        .set("CUDA_RESOLVE_DEVICE_SYMBOLS".into(), "ON".into());
    assert_eq!(
        device_link_language(&model, &app, &[], &[]),
        Some(b"CUDA".as_slice())
    );
    for kind in [
        TargetKind::ObjectLibrary,
        TargetKind::InterfaceLibrary,
        TargetKind::Utility,
    ] {
        app.kind = kind;
        assert_eq!(device_link_language(&model, &app, &languages, &items), None);
    }
    Ok(())
}

#[test]
fn library_filter_skips_host_libraries_and_flags() {
    let model = ProjectModel::default();
    let app = target("app", TargetKind::Executable);
    let items = [
        "-framework",
        "Cocoa",
        "-framework Metal",
        "-pthread",
        "/lib/libhost.so.1",
        "/lib/libkernel.a",
        "/lib/libkernel.a",
        "-lcuda",
        "-L/cuda",
        "--library=cudadevrt",
        "bare",
    ]
    .map(|name| item(name, ResolvedLinkKind::Raw));
    let libraries = device_libraries(&model, &app, &ConfigId::default(), &items, b"CUDA");
    assert_eq!(
        libraries.args,
        [
            "/lib/libkernel.a",
            "-lcuda",
            "-L/cuda",
            "--library=cudadevrt",
            "bare",
        ]
    );
    assert_eq!(libraries.files, ["/lib/libkernel.a"]);
}

#[test]
fn link_options_obey_markers_and_policy() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_CUDA_DEVICE_LINKER_WRAPPER_FLAG".into(),
        "-Xlinker=".into(),
    );
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_CUDA_DEVICE_LINKER_WRAPPER_FLAG_SEP".into(),
        ",".into(),
    );
    let mut app = target("app", TargetKind::Executable);
    let options = [
        "-common",
        "<DEVICE_LINK>",
        "LINKER:--first,--second",
        "</DEVICE_LINK>",
        "<HOST_LINK>",
        "-host",
        "</HOST_LINK>",
    ]
    .map(BString::from);
    assert!(link_options(&model, &app, b"CUDA", &options, true)?.is_empty());
    app.policies
        .policies
        .set(PolicyId::new(105), PolicyStatus::New)?;
    assert_eq!(
        link_options(&model, &app, b"CUDA", &options, true)?,
        ["-common", "-Xlinker=--first,--second",]
    );
    assert_eq!(
        link_options(&model, &app, b"CUDA", &options, false)?,
        ["-common", "-host"]
    );
    Ok(())
}

struct DeviceHost;

impl GenexHost for DeviceHost {
    fn find_target(&self, _name: &[u8]) -> Option<TargetHandle> {
        None
    }

    fn target_name(&self, _target: TargetHandle) -> BString {
        "kernels".into()
    }

    fn raw_property(&self, _target: TargetHandle, _name: &[u8]) -> Option<BString> {
        None
    }

    fn transitive_property(
        &mut self,
        _target: TargetHandle,
        _name: &[u8],
        _ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        Ok(InterfaceView::default())
    }

    fn artifact(
        &self,
        _target: TargetHandle,
        _kind: ArtifactKind,
        _part: ArtifactPart,
        _config: &ConfigId,
    ) -> Option<BString> {
        None
    }

    fn link_closure(
        &mut self,
        _target: TargetHandle,
        _ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        Ok(Vec::new())
    }

    fn definition(&self, _name: &[u8]) -> Option<BString> {
        None
    }

    fn is_device_link(&self, _ctx: &EvalContext) -> bool {
        true
    }
}

#[test]
fn static_device_options_keep_shell_groups_and_escaped_commas()
-> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    for (key, value) in [
        ("CMAKE_CUDA_DEVICE_COMPILER_WRAPPER_FLAG", "-Xcompiler="),
        ("CMAKE_CUDA_DEVICE_COMPILER_WRAPPER_FLAG_SEP", ","),
        ("CMAKE_CUDA_DEVICE_LINKER_WRAPPER_FLAG", "-Xlinker="),
        ("CMAKE_CUDA_DEVICE_LINKER_WRAPPER_FLAG_SEP", ","),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(key.into(), value.into());
    }
    let mut library = target("kernels", TargetKind::StaticLibrary);
    library
        .policies
        .policies
        .set(PolicyId::new(105), PolicyStatus::New)?;
    library
        .props
        .set("STATIC_LIBRARY_OPTIONS".into(), "STATIC_FLAG".into());
    let ctx = EvalContext {
        head: Some(TargetHandle(0)),
        link_language: Some("CUDA".into()),
        traversal: UsageTraversal::Link,
        site: EvalSite {
            property: Some("LINK_OPTIONS".into()),
            ..EvalSite::default()
        },
        ..EvalContext::default()
    };
    let ast = parse(
        br#"SHELL:--host "with space";LINKER:SHELL:--first --second;$<HOST_LINK:--host-only>;$<DEVICE_LINK:LINKER:--device$<COMMA>--end,SHELL:--device-arg "two words">"#,
    )?;
    let (raw, _) = evaluate(&ast, &ctx, &mut DeviceHost)?;
    let options = list::split(&raw, list::Empty::Drop);
    assert_eq!(
        link_options(&model, &library, b"CUDA", &options, true)?,
        [
            "-Xcompiler=--host,with space",
            "-Xlinker=--first,--second",
            "-Xlinker=--device,--end",
            "--device-arg",
            "two words",
        ]
    );
    Ok(())
}

#[test]
fn driver_expands_library_rule_and_records_response_file_arguments() -> Result<(), ResolveError> {
    let mut model = ProjectModel::default();
    model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    model.set_root_dirs("/src".into(), "/build".into());
    model.dir_mut(DirId::ROOT).vars.extend([
        ("CMAKE_CUDA_COMPILER".into(), "/cuda tools/nvcc".into()),
        ("CMAKE_CUDA_COMPILER_ARG1".into(), r#"--required "required value""#.into()),
        ("CMAKE_CUDA_DEVICE_LINK_MODE".into(), "EXPLICIT".into()),
        ("CMAKE_CUDA_RESPONSE_FILE_DEVICE_LINK_FLAG".into(), "--options-file ".into()),
        (
            "CMAKE_CUDA_DEVICE_LINK_LIBRARY".into(),
            "<CMAKE_CUDA_COMPILER> <CMAKE_CUDA_COMPILER_ARG1> -dlink <FLAGS> <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES> <OBJECT_DIR> <TARGET_SUPPORT_DIR> <TARGET_BASE>".into(),
        ),
    ]);
    let mut lib = target("kernels", TargetKind::StaticLibrary);
    lib.props.set(
        "CUDA_COMPILER_LAUNCHER".into(),
        "/tool path/wrap;launch argument".into(),
    );
    lib.props
        .set("CUDA_LINKER_LAUNCHER".into(), "link-wrapper".into());
    lib.props.set(
        "RULE_LAUNCH_LINK".into(),
        "rule-wrapper <TARGET_NAME>".into(),
    );
    model
        .add_target(lib.clone())
        .map_err(|error| ResolveError::Message(error.to_string().into()))?;
    let libraries = DeviceLibraries {
        args: vec!["lib device.a".into()],
        files: vec!["lib device.a".into()],
        ..DeviceLibraries::default()
    };
    let plan = plan(
        &model,
        &lib,
        &ConfigId("Debug".into()),
        b"CUDA",
        &DeviceLinkInputs {
            objects: &["kernel.o".into()],
            libraries: &libraries,
            directories: &["-L/device libs".into()],
            link_flags: &["--link-opt".into()],
            compile_flags: &["--compile-opt".into()],
        },
    )?;
    assert_eq!(
        plan.output,
        "CMakeFiles/kernels.dir/Debug/cmake_device_link.o"
    );
    assert_eq!(plan.inputs, ["kernel.o", "lib device.a"]);
    let DeviceLinkMode::Driver {
        invocation,
        response_file,
    } = plan.mode
    else {
        return Err(ResolveError::Message(
            "Expected driver device linking.".into(),
        ));
    };
    assert_eq!(
        invocation.argv,
        [
            "rule-wrapper",
            "kernels",
            "/cuda tools/nvcc",
            "--required",
            "required value",
            "-dlink",
            "--compile-opt",
            "--link-opt",
            "kernel.o",
            "-o",
            "CMakeFiles/kernels.dir/Debug/cmake_device_link.o",
            "-L/device libs",
            "lib device.a",
            "CMakeFiles/kernels.dir/Debug",
            "CMakeFiles/kernels.dir",
            "CMakeFiles/kernels.dir/Debug/cmake_device_link",
        ]
    );
    assert!(
        invocation
            .shell
            .to_string()
            .contains(r#"--required "required value""#)
    );
    assert_eq!(
        response_file,
        Some(DeviceLinkResponseFile {
            flag: "--options-file ".into(),
            objects: vec!["kernel.o".into()],
            libraries: vec!["lib device.a".into()],
        }),
    );
    Ok(())
}

#[test]
fn unresolved_archives_enable_device_ipo_when_the_executable_disables_it()
-> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    model.dir_mut(DirId::ROOT).vars.extend([
        ("CMAKE_CUDA_COMPILER".into(), "nvcc".into()),
        ("CMAKE_CUDA_COMPILER_ARG1".into(), "--required".into()),
        ("CMAKE_CUDA_DEVICE_LINK_OPTIONS_IPO".into(), " -dlto".into()),
        (
            "CMAKE_CUDA_DEVICE_LINK_EXECUTABLE".into(),
            "<CMAKE_CUDA_COMPILER> <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>".into(),
        ),
    ]);
    let mut app = target("app", TargetKind::Executable);
    let mut library = target("kernels", TargetKind::StaticLibrary);
    for target in [&mut app, &mut library] {
        target
            .policies
            .policies
            .set(PolicyId::new(69), PolicyStatus::New)?;
    }
    app.props
        .set("INTERPROCEDURAL_OPTIMIZATION".into(), "OFF".into());
    library
        .props
        .set("INTERPROCEDURAL_OPTIMIZATION_RELEASE".into(), "ON".into());
    library
        .props
        .set("CUDA_SEPARABLE_COMPILATION".into(), "ON".into());
    library.props.set("CUDA_ARCHITECTURES".into(), "75".into());
    let id = model.add_target(library)?;
    let mut archive = item("kernels", ResolvedLinkKind::Library(id));
    archive.direct = false;
    let items = [archive.clone(), archive];
    for (name, count) in [("Release", 1), ("Debug", 0)] {
        let config = ConfigId(name.into());
        let libraries = device_libraries(&model, &app, &config, &items, b"CUDA");
        let plan = plan(
            &model,
            &app,
            &config,
            b"CUDA",
            &DeviceLinkInputs {
                objects: &["app.o".into()],
                libraries: &libraries,
                directories: &[],
                link_flags: &[],
                compile_flags: &[],
            },
        )?;
        let DeviceLinkMode::Driver { invocation, .. } = plan.mode else {
            return Err("Expected driver device linking.".into());
        };
        assert_eq!(
            invocation.argv.get(1).map(|arg| arg.as_slice()),
            Some(b"--required".as_slice())
        );
        assert_eq!(
            invocation
                .argv
                .iter()
                .filter(|arg| arg.as_slice() == b"-dlto")
                .count(),
            count
        );
    }
    Ok(())
}

#[test]
fn driver_rule_keeps_multiple_commands_in_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    model.dir_mut(DirId::ROOT).vars.extend([
        ("CMAKE_CUDA_COMPILER".into(), "nvcc".into()),
        ("CMAKE_CUDA_COMPILER_ARG1".into(), "--required".into()),
        ("CMAKE_CUDA_DEVICE_LINK_EXECUTABLE".into(),
         "prepare <TARGET>;<CMAKE_CUDA_COMPILER> <CMAKE_CUDA_COMPILER_ARG1> -dlink -o <TARGET>;finish <TARGET_BASE>".into()),
    ]);
    let app = target("app", TargetKind::Executable);
    let plan = plan(
        &model,
        &app,
        &ConfigId::default(),
        b"CUDA",
        &DeviceLinkInputs {
            objects: &[],
            libraries: &DeviceLibraries::default(),
            directories: &[],
            link_flags: &[],
            compile_flags: &[],
        },
    )?;
    let DeviceLinkMode::Driver { invocation, .. } = plan.mode else {
        return Err("Expected driver device linking.".into());
    };
    assert_eq!(
        invocation.argv,
        [
            "sh",
            "-c",
            "prepare CMakeFiles/app.dir/cmake_device_link.o && nvcc --required -dlink -o CMakeFiles/app.dir/cmake_device_link.o && finish CMakeFiles/app.dir/cmake_device_link",
        ],
    );
    Ok(())
}

fn clang_fixture() -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    for (key, value) in [
        ("CMAKE_CUDA_COMPILER", "clang"),
        ("CMAKE_CUDA_COMPILER_ID", "Clang"),
        ("CMAKE_CUDA_COMPILER_VERSION", "19.1"),
        ("CMAKE_CUDA_ARCHITECTURES_ALL", "52-real;75;52"),
        ("CMAKE_CUDA_ARCHITECTURES_ALL_MAJOR", "52-real;86"),
        ("CMAKE_CUDA_ARCHITECTURES_NATIVE", "90a;90a"),
        ("CMAKE_CUDA_DEVICE_LINKER", "/cuda/nvlink"),
        ("CMAKE_CUDA_FATBINARY", "/cuda/fatbinary"),
        (
            "CMAKE_CUDA_DEVICE_LINK_COMPILE",
            r#"<CMAKE_CUDA_COMPILER> -c link.stub -o <OBJECT> -DFATBINFILE=\"<FATBINARY>\" -DREGISTERLINKBINARYFILE=\"<REGISTER_FILE>\""#,
        ),
    ] {
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(key.into(), value.into());
    }
    model
}

#[test]
fn explicit_plan_emits_one_registration_header() -> Result<(), ResolveError> {
    let model = clang_fixture();
    let mut app = target("cuda app", TargetKind::Executable);
    app.props
        .set("CUDA_ARCHITECTURES".into(), "75-real;90-virtual;75".into());
    app.props
        .set("RULE_LAUNCH_LINK".into(), "rule-wrapper".into());
    app.props
        .set("CUDA_COMPILER_LAUNCHER".into(), "compiler-wrapper".into());
    app.props
        .set("CUDA_LINKER_LAUNCHER".into(), "link-wrapper".into());
    let libraries = DeviceLibraries::default();
    let plan = plan(
        &model,
        &app,
        &ConfigId::default(),
        b"CUDA",
        &DeviceLinkInputs {
            objects: &["a.o".into()],
            libraries: &libraries,
            directories: &[],
            link_flags: &[],
            compile_flags: &[],
        },
    )?;
    assert!(plan.output.ends_with(b"cmake_device_link.o"));
    if let DeviceLinkMode::Explicit {
        cubins,
        fatbinary,
        compile,
        ..
    } = plan.mode
    {
        assert_eq!(
            cubins
                .iter()
                .map(|link| link.arch.clone())
                .collect::<Vec<_>>(),
            ["75", "90"]
        );
        assert_eq!(
            cubins
                .iter()
                .filter(|link| link
                    .invocation
                    .argv
                    .iter()
                    .any(|arg| arg.starts_with(b"--register-link-binaries=")))
                .count(),
            1
        );
        assert!(
            fatbinary
                .argv
                .iter()
                .any(|arg| arg.starts_with(b"-im=profile=sm_90,"))
        );
        assert!(compile.argv.iter().any(|arg| arg == b"link.stub"));
        assert_eq!(
            compile.argv.first().map(|arg| arg.as_slice()),
            Some(b"clang".as_slice())
        );
        assert!(
            compile
                .argv
                .iter()
                .any(|arg| arg == b"-DFATBINFILE=\"CMakeFiles/cuda app.dir/cmake_cuda_fatbin.h\"")
        );
        assert!(compile.argv.iter().any(|arg| arg
            == b"-DREGISTERLINKBINARYFILE=\"CMakeFiles/cuda app.dir/cmake_cuda_register.h\""));
    } else {
        return Err(ResolveError::Message(
            "Expected explicit device linking.".into(),
        ));
    }
    Ok(())
}

#[test]
fn clang_19_device_link_expands_named_architectures() -> Result<(), Box<dyn std::error::Error>> {
    let model = clang_fixture();
    let mut app = target("app", TargetKind::Executable);
    let libraries = DeviceLibraries::default();
    for (value, expected) in [
        ("all", ["-arch=sm_52", "-arch=sm_75"].as_slice()),
        ("all-major", ["-arch=sm_52", "-arch=sm_86"].as_slice()),
        ("native", ["-arch=sm_90a"].as_slice()),
    ] {
        app.props.set("CUDA_ARCHITECTURES".into(), value.into());
        let plan = plan(
            &model,
            &app,
            &ConfigId::default(),
            b"CUDA",
            &DeviceLinkInputs {
                objects: &["app.o".into()],
                libraries: &libraries,
                directories: &[],
                link_flags: &[],
                compile_flags: &[],
            },
        )?;
        let DeviceLinkMode::Explicit { cubins, .. } = plan.mode else {
            return Err("Expected explicit device linking.".into());
        };
        assert_eq!(
            cubins
                .iter()
                .filter_map(|link| link.invocation.argv.get(1).map(|arg| arg.as_slice()))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|arg| arg.as_bytes())
                .collect::<Vec<_>>(),
        );
    }
    Ok(())
}
