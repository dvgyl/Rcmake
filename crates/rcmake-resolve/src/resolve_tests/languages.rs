use super::*;
use rcmake_model::GeneratorKind;

fn fixture(generator: GeneratorKind) -> (ProjectModel, PureHost, String) {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let src = format!("{root}/src");
    let out = format!("{root}/build");
    let host = PureHost::new(Path::new(root).to_path_buf(), Path::new(root).join("cmake"));
    let mut model = project_with_roots(&src, &out);
    model.generator = Some(generator);
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), "Linux".into());
    (model, host, src)
}

fn define(model: &mut ProjectModel, key: &str, value: &str) {
    model.global.props.set(key.into(), value.into());
}

#[test]
fn fortran_uses_typed_scans_and_preserves_source_identity() -> TestResult {
    let (mut model, host, src) = fixture(GeneratorKind::Ninja);
    for (key, value) in [
        ("ENABLED_LANGUAGES", "Fortran"),
        ("CMAKE_Fortran_COMPILER", "gfortran"),
        ("CMAKE_Fortran_COMPILER_ID", "GNU"),
        ("CMAKE_Fortran_SOURCE_FILE_EXTENSIONS", "f90;F90"),
        (
            "CMAKE_Fortran_PREPROCESS_SOURCE",
            "<CMAKE_Fortran_COMPILER> -E <SOURCE> -o <PREPROCESSED_SOURCE>",
        ),
        ("CMAKE_Fortran_MODDIR_FLAG", "-J"),
        ("CMAKE_Fortran_POSTPROCESS_FLAG", "-fpreprocessed"),
    ] {
        define(&mut model, key, value);
    }
    let source = Path::new(&src).join("provider.F90");
    host.seed_file(&source, b"module provider\nend module\n")?;
    let mut provider = target("provider", TargetKind::StaticLibrary, &["provider.F90"]);
    provider
        .props
        .set("Fortran_MODULE_DIRECTORY".into(), "modules".into());
    model.add_target(provider)?;
    let output = resolve(&model, &host, &[])?;
    let graph = output.graphs.first().ok_or("Missing Fortran graph.")?;
    let (ddi, pp) = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Scan {
                lang,
                ddi,
                preprocessed: Some(pp),
                ..
            } if lang == "Fortran" => Some((ddi, pp)),
            _ => None,
        })
        .ok_or("Missing Fortran scan.")?;
    assert!(graph.steps.iter().any(|step| matches!(step, BuildStep::Collate {
        lang, ddis, depend_info, modmap_format: None, ..
    } if lang == "Fortran" && ddis.contains(ddi) && matches!(depend_info.as_ref(), DependInfo::Fortran(_)))));
    assert!(graph.steps.iter().any(|step| matches!(step, BuildStep::Compile {
        source, compile_input: Some(input), invocation, depfile: None, dyndep: Some(_), ..
    } if source.ends_with(b"provider.F90") && input == pp
        && invocation.argv.contains(pp) && invocation.argv.iter().any(|arg| arg.starts_with(b"-J")))));
    model.generator = Some(GeneratorKind::UnixMakefiles);
    let output = resolve(&model, &host, &[])?;
    assert!(
        output
            .targets
            .first()
            .and_then(|target| target.fortran.as_ref())
            .and_then(|info| info.make_depend_info.as_deref())
            .is_some_and(|info| matches!(info, DependInfo::Fortran(_)))
    );
    Ok(())
}

#[test]
fn cuda_device_link_and_host_link_use_separate_options() -> TestResult {
    let (mut model, host, src) = fixture(GeneratorKind::Ninja);
    for (key, value) in [
        ("ENABLED_LANGUAGES", "CUDA;CXX"),
        ("CMAKE_CUDA_COMPILER", "nvcc"),
        ("CMAKE_CUDA_COMPILER_ID", "NVIDIA"),
        ("CMAKE_CUDA_COMPILER_VERSION", "12.6.85"),
        ("CMAKE_CUDA_COMPILER_HAS_DEVICE_LINK_PHASE", "ON"),
        (
            "CMAKE_CXX_LINK_EXECUTABLE",
            "cc <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>",
        ),
        (
            "CMAKE_CUDA_DEVICE_LINK_EXECUTABLE",
            "nvcc -dlink <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>",
        ),
        ("CMAKE_CUDA_RUNTIME_LIBRARY_DEFAULT", "Static"),
        (
            "CMAKE_CUDA_RUNTIME_LIBRARY_LINK_OPTIONS_SHARED",
            "cudadevrt;cudart",
        ),
        ("_CMAKE_CUDA_RDC_FLAG", "-rdc=true"),
    ] {
        define(&mut model, key, value);
    }
    let source = Path::new(&src).join("main.cu");
    host.seed_file(&source, b"int main() { return 0; }\n")?;
    let mut app = target("app", TargetKind::Executable, &["main.cu"]);
    app.policies
        .policies
        .set(PolicyId::new(105), PolicyStatus::New)?;
    for (property, value) in [
        ("LINKER_LANGUAGE", "CXX"),
        ("CUDA_ARCHITECTURES", "90a-real"),
        ("CUDA_SEPARABLE_COMPILATION", "ON"),
        ("CUDA_RUNTIME_LIBRARY", "Shared"),
        ("LINK_DEPENDS", "direct.in"),
    ] {
        app.props.set(property.into(), value.into());
    }
    app.usage.push(
        UsageField::LinkOptions,
        entry("$<HOST_LINK:$<$<LINK_LANGUAGE:CXX>:--host-flag>>"),
    );
    app.usage.push(
        UsageField::LinkOptions,
        entry("$<DEVICE_LINK:$<$<LINK_LANGUAGE:CUDA>:--device-flag>>"),
    );
    let mut iface = target("device_options", TargetKind::InterfaceLibrary, &[]);
    iface.usage.push(
        UsageField::InterfaceLinkOptions,
        entry("$<DEVICE_LINK:$<$<LINK_LANGUAGE:CUDA>:--inherited-device>>"),
    );
    model.add_target(iface)?;
    app.usage.link_libraries = Some(vec![link_item("device_options")]);
    model.add_target(app)?;
    let output = resolve(&model, &host, &[])?;
    let graph = output.graphs.first().ok_or("Missing CUDA graph.")?;
    let mut steps = graph
        .steps
        .iter()
        .filter(|step| matches!(step, BuildStep::DeviceLink { .. } | BuildStep::Link { .. }));
    let Some(BuildStep::DeviceLink {
        output,
        inputs: device_inputs,
        mode: DeviceLinkMode::Driver { invocation, .. },
        ..
    }) = steps.next()
    else {
        return Err("Missing CUDA device linker.".into());
    };
    let Some(BuildStep::Link {
        inputs,
        implicit_inputs,
        step,
        invocations,
        ..
    }) = steps.next()
    else {
        return Err("Missing CUDA host linker.".into());
    };
    let link = invocations.first().ok_or("Missing host link invocation.")?;
    assert_eq!(inputs.iter().filter(|input| *input == output).count(), 1);
    assert_eq!(link.argv.iter().filter(|arg| *arg == output).count(), 1);
    assert!(link.argv.iter().any(|arg| arg == b"-lcudart"));
    assert!(link.argv.iter().any(|arg| arg == b"--host-flag"));
    assert!(invocation.argv.iter().any(|arg| arg == b"--device-flag"));
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg == b"--inherited-device")
    );
    assert_eq!(step.language, "CXX");
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg.ends_with(b"code=[sm_90a]"))
    );
    assert!(
        implicit_inputs
            .iter()
            .any(|input| input.ends_with(b"direct.in"))
    );
    assert!(
        device_inputs
            .iter()
            .any(|input| input.ends_with(b"direct.in"))
    );
    assert!(
        !link
            .argv
            .iter()
            .any(|arg| arg.ends_with(b"direct.in") || arg == b"--device-flag")
    );
    assert!(!invocation.argv.iter().any(|arg| arg == b"--host-flag"));
    Ok(())
}

#[test]
fn source_languages_use_explicit_properties_and_exact_toolchain_extensions() -> TestResult {
    let (mut model, _, src) = fixture(GeneratorKind::Ninja);
    define(&mut model, "ENABLED_LANGUAGES", "C;ASM_NASM;Fortran");
    define(
        &mut model,
        "CMAKE_ASM_NASM_SOURCE_FILE_EXTENSIONS",
        "asm;nasm",
    );
    define(&mut model, "CMAKE_ASM_NASM_IGNORE_EXTENSIONS", "obj;h");
    define(
        &mut model,
        "CMAKE_Fortran_SOURCE_FILE_EXTENSIONS",
        "f03;F03",
    );
    model.languages_mut().register_language(b"C");
    model
        .languages_mut()
        .set_extensions(b"C", [b"xyz"], [b"old"]);
    let app = target("app", TargetKind::ObjectLibrary, &["unit.xyz"]);
    assert_eq!(
        lower::source_language(&model, &app, b"unit.xyz"),
        Some("C".into())
    );
    assert_eq!(
        lower::source_language(&model, &app, b"unit.nasm"),
        Some("ASM_NASM".into())
    );
    assert_eq!(
        lower::source_language(&model, &app, b"unit.F03"),
        Some("Fortran".into())
    );
    assert_eq!(lower::source_language(&model, &app, b"unit.F03x"), None);
    assert_eq!(lower::source_language(&model, &app, b"unit.obj"), None);
    // CXX is not enabled, so a C++ source is not compiled.
    assert_eq!(
        lower::source_language(&model, &app, b"mocs_compilation.cpp"),
        None
    );
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry(format!("{src}/explicit.cpp").into())
        .or_default()
        .set("LANGUAGE".into(), "CXX".into());
    assert_eq!(
        lower::source_language(&model, &app, b"explicit.cpp"),
        Some("CXX".into())
    );
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry(format!("{src}/unit.obj").into())
        .or_default()
        .set("LANGUAGE".into(), "Fortran".into());
    assert_eq!(
        lower::source_language(&model, &app, b"unit.obj"),
        Some("Fortran".into())
    );
    Ok(())
}

#[test]
fn autogen_writes_metadata_for_the_generated_sources() -> TestResult {
    let (mut model, host, src) = fixture(GeneratorKind::Ninja);
    model.dir_mut(DirId::ROOT).vars.extend([
        ("QT_VERSION_MAJOR".into(), "6".into()),
        ("QT_VERSION_MINOR".into(), "8".into()),
    ]);
    let mut app = target("app", TargetKind::Executable, &["main.cpp", "example.h"]);
    app.props.set("AUTOMOC".into(), "ON".into());
    app.props.set("AUTOMOC_EXECUTABLE".into(), "moc".into());
    model.add_target(app)?;
    host.seed_file(&Path::new(&src).join("main.cpp"), b"int main() {}\n")?;
    host.seed_file(
        &Path::new(&src).join("example.h"),
        b"class Example { Q_OBJECT };\n",
    )?;

    let first = resolve(&model, &host, &[])?;
    assert!(
        !first
            .metadata
            .generated_files
            .iter()
            .any(|path| path.ends_with(b"AutogenInfo.json")),
        "AutogenInfo.json must not be registered as a RERUN_CMAKE output"
    );
    let graph = first.graphs.first().ok_or("Missing Qt build graph.")?;
    let info = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Custom(custom) => custom
                .invocations
                .iter()
                .flat_map(|command| &command.argv)
                .find(|arg| arg.ends_with(b"AutogenInfo.json")),
            _ => None,
        })
        .ok_or("Missing Qt metadata path.")?;
    let info_file = rcmake_host::path::to_native(info);
    let read = || host.read(&info_file, rcmake_host::ReadRange::All { limit: 65_536 });
    let data = read()?;
    let text = String::from_utf8(data.clone())?;
    assert!(text.contains("QT_MOC_EXECUTABLE"));
    assert!(text.contains("example.h"));
    assert!(
        graph
            .steps
            .iter()
            .any(|step| matches!(step, BuildStep::Compile { source, .. }
        if source.ends_with(b"mocs_compilation.cpp")))
    );

    let autogen = first
        .targets
        .iter()
        .find(|target| target.metadata.name == "app_autogen")
        .ok_or("Missing AUTOGEN target.")?;
    assert!(autogen.metadata.generator_provided);
    let app = first
        .targets
        .iter()
        .find(|target| target.metadata.name == "app")
        .ok_or("Missing origin target.")?;
    assert!(!app.metadata.generator_provided);
    assert!(
        first
            .target_depends
            .utility_items(app.id)
            .iter()
            .any(|item| item.target == autogen.id)
    );

    resolve(&model, &host, &[])?;
    assert_eq!(read()?, data);
    Ok(())
}

#[test]
fn autogen_and_object_intermediate_directory_strategies_are_independent() -> TestResult {
    for short_autogen in [false, true] {
        let (mut model, host, src) = fixture(GeneratorKind::NinjaMultiConfig);
        host.env_set(b"CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY", Some(b"SHORT"));
        host.env_set(b"CMAKE_INTERMEDIATE_DIR_STRATEGY", Some(b"SHORT"));
        let key = if short_autogen {
            "CMAKE_INTERMEDIATE_DIR_STRATEGY"
        } else {
            "CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY"
        };
        model.cache.insert(
            key.into(),
            CacheEntry {
                value: "FULL".into(),
                ty: CacheType::String,
                help: Default::default(),
                advanced: false,
                props: Default::default(),
            },
        );
        let build = model.dir(DirId::ROOT).binary_dir.clone();
        let sub = model.add_directory(
            DirId::ROOT,
            format!("{src}/sub").into(),
            format!("{build}/sub").into(),
            Backtrace::default(),
        )?;
        model.dir_mut(sub).vars.extend([
            ("QT_VERSION_MAJOR".into(), "6".into()),
            ("QT_VERSION_MINOR".into(), "8".into()),
        ]);
        let mut app = target("app", TargetKind::Executable, &["main.cpp"]);
        app.dir = sub;
        app.props.set("AUTOMOC".into(), "ON".into());
        app.props.set("AUTOMOC_EXECUTABLE".into(), "moc".into());
        model.add_target(app)?;
        host.seed_file(&Path::new(&src).join("sub/main.cpp"), b"int main() {}")?;
        let result = resolve(
            &model,
            &host,
            &[ConfigId("Debug".into()), ConfigId("Release".into())],
        )?;
        assert!(
            !result
                .metadata
                .generated_files
                .iter()
                .any(|path| path.ends_with(b"AutogenInfo.json")),
            "AutogenInfo.json must not be registered as a RERUN_CMAKE output"
        );
        let info = result
            .graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .find_map(|step| match step {
                BuildStep::Custom(custom) => custom
                    .invocations
                    .iter()
                    .flat_map(|command| &command.argv)
                    .find(|arg| arg.ends_with(b"AutogenInfo.json")),
                _ => None,
            })
            .ok_or("Missing autogen information.")?;
        assert_eq!(info.to_string().contains("/.o/"), short_autogen, "{info}");
        for app in result
            .targets
            .iter()
            .filter(|target| target.metadata.name == "app")
        {
            let dir = app
                .artifacts
                .object_dir
                .as_ref()
                .ok_or("Missing object directory.")?;
            assert_eq!(dir.starts_with(b".o/"), !short_autogen, "{dir}");
            assert!(dir.ends_with(&app.config.0), "{dir}");
        }
    }
    let (model, host, _) = fixture(GeneratorKind::Ninja);
    host.env_set(b"CMAKE_AUTOGEN_INTERMEDIATE_DIR_STRATEGY", Some(b""));
    assert!(resolve(&model, &host, &[]).is_err());
    Ok(())
}
