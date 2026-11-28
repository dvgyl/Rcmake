use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{
    BT, CacheEntry, CacheType, CustomCommand, CustomCommandKind, DirId, ImportedVisibility,
    InstallKind, InstallOptions, InstallRule, ProjectModel, Target, TargetEvent, TargetKind,
    TargetOrigin,
};

use super::*;
use crate::{
    BuildStep, CompileGroup, CompileUnit, InstallActionKind, InterfaceInclude, LinkFragment,
    LinkFragmentRole, LinkKind, LinkStep, ResolvedTarget, TargetMetadata,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture(system: &str) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/project".into(), "/build".into());
    var(&mut model, "CMAKE_SYSTEM_NAME", system);
    model
}

fn var(model: &mut ProjectModel, name: &str, value: &str) {
    model.global.props.set(name.into(), value.into());
}

fn bt(value: &str) -> BT<BString> {
    BT::new(value.into(), Default::default())
}

fn raw_target(name: &str, kind: TargetKind) -> Target {
    Target::new(
        name.into(),
        kind,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    )
}

fn resolve_target(
    model: &mut ProjectModel,
    raw: Target,
    config: &ConfigId,
    sources: &[&str],
) -> Result<ResolvedTarget, rcmake_model::ModelError> {
    let dir = model.dir(raw.dir);
    let mut compile_groups: Vec<CompileGroup> = Vec::new();
    let mut compile_units = Vec::new();
    for source in sources {
        let Some(language) = source_language(model, &raw, source.as_bytes()) else {
            continue;
        };
        let group = compile_groups
            .iter()
            .position(|group| group.language == language)
            .unwrap_or_else(|| {
                let index = compile_groups.len();
                compile_groups.push(CompileGroup {
                    language,
                    ..CompileGroup::default()
                });
                index
            });
        compile_units.push(CompileUnit {
            source: bt(source),
            object: object_path(model, &raw, config, source.as_bytes()),
            group,
            scan: false,
        });
    }
    let metadata = TargetMetadata {
        name: raw.name.clone(),
        kind: raw.kind,
        source_dir: dir.source_dir.clone(),
        binary_dir: dir.binary_dir.clone(),
        backtrace: raw.backtrace.clone(),
        folder: None,
        exclude_from_all: false,
        generator_provided: false,
    };
    let artifacts = compute_artifacts(model, &raw, config);
    Ok(ResolvedTarget {
        fortran: None,
        id: model.add_target(raw)?,
        config: config.clone(),
        metadata,
        artifacts,
        compile_groups,
        compile_units,
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
        ide: None,
    })
}

#[test]
fn expands_known_placeholders_only() {
    let out = expand_rule(
        b"<CMAKE_C_COMPILER> <FLAGS> -o <OBJECT> -c <SOURCE> <x> <",
        |n| match n {
            b"FLAGS" => Some("-O2".into()),
            b"OBJECT" => Some("a.o".into()),
            b"SOURCE" => Some("a.c".into()),
            _ => None,
        },
    );
    assert_eq!(out, "<CMAKE_C_COMPILER> -O2 -o a.o -c a.c <x> <");
}

#[test]
fn windows_ninja_links_resources_and_import_libraries() -> TestResult {
    use rcmake_host::{Host, PureHost, ReadRange, path::to_native};
    use rcmake_model::{GeneratorKind, LinkItem, UsageField};

    let mut model = fixture("Windows");
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("C:/Source Tree".into(), "C:/Build Tree".into());
    for (key, value) in [
        ("CMAKE_HOST_SYSTEM_NAME", "Windows"),
        ("CMAKE_C_COMPILER_ID", "MSVC"),
        ("CMAKE_C_COMPILER", "C:/Program Files/Tools/cl.exe"),
        ("CMAKE_C_OUTPUT_EXTENSION", ".obj"),
        (
            "CMAKE_C_COMPILE_OBJECT",
            "<CMAKE_C_COMPILER> <FLAGS> /Fo<OBJECT> /Fd<TARGET_COMPILE_PDB> /FS -c <SOURCE>",
        ),
        ("CMAKE_DEPFILE_FLAGS_C", "/showIncludes"),
        ("CMAKE_RC_COMPILER", "C:/Program Files/SDK/rc.exe"),
        ("CMAKE_RC_COMPILER_ARG1", "/nologo"),
        (
            "CMAKE_RC_COMPILE_OBJECT",
            "<CMAKE_RC_COMPILER> <DEFINES> <INCLUDES> <FLAGS> /fo <OBJECT> <SOURCE>",
        ),
        ("CMAKE_RC_OUTPUT_EXTENSION", ".res"),
        ("CMAKE_MT", "C:/Program Files/SDK/mt.exe"),
        ("CMAKE_LINKER", "C:/Program Files/Tools/link.exe"),
        ("CMAKE_AR", "C:/Program Files/Tools/lib.exe"),
        (
            "CMAKE_C_CREATE_STATIC_LIBRARY",
            "<CMAKE_AR> /out:<TARGET> <OBJECTS>",
        ),
        ("CMAKE_STATIC_LIBRARY_PREFIX", ""),
        ("CMAKE_STATIC_LIBRARY_SUFFIX", ".lib"),
        ("CMAKE_SHARED_LIBRARY_CREATE_C_FLAGS", ""),
        (
            "CMAKE_C_CREATE_SHARED_LIBRARY",
            "<CMAKE_COMMAND> -E vs_link_dll --intdir=<OBJECT_DIR> --rc=<CMAKE_RC_COMPILER> --mt=<CMAKE_MT> --manifests <MANIFESTS> -- <CMAKE_LINKER> /dll /out:<TARGET> /implib:<TARGET_IMPLIB> /pdb:<TARGET_PDB> /version:<TARGET_VERSION_MAJOR>.<TARGET_VERSION_MINOR> /stub:<TARGET_BASE> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES>",
        ),
        (
            "CMAKE_C_LINK_EXECUTABLE",
            "<CMAKE_LINKER> /out:<TARGET> /implib:<TARGET_IMPLIB> /pdb:<TARGET_PDB> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES>",
        ),
        ("CMAKE_C_LINK_DEF_FILE_FLAG", "/DEF:"),
        ("CMAKE_C_STANDARD_LIBRARIES", "kernel32.lib user32.lib"),
    ] {
        var(&mut model, key, value);
    }
    let host = PureHost::default();
    for file in [
        "a.c",
        "app.c",
        "b.c",
        "archive.c",
        "icon.RC",
        "exports file.def",
        "extra.def",
        "main.manifest",
    ] {
        let path = format!("C:/Source Tree/{file}");
        host.seed_file(&to_native(path.as_bytes()), b"")?;
    }
    let mut library = raw_target("shared", TargetKind::SharedLibrary);
    for (key, value) in [
        ("RUNTIME_OUTPUT_DIRECTORY", "run files"),
        ("ARCHIVE_OUTPUT_DIRECTORY", "link files"),
        ("PDB_OUTPUT_DIRECTORY", "link symbols"),
        ("PDB_NAME", "linked"),
        ("COMPILE_PDB_NAME", "compiled"),
        ("COMPILE_PDB_OUTPUT_DIRECTORY_DEBUG", "compile symbols"),
        ("VERSION", "2.7.1"),
        ("SOVERSION", "2"),
    ] {
        library.props.set(key.into(), value.into());
    }
    library.sources = ["a.c", "icon.RC", "exports file.def", "main.manifest"]
        .into_iter()
        .map(bt)
        .collect();
    let library = model.add_target(library)?;
    let mut exported = raw_target("exported", TargetKind::SharedLibrary);
    exported
        .props
        .set("WINDOWS_EXPORT_ALL_SYMBOLS".into(), "ON".into());
    exported.sources = ["b.c", "exports file.def", "extra.def"]
        .into_iter()
        .map(bt)
        .collect();
    model.add_target(exported)?;
    let mut archive = raw_target("archive", TargetKind::StaticLibrary);
    archive.sources.push(bt("archive.c"));
    model.add_target(archive)?;
    let mut imported = raw_target("sdk", TargetKind::SharedLibrary);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    imported.props.set(
        "IMPORTED_LOCATION_DEBUG".into(),
        "C:/SDK/bin/sdk.dll".into(),
    );
    imported.props.set(
        "IMPORTED_IMPLIB_DEBUG".into(),
        "C:/SDK/link files/sdk.lib".into(),
    );
    model.add_target(imported)?;
    let mut app = raw_target("app", TargetKind::Executable);
    app.sources.push(bt("app.c"));
    app.usage.link_libraries = Some(
        ["shared", "sdk", "bare", "ready.LIB"]
            .into_iter()
            .map(|name| LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: None,
                text: bt(name),
                lookup_dir: DirId::ROOT,
            })
            .collect(),
    );
    app.usage
        .push(UsageField::LinkDirectories, bt("C:/Dependency Path"));
    model.add_target(app)?;
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: InstallKind::Targets {
            targets: vec![library],
            export: None,
        },
        options: InstallOptions::default(),
    });
    for (index, value) in [
        "TARGETS",
        "shared",
        "RUNTIME",
        "DESTINATION",
        "bin",
        "COMPONENT",
        "Runtime",
        "ARCHIVE",
        "DESTINATION",
        "lib",
        "COMPONENT",
        "Development",
    ]
    .into_iter()
    .enumerate()
    {
        var(&mut model, &format!("_RCMAKE_INSTALL_0_ARG{index}"), value);
    }
    let result = crate::resolve(&model, &host, &[ConfigId("Debug".into())])?;
    let shared = result
        .targets
        .iter()
        .find(|target| target.id == library)
        .ok_or("Missing DLL facts.")?;
    assert_eq!(
        shared.artifacts.file.as_ref().map(|value| value.as_slice()),
        Some(b"C:/Build Tree/run files/shared.dll".as_slice())
    );
    assert_eq!(
        shared
            .artifacts
            .import_file
            .as_ref()
            .map(|value| value.as_slice()),
        Some(b"C:/Build Tree/link files/shared.lib".as_slice())
    );
    assert!(shared.artifacts.soname_file.is_none());
    let graph = result.graphs.first().ok_or("Missing build graph.")?;
    let compile = |name: &[u8]| {
        graph.steps.iter().find_map(|step| match step {
            BuildStep::Compile {
                source,
                object,
                invocation,
                ..
            } if source.ends_with(name) => Some((object, invocation)),
            _ => None,
        })
    };
    let (_, invocation) = compile(b"/a.c").ok_or("Missing DLL compiler command.")?;
    assert_eq!(
        invocation.argv.first().map(|value| value.as_slice()),
        Some(b"C:/Program Files/Tools/cl.exe".as_slice())
    );
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg == b"/Fdcompile symbols/compiled.pdb")
    );
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg == b"C:/Source Tree/a.c")
    );
    let (_, invocation) = compile(b"/app.c").ok_or("Missing executable compiler command.")?;
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg == b"/FdCMakeFiles/app.dir/")
    );
    let (_, invocation) =
        compile(b"/archive.c").ok_or("Missing static library compiler command.")?;
    assert!(
        invocation
            .argv
            .iter()
            .any(|arg| arg == b"/FdCMakeFiles/archive.dir/archive.pdb")
    );
    let (resource, invocation) =
        compile(b"/icon.RC").ok_or("Missing resource compiler command.")?;
    assert!(resource.ends_with(b".res"));
    assert!(
        invocation
            .argv
            .starts_with(&["C:/Program Files/SDK/rc.exe".into(), "/nologo".into()])
    );
    assert!(
        invocation
            .argv
            .windows(2)
            .any(|pair| matches!(pair, [flag, output] if flag == b"/fo" && output == resource))
    );
    let link = |kind| {
        graph.steps.iter().find_map(|step| match step {
            BuildStep::Link {
                inputs,
                implicit_inputs,
                step,
                invocations,
                ..
            } if step.kind == kind => Some((inputs, implicit_inputs, invocations)),
            _ => None,
        })
    };
    let (inputs, implicit_inputs, dll) =
        link(LinkKind::SharedLibrary).ok_or("Missing DLL link command.")?;
    assert!(inputs.contains(resource));
    assert!(
        implicit_inputs
            .iter()
            .any(|arg| arg == b"C:/Source Tree/exports file.def")
    );
    let [dll] = dll.as_slice() else {
        return Err("Expected one DLL link command.".into());
    };
    for argument in [
        "--intdir=CMakeFiles/shared.dir",
        "--rc=C:/Program Files/SDK/rc.exe",
        "--mt=C:/Program Files/SDK/mt.exe",
        "C:/Source Tree/main.manifest",
        "C:/Program Files/Tools/link.exe",
        "/DEF:C:/Source Tree/exports file.def",
        "/implib:C:/Build Tree/link files/shared.lib",
        "/pdb:C:/Build Tree/link symbols/linked.pdb",
        "/version:2.7",
        "/stub:run files/shared",
    ] {
        assert!(
            dll.argv.iter().any(|arg| arg == argument),
            "{argument}: {:?}",
            dll.argv
        );
    }
    let (inputs, _, app) = link(LinkKind::Executable).ok_or("Missing executable link command.")?;
    assert!(inputs.iter().any(|arg| arg == b"link files/shared.lib"));
    assert!(inputs.iter().any(|arg| arg == b"C:/SDK/link files/sdk.lib"));
    assert!(!inputs.iter().any(|arg| arg == b"C:/SDK/bin/sdk.dll"));
    let app = app.first().ok_or("Missing linker invocation.")?;
    assert!(
        app.argv
            .iter()
            .any(|arg| arg == b"-LIBPATH:C:/Dependency Path")
    );
    assert!(app.argv.ends_with(&[
        "bare.lib".into(),
        "ready.LIB".into(),
        "kernel32.lib".into(),
        "user32.lib".into()
    ]));
    assert!(
        !app.argv
            .iter()
            .any(|arg| arg.starts_with(b"-l") || arg.starts_with(b"-L/") || arg == b"/implib:")
    );
    let (_, _, archive) = link(LinkKind::StaticArchive).ok_or("Missing librarian command.")?;
    let [archive] = archive.as_slice() else {
        return Err("Expected one librarian command.".into());
    };
    assert_eq!(
        archive.argv.first().map(|value| value.as_slice()),
        Some(b"C:/Program Files/Tools/lib.exe".as_slice())
    );
    assert!(result.install.actions.iter().any(|action| action.component == "Runtime" && action.destination == "bin"
        && matches!(&action.kind, InstallActionKind::Files {files, ..} if files.as_slice() == ["C:/Build Tree/run files/shared.dll"])));
    assert!(result.install.actions.iter().any(|action| action.component == "Development" && action.destination == "lib"
        && matches!(&action.kind, InstallActionKind::Files {files, ..} if files.as_slice() == ["C:/Build Tree/link files/shared.lib"])));
    let list = host.read(
        &to_native(b"C:/Build Tree/CMakeFiles/exported.dir/exports.def.objs"),
        ReadRange::All { limit: 4096 },
    )?;
    assert_eq!(list, b"CMakeFiles/exported.dir/b.c.obj\nC:/Source Tree/exports file.def\nC:/Source Tree/extra.def");
    assert!(graph.steps.iter().any(|step| match step {
        BuildStep::Link {
            step, invocations, ..
        } if step.output == "exported.dll" => {
            matches!(invocations.as_slice(), [generate, link]
                if generate.argv.iter().any(|arg| arg == b"__create_def")
                  && link.argv.iter().any(|arg| arg == b"/DEF:CMakeFiles/exported.dir/exports.def"))
        }
        _ => false,
    }));
    Ok(())
}

#[test]
fn fortran_and_swift_compile_artifacts_omit_compiler_depfiles() -> TestResult {
    for language in ["Fortran", "Swift"] {
        let mut model = fixture("Linux");
        let config = ConfigId::default();
        var(
            &mut model,
            &format!("CMAKE_DEPFILE_FLAGS_{language}"),
            "-MMD -MF <DEP_FILE>",
        );
        let raw = raw_target("source", TargetKind::ObjectLibrary);
        let mut target = resolve_target(&mut model, raw, &config, &["source.f90"])?;
        let group = target
            .compile_groups
            .first_mut()
            .ok_or("Missing compile group.")?;
        group.language = language.into();
        let graph = lower(&model, &[target], &config)?;
        let Some(BuildStep::Compile {
            depfile,
            invocation,
            ..
        }) = graph.steps.first()
        else {
            return Err("Missing compile step.".into());
        };
        assert!(depfile.is_none());
        assert!(!invocation.argv.iter().any(|arg| arg == b"-MMD"));
    }
    Ok(())
}

#[test]
fn executable_rules_preserve_paths_and_library_inputs() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId("Debug".into());
    var(&mut model, "CMAKE_C_COMPILER", "/tool dir/cc");
    var(
        &mut model,
        "CMAKE_C_COMPILE_OBJECT",
        "<CMAKE_C_COMPILER> <FLAGS> <DEFINES> <INCLUDES> -MD -MF <DEP_FILE> -c <SOURCE> -o <OBJECT>",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_EXECUTABLE",
        "<CMAKE_C_COMPILER> <FLAGS> <OBJECTS> <LINK_FLAGS> <LINK_LIBRARIES> -o <TARGET>",
    );
    var(&mut model, "CMAKE_EXE_LINKER_FLAGS_DEBUG", "-g");
    let mut main = raw_target("main", TargetKind::Executable);
    main.props.set("OUTPUT_NAME_DEBUG".into(), "my app".into());
    main.props
        .set("RUNTIME_OUTPUT_DIRECTORY_DEBUG".into(), "my bin".into());
    main.props.set(
        "LINK_DEPENDS".into(),
        "custom.ld;custom.ld;main file.c".into(),
    );
    let mut target = resolve_target(&mut model, main, &config, &["main file.c"])?;
    assert_eq!(target.artifacts.file, Some("/build/my bin/my app".into()));
    target.link = Some(LinkStep {
        kind: LinkKind::Executable,
        language: "C".into(),
        fragments: vec![
            LinkFragment {
                role: LinkFragmentRole::LibraryPath,
                text: bt("-L'/other libs'"),
            },
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'/other libs/libx.a' -lm"),
            },
        ],
        output: BString::default(),
    });
    let graph = lower(&model, &[target], &config)?;
    let [
        BuildStep::Compile {
            invocation,
            depfile,
            ..
        },
        BuildStep::Link {
            inputs,
            implicit_inputs,
            step,
            invocations,
        },
    ] = graph.steps.as_slice()
    else {
        return Err("Expected a compile step and a link step.".into());
    };
    assert_eq!(
        invocation.argv,
        [
            "/tool dir/cc",
            "-MD",
            "-MF",
            "CMakeFiles/main.dir/main_file.c.o.d",
            "-c",
            "/project/main file.c",
            "-o",
            "CMakeFiles/main.dir/main_file.c.o"
        ]
    );
    assert_eq!(*depfile, Some("CMakeFiles/main.dir/main_file.c.o.d".into()));
    assert_eq!(step.output, "my bin/my app");
    assert_eq!(
        *inputs,
        ["CMakeFiles/main.dir/main_file.c.o", "/other libs/libx.a"]
    );
    assert_eq!(
        *implicit_inputs,
        ["/project/custom.ld", "/project/main file.c"]
    );
    let [linker] = invocations.as_slice() else {
        return Err("Expected one linker invocation.".into());
    };
    assert_eq!(
        linker.argv,
        [
            "/tool dir/cc",
            "CMakeFiles/main.dir/main_file.c.o",
            "-g",
            "-L/other libs",
            "/other libs/libx.a",
            "-lm",
            "-o",
            "my bin/my app"
        ]
    );
    Ok(())
}

#[test]
fn archive_create_finish_and_cleanup_use_configured_tools() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    var(&mut model, "CMAKE_AR", "/tools dir/ar");
    var(&mut model, "CMAKE_RANLIB", "custom-ranlib");
    var(
        &mut model,
        "CMAKE_C_ARCHIVE_CREATE",
        "<CMAKE_AR> qc <TARGET> <OBJECTS>",
    );
    var(
        &mut model,
        "CMAKE_C_ARCHIVE_FINISH",
        "<CMAKE_RANLIB> -D <TARGET>",
    );
    let mut raw = raw_target("stuff", TargetKind::StaticLibrary);
    raw.props
        .set("ARCHIVE_OUTPUT_DIRECTORY".into(), "lib".into());
    raw.props.set("SUFFIX".into(), ".archive".into());
    let target = resolve_target(&mut model, raw, &config, &["a.c", "b.c"])?;
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected a link step.".into());
    };
    let [cleanup, create, finish] = invocations.as_slice() else {
        return Err("Expected three archive invocations.".into());
    };
    assert_eq!(
        cleanup.argv,
        ["cmake", "-E", "rm", "-f", "lib/libstuff.archive"]
    );
    assert_eq!(
        create.argv,
        [
            "/tools dir/ar",
            "qc",
            "lib/libstuff.archive",
            "CMakeFiles/stuff.dir/a.c.o",
            "CMakeFiles/stuff.dir/b.c.o"
        ]
    );
    assert_eq!(finish.argv, ["custom-ranlib", "-D", "lib/libstuff.archive"]);
    Ok(())
}

#[test]
fn darwin_shared_library_uses_install_name_versions_and_symlinks() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Debug".into());
    var(
        &mut model,
        "CMAKE_C_CREATE_SHARED_LIBRARY",
        "<CMAKE_C_COMPILER> <LANGUAGE_COMPILE_FLAGS> <LINK_FLAGS> -o <TARGET> <SONAME_FLAG> <TARGET_INSTALLNAME_DIR><TARGET_SONAME> <OBJECTS> <LINK_LIBRARIES>",
    );
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    for (key, value) in [
        ("VERSION", "1.2.3"),
        ("SOVERSION", "1"),
        ("OUTPUT_NAME_DEBUG", "thing"),
        ("DEBUG_POSTFIX", "_dbg"),
        ("BUILD_RPATH", "/sample path;/another"),
    ] {
        raw.props.set(key.into(), value.into());
    }
    let darwin = compute_artifacts(&model, &raw, &config);
    assert_eq!(darwin.file, Some("/build/libthing_dbg.1.2.3.dylib".into()));
    assert_eq!(
        darwin.soname_file,
        Some("/build/libthing_dbg.1.dylib".into())
    );
    assert_eq!(darwin.linker_file, Some("/build/libthing_dbg.dylib".into()));
    let linux = compute_artifacts(&fixture("Linux"), &raw, &config);
    assert_eq!(linux.file, Some("/build/libthing_dbg.so.1.2.3".into()));
    assert_eq!(linux.soname_file, Some("/build/libthing_dbg.so.1".into()));
    assert_eq!(linux.linker_file, Some("/build/libthing_dbg.so".into()));
    let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected a shared library link step.".into());
    };
    let [linker, symlinks] = invocations.as_slice() else {
        return Err("Expected the linker and the symlink command.".into());
    };
    assert_eq!(
        linker.argv,
        [
            "cc",
            "-dynamiclib",
            "-compatibility_version",
            "1.0.0",
            "-current_version",
            "1.2.3",
            "-o",
            "libthing_dbg.1.2.3.dylib",
            "-install_name",
            "@rpath/libthing_dbg.1.dylib",
            "CMakeFiles/foo.dir/a.c.o",
            "-Wl,-rpath,/sample path",
            "-Wl,-rpath,/another",
        ]
    );
    assert_eq!(
        symlinks.argv,
        [
            "cmake",
            "-E",
            "cmake_symlink_library",
            "libthing_dbg.1.2.3.dylib",
            "libthing_dbg.1.dylib",
            "libthing_dbg.dylib"
        ]
    );
    Ok(())
}

#[test]
fn executable_uses_runtime_search_paths_for_shared_libraries_in_the_build_tree() -> TestResult {
    let mut model = fixture("Darwin");
    var(&mut model, "CMAKE_C_IMPLICIT_LINK_DIRECTORIES", "/usr/lib");
    let config = ConfigId::default();
    let mut app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["app.c"],
    )?;
    app.link = Some(LinkStep {
        kind: LinkKind::Executable,
        language: "C".into(),
        fragments: vec![
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'lib dir/libexample.dylib'"),
            },
            LinkFragment {
                role: LinkFragmentRole::Libraries,
                text: bt("'/usr/lib/libsystem.dylib'"),
            },
        ],
        output: "app".into(),
    });
    let graph = lower(&model, &[app], &config)?;
    let Some(BuildStep::Link { invocations, .. }) = graph.steps.last() else {
        return Err("Expected the executable link step.".into());
    };
    let [linker] = invocations.as_slice() else {
        return Err("Expected one linker invocation.".into());
    };
    assert!(linker.argv.ends_with(&[
        "-Wl,-rpath,/build/lib dir".into(),
        "lib dir/libexample.dylib".into(),
        "/usr/lib/libsystem.dylib".into(),
    ]));
    Ok(())
}

#[test]
fn compile_flag_order_matches_the_lowering_contract() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Debug".into());
    for (key, value) in [
        ("CMAKE_CXX_FLAGS", "-Wall -Wextra"),
        ("CMAKE_CXX_FLAGS_DEBUG", "-g"),
        ("CMAKE_OSX_ARCHITECTURES", "arm64;x86_64"),
        ("CMAKE_OSX_SYSROOT", "/SDK Dir"),
        ("CMAKE_OSX_DEPLOYMENT_TARGET", "12.0"),
        ("CMAKE_CXX_COMPILE_OPTIONS_VISIBILITY", "-fvisibility="),
        (
            "CMAKE_CXX_COMPILE_OPTIONS_VISIBILITY_INLINES_HIDDEN",
            "-fvisibility-inlines-hidden",
        ),
    ] {
        var(&mut model, key, value);
    }
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    for (key, value) in [
        ("CXX_STANDARD", "17"),
        ("CXX_EXTENSIONS", "OFF"),
        ("CXX_VISIBILITY_PRESET", "hidden"),
        ("VISIBILITY_INLINES_HIDDEN", "ON"),
    ] {
        raw.props.set(key.into(), value.into());
    }
    let mut target = resolve_target(&mut model, raw, &config, &["a.cpp", "b.c"])?;
    let Some(group) = target.compile_groups.first_mut() else {
        return Err("Expected a C++ compile group.".into());
    };
    group.flags.push(bt("-Werror"));
    group.defines.extend([
        bt("lowercase"),
        bt("NAME=Some 'text'"),
        bt("NAME=Some 'text'"),
        bt("FIRST"),
    ]);
    group.includes.extend([
        InterfaceInclude {
            path: bt("/ordinary include///"),
            system: false,
        },
        InterfaceInclude {
            path: bt("/"),
            system: false,
        },
        InterfaceInclude {
            path: bt("/system include/"),
            system: true,
        },
        InterfaceInclude {
            path: bt("/system dot/./"),
            system: true,
        },
        InterfaceInclude {
            path: bt("/Framework Path/Foo.framework/Headers"),
            system: false,
        },
    ]);
    let graph = lower(&model, &[target], &config)?;
    let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
        return Err("Expected a compile step.".into());
    };
    assert!(invocation.argv.windows(15).any(|flags| flags
        == [
            "-Wall",
            "-Wextra",
            "-g",
            "-std=c++17",
            "-arch",
            "arm64",
            "-arch",
            "x86_64",
            "-isysroot",
            "/SDK Dir",
            "-mmacosx-version-min=12.0",
            "-fPIC",
            "-fvisibility=hidden",
            "-fvisibility-inlines-hidden",
            "-Werror",
        ]));
    assert!(invocation.argv.windows(10).any(|args| args
        == [
            "-DFIRST",
            "-DNAME=Some 'text'",
            "-Dlowercase",
            "-I/ordinary include",
            "-I/",
            "-isystem",
            "/system include",
            "-isystem",
            "/system dot/.",
            "-I/Framework Path/Foo.framework/Headers"
        ]));
    assert!(
        matches!(graph.steps.last(), Some(BuildStep::Link { step, .. }) if step.language == "CXX")
    );
    Ok(())
}

#[test]
fn compile_architecture_defaults_follow_darwin_initialization() -> TestResult {
    let config = ConfigId("Debug".into());
    for (system, default, explicit, configured, compiler_target, expected) in [
        ("Darwin", "", "", None, "", None),
        ("Darwin", "arm64", "", None, "", Some("arm64")),
        ("Darwin", "arm64", "x86_64", None, "", Some("x86_64")),
        ("Darwin", "arm64", "x86_64", Some(""), "", Some("arm64")),
        ("Darwin", "arm64", "", Some("x86_64"), "", Some("x86_64")),
        ("Darwin", "arm64", "", None, "x86_64-apple-darwin", None),
        ("Linux", "arm64", "", None, "", None),
    ] {
        let mut model = fixture(system);
        var(&mut model, "_CMAKE_APPLE_ARCHS_DEFAULT", default);
        var(&mut model, "CMAKE_OSX_ARCHITECTURES", explicit);
        var(&mut model, "CMAKE_C_COMPILER_TARGET", compiler_target);
        let mut raw = raw_target("foo", TargetKind::ObjectLibrary);
        if let Some(arch) = configured {
            raw.props.set("OSX_ARCHITECTURES_DEBUG".into(), arch.into());
        }
        let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
        let graph = lower(&model, &[target], &config)?;
        let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
            return Err("Expected a compile step.".into());
        };
        let architecture = invocation.argv.windows(2).find_map(|args| match args {
            [flag, value] if flag == "-arch" => Some(value.as_slice()),
            _ => None,
        });
        assert_eq!(architecture, expected.map(str::as_bytes));
    }
    Ok(())
}

#[test]
fn object_paths_use_the_project_source_and_build_trees() -> TestResult {
    let mut model = fixture("Linux");
    let mut raw = raw_target("scan-test", TargetKind::ObjectLibrary);
    raw.dir = model.add_directory(
        DirId::ROOT,
        "/project/test/detail".into(),
        "/build/test/detail".into(),
        Default::default(),
    )?;
    let config = ConfigId::default();
    for (source, object) in [
        ("scan.cc", "scan.cc.o"),
        ("../src/os.cc", "__/src/os.cc.o"),
        ("../../src/os.cc", "__/__/src/os.cc.o"),
        ("/build/test/generated.c", "__/generated.c.o"),
        ("/build/test/detail/generated.c", "generated.c.o"),
        ("/build-other/generated.c", "build-other/generated.c.o"),
        ("../../../external/os.cc", "external/os.cc.o"),
    ] {
        let expected = format!("test/detail/CMakeFiles/scan-test.dir/{object}");
        assert_eq!(
            object_path(&model, &raw, &config, source.as_bytes()),
            expected.as_str()
        );
    }
    Ok(())
}

#[test]
fn file_api_object_path_keeps_the_ninja_intermediate_dir() -> TestResult {
    use rcmake_model::GeneratorKind;
    let mut model = fixture("Linux");
    let mut raw = raw_target("ob", TargetKind::ObjectLibrary);
    raw.dir = model.add_directory(
        DirId::ROOT,
        "/project/sub".into(),
        "/build/sub".into(),
        Default::default(),
    )?;
    let config = ConfigId("Debug".into());
    let object = object_path(&model, &raw, &config, b"c.c");
    assert_eq!(object, "sub/CMakeFiles/ob.dir/c.c.o");
    for (generator, expected) in [
        (None, "sub/CMakeFiles/ob.dir/c.c.o"),
        (Some(GeneratorKind::Ninja), "sub/CMakeFiles/ob.dir/./c.c.o"),
    ] {
        model.generator = generator;
        assert_eq!(
            file_api_object_path(&model, &raw, &config, &object),
            expected
        );
    }
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    let object = object_path(&model, &raw, &config, b"c.c");
    assert_eq!(object, "sub/CMakeFiles/ob.dir/Debug/c.c.o");
    assert_eq!(file_api_object_path(&model, &raw, &config, &object), object);
    Ok(())
}

#[test]
fn classifies_sources_and_keeps_external_object_paths_distinct() {
    let mut model = fixture("Linux");
    let raw = raw_target("main", TargetKind::ObjectLibrary);
    let config = ConfigId::default();
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/a.txt".into())
        .or_default()
        .set("LANGUAGE".into(), "OBJC".into());
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/unused.cpp".into())
        .or_default()
        .set("HEADER_FILE_ONLY".into(), "ON".into());
    for (path, language) in [
        ("x.C", "CXX"),
        ("x.mm", "OBJCXX"),
        ("x.S", "ASM"),
        ("a.txt", "OBJC"),
    ] {
        assert_eq!(
            source_language(&model, &raw, path.as_bytes()),
            Some(language.into())
        );
    }
    for path in [
        b"unused.cpp".as_slice(),
        b"header.h",
        b"header.tcc",
        b"binary.o",
    ] {
        assert_eq!(source_language(&model, &raw, path), None);
    }
    assert_eq!(
        object_path(&model, &raw, &config, b"d/main.c"),
        "CMakeFiles/main.dir/d/main.c.o"
    );
    let out = object_path(&model, &raw, &config, b"../ext/main.c");
    assert_eq!(out, "CMakeFiles/main.dir/ext/main.c.o");
    assert_ne!(out, object_path(&model, &raw, &config, b"/ext2/main.c"));
    assert_ne!(
        out,
        object_path(&model, &raw, &config, b"__/external/root-/ext/main.c")
    );
    var(&mut model, "CMAKE_C_LINKER_PREFERENCE", "40");
    assert_eq!(
        linker_language(&model, &raw, [b"CXX".as_slice(), b"C"]),
        Some("C".into())
    );
}

#[test]
fn imported_artifacts_follow_configuration_mapping() {
    let mut raw = raw_target("imported", TargetKind::SharedLibrary);
    raw.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    raw.props.set(
        "MAP_IMPORTED_CONFIG_DEBUG".into(),
        "RelWithDebInfo;Release".into(),
    );
    raw.props.set(
        "IMPORTED_LOCATION_RELEASE".into(),
        "/opt/lib/libfoo.so".into(),
    );
    raw.props
        .set("IMPORTED_LOCATION".into(), "/default/other.so".into());
    let artifacts = compute_artifacts(&fixture("Linux"), &raw, &ConfigId("debug".into()));
    assert_eq!(artifacts.file, Some("/opt/lib/libfoo.so".into()));
    assert_eq!(artifacts.object_dir, None);
}

#[test]
fn custom_commands_attach_to_consumers_in_dependency_order() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    let program = resolve_target(
        &mut model,
        raw_target("generator", TargetKind::Executable),
        &config,
        &["generator.c"],
    )?;
    let app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["/build/generated.c"],
    )?;
    let util = resolve_target(
        &mut model,
        raw_target("assets", TargetKind::Utility),
        &config,
        &[],
    )?;
    let mut generated = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    generated.outputs.push(bt("generated.c"));
    generated.depends.push(bt("input.txt"));
    generated.command_lines.push(vec![
        "generator".into(),
        "it's literal $HOME".into(),
        "".into(),
    ]);
    generated.working_directory = Some("stage".into());
    generated.byproducts.push(bt("extra.h"));
    generated.depfile = Some("gen.d".into());
    generated.uses_terminal = true;
    model.add_command(generated)?;
    let mut input = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    input.outputs.push(bt("input.txt"));
    model.add_command(input)?;
    for (event, label) in [
        (TargetEvent::PreBuild, "pre-build"),
        (TargetEvent::PreLink, "pre-link"),
        (TargetEvent::PostBuild, "post-build"),
    ] {
        let mut command = CustomCommand::new(
            DirId::ROOT,
            CustomCommandKind::TargetEvent {
                target: app.id,
                event,
            },
            Default::default(),
            Default::default(),
        );
        command
            .command_lines
            .push(vec!["echo".into(), label.into()]);
        model.add_command(command)?;
    }
    let mut command = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(util.id),
        Default::default(),
        Default::default(),
    );
    command.depends.push(bt("extra.h"));
    model.add_command(command)?;
    let graph = lower(&model, &[app, util, program], &config)?;
    let [_, app, utility] = graph.targets.as_slice() else {
        return Err("Expected three targets in model order.".into());
    };
    assert_eq!(app.name, "app");
    assert_eq!(app.depends, [0]);
    assert_eq!(utility.depends, [0]);
    let steps: Vec<_> = app
        .steps
        .iter()
        .filter_map(|&index| graph.steps.get(index))
        .collect();
    let [
        BuildStep::Custom(input),
        BuildStep::Custom(generated),
        BuildStep::Compile { .. },
        BuildStep::Link { invocations, .. },
    ] = steps.as_slice()
    else {
        return Err("Expected custom commands, compile, and link in dependency order.".into());
    };
    assert_eq!(input.outputs, ["input.txt"]);
    assert_eq!(generated.byproducts, ["extra.h"]);
    assert_eq!(generated.depfile, Some("gen.d".into()));
    assert!(generated.uses_terminal);
    let Some(invocation) = generated.invocations.first() else {
        return Err("Expected the generator invocation.".into());
    };
    assert_eq!(
        invocation.argv,
        ["/build/generator", "it's literal $HOME", ""]
    );
    assert_eq!(invocation.working_dir, Some("/build/stage".into()));
    assert_eq!(
        invocation.shell,
        r"/build/generator 'it'\''s literal $HOME' ''"
    );
    assert_eq!(
        super::shell::simple_args(&invocation.shell),
        Some(invocation.argv.clone())
    );
    let [pre_build, pre_link, linker, post_build] = invocations.as_slice() else {
        return Err("Expected build events around the linker invocation.".into());
    };
    assert_eq!(pre_build.argv, ["echo", "pre-build"]);
    assert_eq!(pre_link.argv, ["echo", "pre-link"]);
    assert!(linker.argv.iter().any(|arg| arg == "-o"));
    assert_eq!(post_build.argv, ["echo", "post-build"]);
    Ok(())
}

#[test]
fn lowers_install_rules_and_collects_metadata() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId::default();
    var(&mut model, "CMAKE_INSTALL_LIBDIR", "lib64");
    var(&mut model, "CMAKE_INSTALL_DEFAULT_COMPONENT_NAME", "Core");
    var(&mut model, "CMAKE_C_COMPILER_ID", "GNU");
    var(
        &mut model,
        "CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES",
        "/one;/two",
    );
    model.cache.insert(
        "CMAKE_C_COMPILER".into(),
        CacheEntry {
            value: "/tools/cc".into(),
            ty: CacheType::Filepath,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    model
        .dir_mut(DirId::ROOT)
        .listfiles
        .push("/project/CMakeLists.txt".into());
    let child = model.add_directory(
        DirId::ROOT,
        "/project/sub".into(),
        "/build/sub".into(),
        Default::default(),
    )?;
    model.dir_mut(child).listfiles.extend([
        BString::from("/project/CMakeLists.txt"),
        BString::from("/project/sub/CMakeLists.txt"),
    ]);
    let mut raw = raw_target("foo", TargetKind::SharedLibrary);
    raw.props.set("VERSION".into(), "2.3".into());
    raw.props.set("SOVERSION".into(), "2".into());
    let target = resolve_target(&mut model, raw, &config, &["a.c"])?;
    let rules = [
        (
            InstallKind::Targets {
                targets: vec![target.id],
                export: Some("pkg".into()),
            },
            InstallOptions::default(),
        ),
        (
            InstallKind::Files(vec![bt("README")]),
            InstallOptions {
                destination: Some("share".into()),
                rename: Some("MANUAL".into()),
                optional: true,
                ..InstallOptions::default()
            },
        ),
        (
            InstallKind::Programs(vec![bt("bootstrap")]),
            InstallOptions::default(),
        ),
        (
            InstallKind::Directory(vec![bt("docs/")]),
            InstallOptions::default(),
        ),
        (
            InstallKind::Script("after.cmake".into()),
            InstallOptions::default(),
        ),
        (
            InstallKind::Code("message(STATUS ready)".into()),
            InstallOptions::default(),
        ),
        (
            InstallKind::Export {
                set: "pkg".into(),
                namespace: "pkg::".into(),
                file: None,
                cxx_modules_directory: None,
            },
            InstallOptions {
                destination: Some("lib64/cmake/pkg".into()),
                ..InstallOptions::default()
            },
        ),
    ];
    for (kind, options) in rules {
        model.installs.push(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind,
            options,
        });
    }
    for (index, word) in [
        "DIRECTORY",
        "docs/",
        "DESTINATION",
        "share",
        "FILES_MATCHING",
        "PATTERN",
        "*.md",
    ]
    .into_iter()
    .enumerate()
    {
        var(&mut model, &format!("_RCMAKE_INSTALL_3_ARG{index}"), word);
    }
    let install = lower_install_manifest(&model, &[target]);
    let [lib, file, program, directory, script, code, export] = install.actions.as_slice() else {
        return Err("Expected seven install actions.".into());
    };
    assert_eq!(lib.destination, "lib64");
    assert_eq!(lib.component, "Core");
    assert!(
        matches!(&lib.kind, InstallActionKind::Files { files, file_type, .. } if file_type == "SHARED_LIBRARY" && files == &["/build/libfoo.so.2.3", "/build/libfoo.so.2", "/build/libfoo.so"])
    );
    assert!(
        matches!(&file.kind, InstallActionKind::Files { files, rename: Some(rename), .. } if files == &["/project/README"] && rename == "MANUAL")
    );
    assert!(file.optional);
    assert!(
        matches!(&program.kind, InstallActionKind::Files { file_type, .. } if file_type == "PROGRAM")
    );
    assert!(
        matches!(&directory.kind, InstallActionKind::Directory { dirs, options } if dirs == &["/project/docs/"] && options == &["FILES_MATCHING", "PATTERN", "*.md"])
    );
    assert_eq!(
        script.kind,
        InstallActionKind::Script("/project/after.cmake".into())
    );
    assert_eq!(
        code.kind,
        InstallActionKind::Code("message(STATUS ready)".into())
    );
    assert_eq!(export.kind, InstallActionKind::Export { plan: 0 });
    let metadata = collect_metadata(&model);
    assert_eq!(
        metadata.listfiles,
        ["/project/CMakeLists.txt", "/project/sub/CMakeLists.txt"]
    );
    let Some(toolchain) = metadata.toolchains.get(b"C".as_slice()) else {
        return Err("Expected toolchain data for C.".into());
    };
    assert_eq!(toolchain.compiler_path, "/tools/cc");
    assert_eq!(toolchain.compiler_id, "GNU");
    assert_eq!(toolchain.implicit_includes, ["/one", "/two"]);
    Ok(())
}

#[test]
fn installs_public_headers_with_their_own_options() -> TestResult {
    let mut model = fixture("Linux");
    let generated = if cfg!(windows) {
        "C:/generated/config.h"
    } else {
        "/generated/config.h"
    };
    let child = model.add_directory(
        DirId::ROOT,
        "/project/sub".into(),
        "/build/sub".into(),
        Default::default(),
    )?;
    let mut raw = raw_target("api", TargetKind::StaticLibrary);
    raw.dir = child;
    raw.props
        .set("PUBLIC_HEADER".into(), format!("api.h;;{generated}").into());
    let target = resolve_target(&mut model, raw, &ConfigId("Debug".into()), &["a.c"])?;
    for _ in 0..2 {
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind: InstallKind::Targets {
                targets: vec![target.id],
                export: None,
            },
            options: InstallOptions::default(),
        });
    }
    for (index, word) in [
        "TARGETS",
        "api",
        "ARCHIVE",
        "DESTINATION",
        "lib64",
        "CONFIGURATIONS",
        "Release",
        "PUBLIC_HEADER",
        "DESTINATION",
        "headers",
        "COMPONENT",
        "Development",
        "OPTIONAL",
        "CONFIGURATIONS",
        "Debug",
        "PERMISSIONS",
        "OWNER_READ",
    ]
    .into_iter()
    .enumerate()
    {
        var(&mut model, &format!("_RCMAKE_INSTALL_1_ARG{index}"), word);
    }
    let manifest = lower_install_manifest(&model, &[target]);
    let [_, defaults, explicit] = manifest.actions.as_slice() else {
        return Err("Expected one archive and two header actions.".into());
    };
    assert_eq!(defaults.destination, "include");
    assert_eq!(explicit.destination, "headers");
    assert_eq!(explicit.component, "Development");
    assert_eq!(explicit.configurations, ["Debug"]);
    assert!(explicit.optional);
    for action in [defaults, explicit] {
        assert!(
            matches!(&action.kind, InstallActionKind::Files { file_type, files, .. }
            if file_type == "FILE" && files == &["/project/api.h", generated])
        );
    }
    assert!(
        matches!(&explicit.kind, InstallActionKind::Files { permissions, .. }
        if permissions == &["OWNER_READ"])
    );
    Ok(())
}

#[test]
fn target_events_evaluate_artifacts_and_run_after_linker_symlinks() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId("Release".into());
    let mut library = raw_target("png_shared", TargetKind::SharedLibrary);
    library.props.set("OUTPUT_NAME".into(), "png16".into());
    library.props.set("VERSION".into(), "16.50.0".into());
    library.props.set("SOVERSION".into(), "16".into());
    library.props.set("MARKER".into(), "library context".into());
    let library = resolve_target(&mut model, library, &config, &["png.c"])?;
    let mut pre = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: library.id,
            event: TargetEvent::PreLink,
        },
        Default::default(),
        Default::default(),
    );
    pre.depends.push(bt("input-$<CONFIG>.txt"));
    pre.command_lines
        .push(vec!["echo".into(), "$<TARGET_PROPERTY:MARKER>".into()]);
    model.add_command(pre)?;
    let mut post = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: library.id,
            event: TargetEvent::PostBuild,
        },
        Default::default(),
        Default::default(),
    );
    post.command_lines.push(
        [
            "rcmake",
            "-E",
            "create_symlink",
            "$<TARGET_LINKER_FILE_NAME:png_shared>",
            "$<TARGET_LINKER_FILE_DIR:png_shared>/libpng.dylib",
        ]
        .map(BString::from)
        .to_vec(),
    );
    post.byproducts
        .push(bt("$<$<CONFIG:Release>:libpng.dylib>"));
    model.add_command(post)?;

    let graph = lower(&model, &[library], &config)?;
    let [
        BuildStep::Compile { .. },
        BuildStep::Link {
            step,
            inputs,
            invocations,
            ..
        },
    ] = graph.steps.as_slice()
    else {
        return Err("Expected one compile step and one link step with event byproducts.".into());
    };
    assert_eq!(step.output, "libpng16.16.50.0.dylib");
    assert_eq!(
        graph
            .link_event_byproducts
            .get(b"libpng16.16.50.0.dylib".as_slice()),
        Some(&vec![BString::from("libpng.dylib")])
    );
    assert!(inputs.contains(&"input-Release.txt".into()));
    let [pre, linker, symlinks, post] = invocations.as_slice() else {
        return Err(
            "Expected the target events before linking and after the linker symlinks.".into(),
        );
    };
    assert_eq!(pre.argv, ["echo", "library context"]);
    assert!(linker.argv.iter().any(|arg| arg == "-o"));
    assert_eq!(symlinks.argv.get(2), Some(&"cmake_symlink_library".into()));
    assert_eq!(
        post.argv,
        [
            "rcmake",
            "-E",
            "create_symlink",
            "libpng16.dylib",
            "/build/libpng.dylib"
        ]
    );
    Ok(())
}

#[test]
fn utility_events_share_one_recipe_in_event_order() -> TestResult {
    let mut model = fixture("Linux");
    let config = ConfigId("Release".into());
    let utility = resolve_target(
        &mut model,
        raw_target("assets", TargetKind::Utility),
        &config,
        &[],
    )?;
    let command =
        |kind| CustomCommand::new(DirId::ROOT, kind, Default::default(), Default::default());
    let event = |event| CustomCommandKind::TargetEvent {
        target: utility.id,
        event,
    };
    let mut post = command(event(TargetEvent::PostBuild));
    post.byproducts.push(bt("post-$<CONFIG>.txt"));
    post.command_lines.push(vec!["echo".into(), "post".into()]);
    model.add_command(post)?;
    let mut main = command(CustomCommandKind::Utility(utility.id));
    main.depends.extend([bt("assets"), bt("/project/assets")]);
    main.command_expand_lists = true;
    main.command_lines.push(
        ["$<0:program>", "echo", "a;;b;", "$<0:arg>", "", "last"]
            .map(BString::from)
            .to_vec(),
    );
    model.add_command(main)?;
    let mut pre = command(event(TargetEvent::PreBuild));
    pre.command_lines
        .push(vec!["$<0:program>".into(), "skip".into()]);
    pre.command_lines.push(vec!["echo".into(), "pre".into()]);
    model.add_command(pre)?;
    let mut pre_link = command(event(TargetEvent::PreLink));
    pre_link
        .command_lines
        .push(vec!["echo".into(), "pre-link".into()]);
    model.add_command(pre_link)?;
    let graph = lower(&model, &[utility], &config)?;
    let [BuildStep::Custom(recipe)] = graph.steps.as_slice() else {
        return Err("Expected one recipe for the utility target.".into());
    };
    assert_eq!(recipe.depends, ["/project/assets"]);
    let [main, pre, post] = recipe.invocations.as_slice() else {
        return Err("Expected three commands after generator expression evaluation.".into());
    };
    assert_eq!(pre.argv, ["echo", "pre"]);
    assert_eq!(main.argv, ["echo", "a", "b", "last"]);
    assert_eq!(post.argv, ["echo", "post"]);
    assert_eq!(recipe.byproducts, ["post-Release.txt"]);
    Ok(())
}

#[test]
fn compile_excludes_normalized_implicit_include_paths() -> TestResult {
    let mut model = fixture("Darwin");
    let config = ConfigId::default();
    model.dir_mut(DirId::ROOT).props.set(
        "CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES".into(),
        "/sdk/usr/include;/toolchain/./include/".into(),
    );
    var(
        &mut model,
        "CMAKE_CXX_IMPLICIT_INCLUDE_DIRECTORIES",
        "/keep",
    );
    let mut app = resolve_target(
        &mut model,
        raw_target("app", TargetKind::Executable),
        &config,
        &["main.c"],
    )?;
    app.compile_groups[0].includes = [
        ("/sdk/usr/lib/../include/", true),
        ("/toolchain/include", false),
        ("/sdk/usr/include/addon", true),
        ("/keep/", false),
    ]
    .into_iter()
    .map(|(path, system)| InterfaceInclude {
        path: bt(path),
        system,
    })
    .collect();
    let graph = lower(&model, &[app], &config)?;
    let Some(BuildStep::Compile { invocation, .. }) = graph.steps.first() else {
        return Err("Expected a C compiler invocation.".into());
    };
    assert!(
        invocation
            .argv
            .windows(2)
            .any(|pair| pair == ["-isystem", "/sdk/usr/include/addon"])
    );
    assert!(invocation.argv.contains(&"-I/keep".into()));
    assert!(!invocation.argv.contains(&"/sdk/usr/lib/../include".into()));
    assert!(!invocation.argv.contains(&"-I/toolchain/include".into()));
    Ok(())
}

#[test]
fn ninja_linker_depfile_uses_the_configured_wrapper_and_support_directory() -> TestResult {
    use rcmake_model::GeneratorKind::{Ninja, NinjaMultiConfig};

    for (system, generator, wrapper, sep, disabled, expected) in [
        ("Linux", Ninja, "-Wl,", ",", "", true),
        ("Darwin", NinjaMultiConfig, "-Xlinker; ", "", "", true),
        ("Linux", Ninja, "-Wl,", ",", "global", false),
        ("Linux", Ninja, "-Wl,", ",", "language", false),
        ("Linux", Ninja, "-Wl,", ",", "shared", false),
    ] {
        let config = ConfigId("Debug".into());
        let mut model = fixture(system);
        model.generator = Some(generator);
        var(&mut model, "CMAKE_HOST_SYSTEM_NAME", system);
        var(&mut model, "CMAKE_C_COMPILER", "cc");
        var(&mut model, "CMAKE_C_LINKER_WRAPPER_FLAG", wrapper);
        var(&mut model, "CMAKE_C_LINKER_WRAPPER_FLAG_SEP", sep);
        var(
            &mut model,
            "CMAKE_C_LINKER_DEPFILE_FLAGS",
            "LINKER:--dependency-file=<DEP_FILE>",
        );
        var(&mut model, "CMAKE_C_LINKER_DEPFILE_FORMAT", "gcc");
        var(
            &mut model,
            "CMAKE_C_LINK_DEPENDS_USE_LINKER",
            if disabled == "language" { "OFF" } else { "ON" },
        );
        if disabled == "global" {
            var(&mut model, "CMAKE_LINK_DEPENDS_USE_LINKER", "OFF");
        }
        let mut raw = raw_target("app", TargetKind::Executable);
        if disabled == "shared" {
            raw.props.set("LINK_DEPENDS_NO_SHARED".into(), "ON".into());
        }
        let target = resolve_target(&mut model, raw, &config, &["main.c"])?;
        let graph = lower(&model, &[target], &config)?;
        let Some(BuildStep::Link {
            step, invocations, ..
        }) = graph.steps.last()
        else {
            return Err("Expected a link step.".into());
        };
        let path = if generator == Ninja {
            "CMakeFiles/app.dir/link.d"
        } else {
            "CMakeFiles/app.dir/Debug/link.d"
        };
        assert_eq!(
            graph.link_depfiles.get(&step.output),
            expected.then(|| path.into()).as_ref()
        );
        let flag = format!("--dependency-file={path}");
        let args = &invocations[0].argv;
        if expected && generator == NinjaMultiConfig {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "-Xlinker" && pair[1] == flag.as_str())
            );
        } else {
            assert_eq!(
                args.iter().any(|arg| arg == format!("-Wl,{flag}").as_str()),
                expected
            );
        }
    }
    Ok(())
}

#[test]
fn ninja_compile_definitions_quote_the_value() {
    let config = ConfigId("Debug".into());
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    var(&mut model, "CMAKE_HOST_SYSTEM_NAME", "Linux");
    let target = raw_target("app", TargetKind::Executable);
    let group = CompileGroup {
        defines: vec![bt("WORDS=long long"), bt("EMPTY="), bt("PLAIN")],
        ..Default::default()
    };
    assert_eq!(
        super::compile::formatted_defines(&model, &target, &config, &group),
        r#"-DEMPTY="" -DPLAIN -DWORDS="long long""#,
    );
}

#[test]
fn ninja_multi_config_sorts_cmake_intdir_with_source_defines() {
    let config = ConfigId("Debug".into());
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    var(&mut model, "CMAKE_HOST_SYSTEM_NAME", "Linux");
    let target = raw_target("app", TargetKind::Executable);
    let group = CompileGroup {
        defines: vec![
            bt("ZZZ=1"),
            bt("FOO=1"),
            bt("AAA=1"),
            bt("SRC_Z=1"),
            bt("SRC_A=1"),
        ],
        source_define_start: Some(3),
        ..Default::default()
    };
    assert_eq!(
        super::compile::formatted_defines(&model, &target, &config, &group),
        r#"-DAAA=1 -DFOO=1 -DZZZ=1 -DCMAKE_INTDIR=\"Debug\" -DSRC_A=1 -DSRC_Z=1"#,
    );

    // Source defines sorting before (`AAA=1`), equal (`CMAKE_INTDIR` / exact duplicate),
    // and after (`ZZZ=1`) share one sorted/deduped set with `CMAKE_INTDIR`, while
    // target defines stay first.
    let ordered_group = CompileGroup {
        defines: vec![
            bt("TGT_Z=1"),
            bt("TGT_A=1"),
            bt("ZZZ=1"),
            bt("CMAKE_INTDIR"),
            bt("CMAKE_INTDIR=\"Debug\""),
            bt("AAA=1"),
        ],
        source_define_start: Some(2),
        ..Default::default()
    };
    assert_eq!(
        super::compile::formatted_defines(&model, &target, &config, &ordered_group),
        r#"-DTGT_A=1 -DTGT_Z=1 -DAAA=1 -DCMAKE_INTDIR -DCMAKE_INTDIR=\"Debug\" -DZZZ=1"#,
    );
}

#[test]
fn two_token_link_flags_are_not_file_inputs_and_ipo_precedes_arch() -> TestResult {
    let mut model = fixture("Darwin");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let config = ConfigId("Release".into());
    var(&mut model, "CMAKE_C_FLAGS_RELEASE", "-O3 -DNDEBUG");
    var(&mut model, "CMAKE_C_COMPILE_OPTIONS_IPO", "-flto=thin");
    var(&mut model, "CMAKE_C_LINK_OPTIONS_IPO", "-fuse-ld=lld");
    var(&mut model, "CMAKE_OSX_ARCHITECTURES", "arm64");
    var(
        &mut model,
        "CMAKE_C_LINK_EXECUTABLE",
        "<CMAKE_C_COMPILER> <FLAGS> <CMAKE_C_LINK_FLAGS> <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>",
    );
    let mut app = raw_target("app", TargetKind::Executable);
    app.policies.policies.set(
        rcmake_diag::PolicyId::new(69),
        rcmake_diag::PolicyStatus::New,
    )?;
    app.props
        .set("INTERPROCEDURAL_OPTIMIZATION".into(), "ON".into());
    let link_item = |text: &str| rcmake_model::LinkItem {
        text: bt(text),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    };
    app.sources.push(bt("main.c"));
    app.usage.link_libraries = Some(vec![
        link_item("-F /System/Library/PrivateFrameworks"),
        link_item("-L /opt/custom/lib"),
        link_item("-Wl,--whole-archive /opt/custom/libfoo.a"),
        link_item("/usr/lib/libIOReport.tbd"),
    ]);
    model.add_target(app)?;
    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/main.c"), b"int main() {}")?;

    let result = crate::resolve(&model, &host, &[config])?;
    let Some(BuildStep::Link {
        inputs,
        invocations,
        ..
    }) = result.graphs[0].steps.last()
    else {
        return Err("Expected link step".into());
    };
    assert!(
        !inputs
            .iter()
            .any(|p| p == b"/System/Library/PrivateFrameworks"
                || p == b"/opt/custom/lib"
                || p == b"/opt/custom/libfoo.a"),
        "flag arguments must not become link inputs: {inputs:?}"
    );
    assert!(
        inputs.iter().any(|p| p == b"/usr/lib/libIOReport.tbd"),
        "real .tbd file must stay a link input: {inputs:?}"
    );
    let shell = invocations[0].shell.as_slice();
    let flto = shell
        .windows(b"-flto=thin".len())
        .position(|w| w == b"-flto=thin")
        .ok_or("missing -flto=thin")?;
    let arch = shell
        .windows(b"-arch arm64".len())
        .position(|w| w == b"-arch arm64")
        .ok_or("missing -arch arm64")?;
    let lld = shell
        .windows(b"-fuse-ld=lld".len())
        .position(|w| w == b"-fuse-ld=lld")
        .ok_or("missing -fuse-ld=lld")?;
    assert!(
        flto < arch && arch < lld,
        "expected -flto=thin before -arch arm64 and -fuse-ld=lld after: {}",
        invocations[0].shell
    );
    Ok(())
}

#[test]
fn imported_frameworks_and_non_dll_def_files_populate_link_implicit_inputs() -> TestResult {
    let mut model = fixture("Darwin");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let config = ConfigId::default();

    let mut gl = Target::new(
        "OpenGL::GL".into(),
        TargetKind::UnknownLibrary,
        TargetOrigin::Imported(rcmake_model::ImportedVisibility::Global),
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    gl.props.set(
        "IMPORTED_LOCATION".into(),
        "/SDK/System/Library/Frameworks/OpenGL.framework".into(),
    );
    model.add_target(gl)?;

    let link_item = |text: &str| rcmake_model::LinkItem {
        text: bt(text),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    };
    let mut shared = raw_target("rsgl", TargetKind::SharedLibrary);
    shared
        .sources
        .extend([bt("gl.c"), bt("exports.def"), bt("ignored.def")]);
    shared.usage.link_libraries = Some(vec![
        link_item("OpenGL::GL"),
        link_item("/SDK/System/Library/Frameworks/Cocoa.framework"),
    ]);
    model.add_target(shared)?;

    let mut exe_plain = raw_target("plain_exe", TargetKind::Executable);
    exe_plain.sources.extend([bt("gl.c"), bt("exports.def")]);
    model.add_target(exe_plain)?;

    let mut exe_exp = raw_target("export_exe", TargetKind::Executable);
    exe_exp.props.set("ENABLE_EXPORTS".into(), "ON".into());
    exe_exp.sources.extend([bt("gl.c"), bt("exports.def")]);
    model.add_target(exe_exp)?;

    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/ignored.def".into())
        .or_default()
        .set("HEADER_FILE_ONLY".into(), "ON".into());

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/gl.c"), b"int main() {}")?;
    host.seed_file(std::path::Path::new("/project/exports.def"), b"EXPORTS")?;
    host.seed_file(std::path::Path::new("/project/ignored.def"), b"EXPORTS")?;

    let result = crate::resolve(&model, &host, &[config])?;
    let link_implicits = |name: &[u8]| -> Vec<BString> {
        let target = result.graphs[0]
            .targets
            .iter()
            .find(|t| t.name == name)
            .expect("target");
        target
            .steps
            .iter()
            .find_map(|&idx| match &result.graphs[0].steps[idx] {
                BuildStep::Link {
                    implicit_inputs, ..
                } => Some(implicit_inputs.clone()),
                _ => None,
            })
            .expect("link step")
    };
    let rsgl = link_implicits(b"rsgl");
    assert!(
        rsgl.contains(&"/SDK/System/Library/Frameworks/OpenGL.framework".into()),
        "imported .framework must be in implicit_inputs: {rsgl:?}"
    );
    assert!(
        !rsgl.contains(&"/SDK/System/Library/Frameworks/Cocoa.framework".into()),
        "raw .framework path must not be in implicit_inputs: {rsgl:?}"
    );
    assert!(
        rsgl.contains(&"/project/exports.def".into()),
        "shared library .def must be in implicit_inputs: {rsgl:?}"
    );
    assert!(
        !rsgl.contains(&"/project/ignored.def".into()),
        "HEADER_FILE_ONLY .def must not be in implicit_inputs: {rsgl:?}"
    );
    assert!(
        !link_implicits(b"plain_exe").contains(&"/project/exports.def".into()),
        "executable without ENABLE_EXPORTS must not include .def"
    );
    assert!(
        link_implicits(b"export_exe").contains(&"/project/exports.def".into()),
        "executable with ENABLE_EXPORTS must include .def"
    );
    Ok(())
}

#[test]
fn posix_link_wildcards_use_double_quotes_and_asm_omits_bogus_color_flag() -> TestResult {
    let mut model = fixture("Darwin");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let config = ConfigId::default();
    var(&mut model, "CMAKE_HOST_SYSTEM_NAME", "Darwin");
    var(&mut model, "CMAKE_C_LINKER_WRAPPER_FLAG", "-Xlinker; ");
    var(&mut model, "CMAKE_COLOR_DIAGNOSTICS", "OFF");
    var(&mut model, "CMAKE_ASM_COMPILER", "/usr/bin/cc");
    var(&mut model, "CMAKE_ASM_COMPILER_ID", "AppleClang");
    var(
        &mut model,
        "CMAKE_ASM_COMPILE_OBJECT",
        "<CMAKE_ASM_COMPILER> <DEFINES> <INCLUDES> <FLAGS> -o <OBJECT> -c <SOURCE>",
    );
    var(&mut model, "CMAKE_ASM_SOURCE_FILE_EXTENSIONS", "S;s");

    let mut lib = raw_target("usearch", TargetKind::SharedLibrary);
    lib.sources.extend([bt("a.c"), bt("b.c"), bt("k.S")]);
    lib.usage.push(
        rcmake_model::UsageField::LinkOptions,
        bt("LINKER:-exported_symbol,_usearch_*"),
    );
    lib.usage.push(
        rcmake_model::UsageField::LinkOptions,
        bt("-Wl,-exported_symbol,_extra_*"),
    );
    model.add_target(lib)?;

    // Empty-evaluating per-source COMPILE_OPTIONS must split compile groups for codemodel
    // without polluting Ninja compile commands.
    let props = model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/b.c".into())
        .or_default();
    props.set_tracked(
        "COMPILE_OPTIONS".into(),
        "$<$<BOOL:OFF>:-fno-sanitize=function>".into(),
        bt("set_source_files_properties").backtrace,
    );

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/a.c"), b"void a() {}")?;
    host.seed_file(std::path::Path::new("/project/b.c"), b"void b() {}")?;
    host.seed_file(std::path::Path::new("/project/k.S"), b".text\n")?;

    let result = crate::resolve(&model, &host, &[config])?;
    let resolved = &result.targets[0];
    let c_groups: Vec<_> = resolved
        .compile_groups
        .iter()
        .filter(|g| g.language == b"C")
        .collect();
    assert_eq!(
        c_groups.len(),
        2,
        "empty-genex per-source COMPILE_OPTIONS must stay a distinct compile group"
    );
    assert_eq!(c_groups[1].flags.len(), 1);
    assert_eq!(c_groups[1].flags[0].value, "SHELL:");

    for step in &result.graphs[0].steps {
        if let BuildStep::Compile {
            source, invocation, ..
        } = step
        {
            if source.ends_with(b"k.S") {
                assert!(
                    !invocation
                        .shell
                        .windows(b"-fno-color-diagnostics".len())
                        .any(|w| w == b"-fno-color-diagnostics"),
                    "ASM without CMAKE_ASM_COMPILE_OPTIONS_COLOR_DIAGNOSTICS_OFF must not emit -fno-color-diagnostics: {}",
                    invocation.shell
                );
            }
            assert!(
                !invocation
                    .shell
                    .windows(b"SHELL:".len())
                    .any(|w| w == b"SHELL:"),
                "SHELL: sentinel must never appear in compile commands: {}",
                invocation.shell
            );
        }
        if let BuildStep::Link { invocations, .. } = step {
            let shell = &invocations[0].shell;
            assert!(
                shell
                    .windows(b"-Xlinker \"_usearch_*\"".len())
                    .any(|w| w == b"-Xlinker \"_usearch_*\""),
                "wildcard LINKER: option must use double quotes: {shell}"
            );
            assert!(
                shell
                    .windows(b"\"-Wl,-exported_symbol,_extra_*\"".len())
                    .any(|w| w == b"\"-Wl,-exported_symbol,_extra_*\""),
                "wildcard link option must use double quotes: {shell}"
            );
        }
    }
    Ok(())
}

#[test]
fn custom_command_preserves_duplicate_depends() -> TestResult {
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let mut cmd = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    cmd.outputs.push(bt("complete"));
    cmd.depends
        .extend([bt("install"), bt("mkdir"), bt("install")]);
    cmd.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "touch".into(),
        "complete".into(),
    ]);
    model.add_command(cmd)?;
    let host = rcmake_host::PureHost::default();
    let result = crate::resolve(&model, &host, &[])?;
    let Some(BuildStep::Custom(step)) = result.graphs[0].steps.first() else {
        return Err("Expected custom step".into());
    };
    assert_eq!(step.depends, ["install", "mkdir", "install"]);
    Ok(())
}

#[test]
fn custom_target_depends_attaches_main_dependency_command_on_existing_source() -> TestResult {
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let mut copy = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    copy.outputs.push(bt("/build/data/unicode.txt"));
    copy.main_dependency = Some("/project/data/unicode.txt".into());
    copy.command_lines.push(vec![
        "cmake".into(),
        "-E".into(),
        "copy".into(),
        "/project/data/unicode.txt".into(),
        "/build/data/unicode.txt".into(),
    ]);
    model.add_command(copy)?;
    let target_id = model.add_target(raw_target("test_data", TargetKind::Utility))?;
    let mut util = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(target_id),
        Default::default(),
        Default::default(),
    );
    util.depends.push(bt("data/unicode.txt"));
    model.add_command(util)?;
    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/data/unicode.txt"), b"ok")?;

    let result = crate::resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    let target = graph
        .targets
        .iter()
        .find(|t| t.name == "test_data")
        .ok_or("missing test_data target")?;
    assert_eq!(
        target.steps.len(),
        2,
        "test_data must own both the copy custom command (via MAIN_DEPENDENCY) and its utility step: {:?}",
        target.steps
    );
    let util_step = target
        .steps
        .iter()
        .find_map(|&idx| match &graph.steps[idx] {
            BuildStep::Custom(step) if step.outputs.contains(&"CMakeFiles/test_data".into()) => {
                Some(step)
            }
            _ => None,
        })
        .ok_or("missing utility step")?;
    assert_eq!(util_step.depends, ["/project/data/unicode.txt"]);
    Ok(())
}

#[test]
fn feature_wrapped_whole_archive_recovers_single_and_multi_token_and_ignores_raw_flags()
-> TestResult {
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let config = ConfigId::default();
    var(&mut model, "CMAKE_C_LINKER_WRAPPER_FLAG", "-Wl,");
    var(&mut model, "CMAKE_C_LINKER_WRAPPER_FLAG_SEP", ",");
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_MULTI_SUPPORTED",
        "ON",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_MULTI",
        "-Wl,--whole-archive <LINK_ITEM> -Wl,--no-whole-archive",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_SINGLE_SUPPORTED",
        "ON",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_SINGLE",
        "LINKER:--whole-archive,<LINK_ITEM>,--no-whole-archive",
    );

    let link_item = |text: &str| rcmake_model::LinkItem {
        text: bt(text),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    };
    let mut app = raw_target("app", TargetKind::Executable);
    app.sources.push(bt("main.c"));
    app.usage.link_libraries = Some(vec![
        link_item("$<LINK_LIBRARY:WHOLE_MULTI,/opt/lib/libmulti.a>"),
        link_item("$<LINK_LIBRARY:WHOLE_SINGLE,/opt/lib/libsingle.a>"),
        link_item("-Wl,--whole-archive /opt/lib/libraw_multi.a"),
        link_item("-Wl,--whole-archive,/opt/lib/libraw_single.a"),
    ]);
    model.add_target(app)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/main.c"), b"int main() {}")?;

    let result = crate::resolve(&model, &host, &[config])?;
    let Some(BuildStep::Link { inputs, step, .. }) = result.graphs[0].steps.last() else {
        return Err("Expected link step".into());
    };
    assert!(
        step.fragments.iter().any(|f| {
            f.role == crate::LinkFragmentRole::Libraries
                && f.text.value == "-Wl,--whole-archive /opt/lib/libmulti.a -Wl,--no-whole-archive"
        }),
        "expected multi-token feature fragment: {:?}",
        step.fragments
    );
    assert!(
        step.fragments.iter().any(|f| {
            f.role == crate::LinkFragmentRole::Libraries
                && f.text.value == "-Wl,--whole-archive,/opt/lib/libsingle.a,--no-whole-archive"
        }),
        "expected single comma-joined feature fragment: {:?}",
        step.fragments
    );
    assert!(
        inputs.contains(&"/opt/lib/libmulti.a".into()),
        "multi-token LINK_LIBRARY feature must recover /opt/lib/libmulti.a: {inputs:?}"
    );
    assert!(
        inputs.contains(&"/opt/lib/libsingle.a".into()),
        "single-token LINK_LIBRARY feature must recover /opt/lib/libsingle.a: {inputs:?}"
    );
    assert!(
        !inputs.contains(&"/opt/lib/libraw_multi.a".into())
            && !inputs.contains(&"/opt/lib/libraw_single.a".into()),
        "raw -Wl flags without LINK_LIBRARY must not become link inputs: {inputs:?}"
    );
    Ok(())
}

#[test]
fn explicit_link_depends_stays_in_implicit_inputs_when_also_linked() -> TestResult {
    let mut model = fixture("Linux");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let config = ConfigId::default();

    let mut shared = raw_target("shared_b", TargetKind::SharedLibrary);
    shared.sources.push(bt("lib.c"));
    model.add_target(shared)?;

    let mut app = raw_target("app", TargetKind::Executable);
    app.sources.push(bt("main.c"));
    app.props.set("LINK_DEPENDS_NO_SHARED".into(), "ON".into());
    app.props
        .set("LINK_DEPENDS".into(), "$<TARGET_FILE:shared_b>".into());
    app.usage.link_libraries = Some(vec![rcmake_model::LinkItem {
        text: bt("shared_b"),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    }]);
    model.add_target(app)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(
        std::path::Path::new("/project/lib.c"),
        b"int f() { return 1; }",
    )?;
    host.seed_file(
        std::path::Path::new("/project/main.c"),
        b"int main() { return 0; }",
    )?;

    let result = crate::resolve(&model, &host, &[config])?;
    let app_target = result.graphs[0]
        .targets
        .iter()
        .find(|t| t.name == "app")
        .ok_or("missing app target")?;
    let (inputs, implicit_inputs) = app_target
        .steps
        .iter()
        .find_map(|&idx| match &result.graphs[0].steps[idx] {
            BuildStep::Link {
                inputs,
                implicit_inputs,
                ..
            } => Some((inputs, implicit_inputs)),
            _ => None,
        })
        .ok_or("missing app link step")?;
    assert!(
        inputs.contains(&"libshared_b.so".into()),
        "linked shared library should appear in inputs: {inputs:?}"
    );
    assert!(
        implicit_inputs.contains(&"libshared_b.so".into()),
        "explicit LINK_DEPENDS must remain in implicit_inputs even when also in inputs: {implicit_inputs:?}"
    );
    Ok(())
}

#[test]
fn cluster1_windows_interface_sources_and_export_paths() -> TestResult {
    let mut model = fixture("Windows");
    model.set_root_dirs("/src".into(), "/build".into());
    var(&mut model, "CMAKE_HOST_SYSTEM_NAME", "Windows");
    let mut iface = raw_target("iface", TargetKind::InterfaceLibrary);
    iface.usage.push(
        rcmake_model::UsageField::InterfaceSources,
        bt("/src/iface.c"),
    );
    model.add_target(iface)?;
    let mut app = raw_target("app", TargetKind::Executable);
    app.sources.push(bt("main.c"));
    app.usage.link_libraries = Some(vec![rcmake_model::LinkItem {
        text: bt("iface"),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    }]);
    model.add_target(app)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/src/iface.c"), b"int f(){return 1;}")?;
    host.seed_file(
        std::path::Path::new("/src/main.c"),
        b"int main(){return 0;}",
    )?;
    let result = crate::resolve(&model, &host, &[])?;
    assert!(
        result.targets[1]
            .compile_units
            .iter()
            .any(|unit| unit.source.value == "/src/iface.c")
    );
    assert!(crate::usage::paths::full_path_for_model(
        &model,
        DirId::ROOT,
        b"C:/src/iface.c"
    ));

    // Relative interface source must still be rejected.
    let mut bad_model = model.clone();
    let iface_id = bad_model
        .find_target(DirId::ROOT, b"iface")
        .ok_or("iface")?;
    bad_model
        .target_mut(iface_id)
        .ok_or("iface mut")?
        .usage
        .push(rcmake_model::UsageField::InterfaceSources, bt("rel/bad.c"));
    assert!(crate::resolve(&bad_model, &host, &[]).is_err());

    // Export interface path validation accepts `/include` and unterminated genex on Windows.
    let target = model.target(iface_id).ok_or("iface")?;
    let mut export_model = model.clone();
    export_model
        .target_mut(iface_id)
        .ok_or("iface mut")?
        .usage
        .push(
            rcmake_model::UsageField::InterfaceIncludeDirectories,
            bt("/include;/include/$<BUILD_INTERFACE:subdir"),
        );
    let _ = target;
    let export_target = export_model.target(iface_id).ok_or("iface")?;
    let mut usage = crate::usage::UsageResolver::new(&export_model);
    let _ = &mut usage;
    assert!(crate::usage::paths::full_path_for_model(
        &export_model,
        export_target.dir,
        b"/include"
    ));
    assert!(crate::usage::paths::full_path_for_model(
        &export_model,
        export_target.dir,
        b"/include/$<BUILD_INTERFACE:subdir"
    ));
    assert!(!crate::usage::paths::full_path_for_model(
        &export_model,
        export_target.dir,
        b"relative/include"
    ));

    // POSIX host cross-compiling to Windows uses host path rules, not target platform rules.
    let mut cross_model = fixture("Windows");
    var(&mut cross_model, "CMAKE_HOST_SYSTEM_NAME", "Linux");
    assert!(!crate::usage::paths::full_path_for_model(
        &cross_model,
        DirId::ROOT,
        b"C:/src/iface.c"
    ));
    Ok(())
}

#[test]
fn cluster2_windows_optional_import_libs_and_module_defaults() -> TestResult {
    let mut model = fixture("Windows");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    var(&mut model, "CMAKE_IMPORT_LIBRARY_SUFFIX", ".lib");
    var(&mut model, "CMAKE_INSTALL_BINDIR", "custom_bin");

    let mut shared = raw_target("shlib", TargetKind::SharedLibrary);
    shared.sources.push(bt("a.c"));
    let shared_id = model.add_target(shared)?;

    let mut exe = raw_target("app", TargetKind::Executable);
    exe.props.set("ENABLE_EXPORTS".into(), "ON".into());
    exe.sources.push(bt("b.c"));
    let exe_id = model.add_target(exe)?;

    let mut module = raw_target("plug", TargetKind::ModuleLibrary);
    module.sources.push(bt("c.c"));
    let module_id = model.add_target(module)?;

    model.add_install(rcmake_model::InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: rcmake_model::InstallKind::Targets {
            targets: vec![shared_id, exe_id, module_id],
            export: None,
        },
        options: rcmake_model::InstallOptions::default(),
    });
    for (idx, arg) in ["TARGETS", "ARCHIVE", "DESTINATION", "lib"]
        .into_iter()
        .enumerate()
    {
        model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_0_ARG{idx}").into(), arg.into());
    }
    model.add_install(rcmake_model::InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: rcmake_model::InstallKind::Targets {
            targets: vec![shared_id],
            export: None,
        },
        options: rcmake_model::InstallOptions::default(),
    });

    let host = rcmake_host::PureHost::default();
    for src in ["/project/a.c", "/project/b.c", "/project/c.c"] {
        host.seed_file(std::path::Path::new(src), b"int x(){return 0;}")?;
    }
    let result = crate::resolve(&model, &host, &[])?;

    // Shared library (rule 1, no explicit destinations):
    // install script marks the import lib OPTIONAL while File API preserves declared OPTIONAL.
    let shared_imp_action = result
        .install
        .actions
        .iter()
        .find(|act| matches!(&act.kind, InstallActionKind::Files { file_type, files, .. } if file_type == "STATIC_LIBRARY" && files.iter().any(|f| f.ends_with(b"shlib.lib"))))
        .ok_or("missing shared import install action")?;
    let shared_dll_action = result
        .install
        .actions
        .iter()
        .find(|act| matches!(&act.kind, InstallActionKind::Files { file_type, files, .. } if file_type == "SHARED_LIBRARY" && files.iter().any(|f| f.ends_with(b"shlib.dll"))))
        .ok_or("missing shared dll install action")?;
    assert!(
        shared_imp_action.optional,
        "Windows shared import library install script action must be optional"
    );
    assert!(
        !shared_dll_action.optional,
        "Windows shared DLL install script action must remain required"
    );
    let shared_installers: Vec<_> = result
        .install
        .target_installers
        .iter()
        .filter(|inst| inst.rule == 1 && inst.target == shared_id)
        .collect();
    let imp = shared_installers
        .iter()
        .find(|inst| inst.import_library)
        .ok_or("missing shared import installer")?;
    let dll = shared_installers
        .iter()
        .find(|inst| !inst.import_library)
        .ok_or("missing shared dll installer")?;
    assert!(
        !imp.optional,
        "File API shared import installer must preserve declared OPTIONAL flag"
    );
    assert!(!dll.optional, "Windows DLL must remain required");

    // Exported executable (rule 0): has optional ARCHIVE import-library installer,
    // and RUNTIME installer contains only the executable (not .lib).
    let exe_imp = result
        .install
        .target_installers
        .iter()
        .find(|inst| inst.rule == 0 && inst.target == exe_id && inst.import_library)
        .ok_or("missing exported exe import installer")?;
    assert!(exe_imp.optional);
    assert_eq!(exe_imp.destination, "lib");
    let exe_rt = result
        .install
        .target_installers
        .iter()
        .find(|inst| inst.rule == 0 && inst.target == exe_id && !inst.import_library)
        .ok_or("missing exported exe runtime installer")?;
    assert!(
        exe_rt
            .paths
            .iter()
            .all(|(_, name)| !name.ends_with(b".lib")),
        "RUNTIME installer must not include .lib: {:?}",
        exe_rt.paths
    );

    // Module library on DLL platform defaults to CMAKE_INSTALL_BINDIR (`custom_bin`).
    let mod_inst = result
        .install
        .target_installers
        .iter()
        .find(|inst| inst.rule == 0 && inst.target == module_id)
        .ok_or("missing module installer")?;
    assert_eq!(mod_inst.destination, "custom_bin");
    Ok(())
}

#[test]
fn cluster3_def_eligibility_no_exe_implib_symlink_and_vs_nm_gating() -> TestResult {
    let mut model = fixture("Windows");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    var(&mut model, "CMAKE_C_LINK_DEF_FILE_FLAG", "/DEF:");
    var(&mut model, "CMAKE_NM", "C:/tools/llvm-nm.exe");
    var(&mut model, "CMAKE_IMPORT_LIBRARY_SUFFIX", ".lib");
    var(
        &mut model,
        "CMAKE_C_LINK_EXECUTABLE",
        "<CMAKE_LINKER> /out:<TARGET> /implib:<TARGET_IMPLIB> <LINK_FLAGS> <OBJECTS>",
    );

    let mut plain_exe = raw_target("plain", TargetKind::Executable);
    plain_exe
        .props
        .set("WINDOWS_EXPORT_ALL_SYMBOLS".into(), "ON".into());
    plain_exe.sources.push(bt("main.c"));
    model.add_target(plain_exe)?;

    let mut exported_exe = raw_target("say", TargetKind::Executable);
    exported_exe.props.set("ENABLE_EXPORTS".into(), "ON".into());
    exported_exe
        .props
        .set("WINDOWS_EXPORT_ALL_SYMBOLS".into(), "ON".into());
    exported_exe.sources.push(bt("main.c"));
    model.add_target(exported_exe)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(
        std::path::Path::new("/project/main.c"),
        b"int main(){return 0;}",
    )?;
    let result = crate::resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    let link_invocations = |name: &str| {
        let target = graph.targets.iter().find(|t| t.name == name)?;
        target
            .steps
            .iter()
            .find_map(|&idx| match &graph.steps[idx] {
                BuildStep::Link { invocations, .. } => Some(invocations),
                _ => None,
            })
    };

    // Executable without ENABLE_EXPORTS must NOT run __create_def.
    let plain_cmds = link_invocations("plain").ok_or("missing plain link")?;
    assert!(
        !plain_cmds
            .iter()
            .any(|inv| inv.argv.iter().any(|arg| arg == "__create_def")),
        "plain exe must not generate .def: {plain_cmds:?}"
    );

    // Exported executable runs __create_def with --nm on Ninja, and does NOT run cmake_symlink_executable.
    let say_cmds = link_invocations("say").ok_or("missing say link")?;
    assert!(
        say_cmds
            .iter()
            .any(|inv| inv.argv.iter().any(|arg| arg == "__create_def")
                && inv
                    .argv
                    .iter()
                    .any(|arg| arg == "--nm=C:/tools/llvm-nm.exe")),
        "Ninja exported exe must pass --nm: {say_cmds:?}"
    );
    assert!(
        !say_cmds
            .iter()
            .any(|inv| inv.argv.iter().any(|arg| arg == "cmake_symlink_executable")),
        "Windows exported exe must not overwrite implib with cmake_symlink_executable: {say_cmds:?}"
    );

    // Visual Studio: generator toolset governs --nm (v143 omits even if target overrides VS_PLATFORM_TOOLSET).
    for (toolset, expect_nm) in [("v143", false), ("ClangCL", true)] {
        let mut vs_model = model.clone();
        vs_model.generator = Some(rcmake_model::GeneratorKind::VisualStudio(
            rcmake_model::VsVersion::Vs17,
        ));
        var(&mut vs_model, "CMAKE_VS_PLATFORM_TOOLSET", toolset);
        if !expect_nm
            && let Some(say_id) = vs_model.find_target(DirId::ROOT, b"say")
            && let Some(say_raw) = vs_model.target_mut(say_id)
        {
            say_raw
                .props
                .set("VS_PLATFORM_TOOLSET".into(), "ClangCL".into());
        }
        let vs_res = crate::resolve_with_options(
            &vs_model,
            &host,
            &[ConfigId("Debug".into())],
            &crate::ResolveOptions {
                ide: Some(crate::ide::IdeFlavor::VisualStudio),
                ..Default::default()
            },
        )?;
        let vs_graph = &vs_res.graphs[0];
        let say_target = vs_graph
            .targets
            .iter()
            .find(|t| t.name == "say")
            .ok_or("vs say")?;
        let say_inv = say_target
            .steps
            .iter()
            .find_map(|&idx| match &vs_graph.steps[idx] {
                BuildStep::Link { invocations, .. } => Some(invocations),
                _ => None,
            })
            .ok_or("vs say link")?;
        let has_nm = say_inv.iter().any(|inv| {
            inv.argv
                .iter()
                .any(|arg| arg == "--nm=C:/tools/llvm-nm.exe")
        });
        assert_eq!(has_nm, expect_nm, "toolset {toolset}: {say_inv:?}");
    }
    Ok(())
}

#[test]
fn cluster4_wholearchive_runtime_dlls_win32_genex_and_cmp0212() -> TestResult {
    let mut model = fixture("Windows");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    var(&mut model, "CMAKE_IMPORT_LIBRARY_SUFFIX", ".lib");
    var(&mut model, "CMAKE_C_CREATE_WIN32_EXE", "/subsystem:windows");
    var(
        &mut model,
        "CMAKE_C_CREATE_CONSOLE_EXE",
        "/subsystem:console",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_ARCHIVE_SUPPORTED",
        "ON",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_LIBRARY_USING_WHOLE_ARCHIVE",
        "/WHOLEARCHIVE:<LINK_ITEM>",
    );
    var(
        &mut model,
        "CMAKE_C_LINK_EXECUTABLE",
        "<CMAKE_LINKER> /out:<TARGET> <LINK_FLAGS> <OBJECTS> <LINK_LIBRARIES>",
    );

    let mut stat = raw_target("wholelib", TargetKind::StaticLibrary);
    stat.sources.push(bt("a.c"));
    model.add_target(stat)?;

    let mut lib1 = raw_target("lib1", TargetKind::SharedLibrary);
    lib1.sources.push(bt("a.c"));
    let mut lib2 = raw_target("lib2", TargetKind::SharedLibrary);
    lib2.sources.push(bt("a.c"));
    let mut lib3 = raw_target("lib3", TargetKind::SharedLibrary);
    lib3.sources.push(bt("a.c"));
    lib1.usage.link_libraries = Some(vec![rcmake_model::LinkItem {
        text: bt("lib2"),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Private),
    }]);
    lib1.usage.interface_link_libraries = Some(vec![rcmake_model::LinkItem {
        text: bt("lib3"),
        lookup_dir: DirId::ROOT,
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: Some(rcmake_model::UsageScope::Interface),
    }]);
    model.add_target(lib1)?;
    model.add_target(lib2)?;
    model.add_target(lib3)?;

    let mut imported = Target::new(
        "imported".into(),
        TargetKind::SharedLibrary,
        rcmake_model::TargetOrigin::Imported(rcmake_model::ImportedVisibility::Global),
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    imported
        .props
        .set("IMPORTED_LOCATION".into(), "/project/imported.dll".into());
    imported
        .props
        .set("IMPORTED_IMPLIB".into(), "/project/imported.lib".into());
    model.add_target(imported)?;

    let mut exe = raw_target("exe", TargetKind::Executable);
    exe.sources.push(bt("main.c"));
    exe.props
        .set("WIN32_EXECUTABLE".into(), "$<CONFIG:Release>".into());
    exe.usage.link_libraries = Some(vec![
        rcmake_model::LinkItem {
            text: bt("$<LINK_LIBRARY:WHOLE_ARCHIVE,wholelib>"),
            lookup_dir: DirId::ROOT,
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: Some(rcmake_model::UsageScope::Private),
        },
        rcmake_model::LinkItem {
            text: bt("lib1"),
            lookup_dir: DirId::ROOT,
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: Some(rcmake_model::UsageScope::Private),
        },
        rcmake_model::LinkItem {
            text: bt("imported"),
            lookup_dir: DirId::ROOT,
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: Some(rcmake_model::UsageScope::Private),
        },
    ]);
    let exe_id = model.add_target(exe)?;

    // CMP0212 OLD vs NEW custom command DEPENDS lookup.
    let mut old_policies = rcmake_diag::PolicyMap::default();
    old_policies.set(
        rcmake_diag::PolicyId::new(212),
        rcmake_diag::PolicyStatus::Old,
    )?;
    let mut cmd_old = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::Output,
        Default::default(),
        old_policies,
    );
    cmd_old.outputs.push(bt("out_old.txt"));
    cmd_old.depends.push(bt("exe.exe"));
    model.add_command(cmd_old)?;

    let mut new_policies = rcmake_diag::PolicyMap::default();
    new_policies.set(
        rcmake_diag::PolicyId::new(212),
        rcmake_diag::PolicyStatus::New,
    )?;
    let mut cmd_new = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::Output,
        Default::default(),
        new_policies,
    );
    cmd_new.outputs.push(bt("out_new.txt"));
    cmd_new.depends.push(bt("exe.exe"));
    model.add_command(cmd_new)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/a.c"), b"int a(){return 1;}")?;
    host.seed_file(
        std::path::Path::new("/project/main.c"),
        b"int main(){return 0;}",
    )?;
    let result = crate::resolve(&model, &host, &[ConfigId("Release".into())])?;

    // /WHOLEARCHIVE: decorated on command line, real archive path in Ninja inputs.
    let exe_graph = result.graphs[0]
        .targets
        .iter()
        .find(|t| t.name == "exe")
        .ok_or("exe graph")?;
    let (inputs, invocations) = exe_graph
        .steps
        .iter()
        .find_map(|&idx| match &result.graphs[0].steps[idx] {
            BuildStep::Link {
                inputs,
                invocations,
                ..
            } => Some((inputs, invocations)),
            _ => None,
        })
        .ok_or("exe link")?;
    assert!(
        !inputs.iter().any(|p| p.starts_with(b"/WHOLEARCHIVE:")),
        "Ninja prerequisites must not contain /WHOLEARCHIVE: prefix: {inputs:?}"
    );
    assert!(
        inputs
            .iter()
            .any(|p| p.ends_with(b"wholelib.a") || p.ends_with(b"wholelib.lib")),
        "real archive file path must be in inputs: {inputs:?}"
    );
    assert!(
        invocations
            .iter()
            .any(|inv| inv.shell.windows(14).any(|w| w == b"/WHOLEARCHIVE:")),
        "command line must keep /WHOLEARCHIVE: decoration: {invocations:?}"
    );
    assert!(
        invocations
            .iter()
            .any(|inv| inv.shell.windows(18).any(|w| w == b"/subsystem:windows")),
        "WIN32_EXECUTABLE genex must evaluate ON in Release: {invocations:?}"
    );

    // TARGET_RUNTIME_DLLS order: lib1, imported, lib3, lib2.
    let mut usage = crate::usage::UsageResolver::new(&model);
    let ctx = rcmake_genex::EvalContext {
        config: ConfigId("Release".into()),
        head: Some(crate::usage::handle(exe_id)),
        current: Some(crate::usage::handle(exe_id)),
        ..Default::default()
    };
    let (dlls, _) = rcmake_genex::evaluate(
        &rcmake_genex::parse(b"$<TARGET_RUNTIME_DLLS:exe>")?,
        &ctx,
        &mut usage.host(DirId::ROOT),
    )?;
    let names: Vec<_> = rcmake_base::list::split(&dlls, rcmake_base::list::Empty::Drop)
        .into_iter()
        .map(|p| BString::from(rcmake_base::path_lex::Lexical::new(&p).filename()))
        .collect();
    assert_eq!(names, ["lib1.dll", "imported.dll", "lib3.dll", "lib2.dll"]);

    // CMP0212 OLD attaches target dependency on `exe`, NEW keeps plain file dependency.
    let step_for = |out: &str| {
        result.graphs[0].steps.iter().find_map(|step| match step {
            BuildStep::Custom(custom) if custom.outputs.iter().any(|o| o == out) => Some(custom),
            _ => None,
        })
    };
    let old_step = step_for("out_old.txt").ok_or("old step")?;
    let new_step = step_for("out_new.txt").ok_or("new step")?;
    assert!(
        old_step
            .target_dependencies
            .iter()
            .any(|(id, _)| *id == exe_id)
    );
    assert!(new_step.target_dependencies.is_empty());
    Ok(())
}

#[test]
fn cluster5_msvc_defines_pdb_cmp0202_runtime_checks_and_pch_pdb_dir() -> TestResult {
    let mut model = fixture("Windows");
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    var(&mut model, "CMAKE_C_COMPILER_ID", "MSVC");
    var(&mut model, "CMAKE_IMPORT_LIBRARY_SUFFIX", ".lib");
    var(&mut model, "CMAKE_C_LINKER_SUPPORTS_PDB", "ON");
    var(
        &mut model,
        "CMAKE_C_SHARED_LIBRARY_COMPILE_DEFINITIONS",
        "_WINDLL",
    );
    var(
        &mut model,
        "CMAKE_MSVC_RUNTIME_CHECKS_DEFAULT",
        "StackFrameErrorCheck;UninitializedVariable",
    );
    var(
        &mut model,
        "CMAKE_C_COMPILE_OPTIONS_MSVC_RUNTIME_CHECKS_RTCsu",
        "-RTC1",
    );
    var(
        &mut model,
        "CMAKE_MSVC_DEBUG_INFORMATION_FORMAT_DEFAULT",
        "ProgramDatabase",
    );
    var(
        &mut model,
        "CMAKE_C_COMPILE_OPTIONS_MSVC_DEBUG_INFORMATION_FORMAT_ProgramDatabase",
        "-Zi",
    );

    let mut policies = rcmake_diag::PolicyMap::default();
    policies.set(
        rcmake_diag::PolicyId::new(202),
        rcmake_diag::PolicyStatus::New,
    )?;
    policies.set(
        rcmake_diag::PolicyId::new(203),
        rcmake_diag::PolicyStatus::New,
    )?;
    policies.set(
        rcmake_diag::PolicyId::new(204),
        rcmake_diag::PolicyStatus::New,
    )?;

    let mut lib = Target::new(
        "sh".into(),
        TargetKind::SharedLibrary,
        rcmake_model::TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        policies,
    );
    lib.sources.push(bt("a.c"));
    lib.props.set("DEBUG_POSTFIX".into(), "_d".into());
    lib.props.set("PDB_NAME".into(), "custom".into());
    let lib_id = model.add_target(lib)?;

    let host = rcmake_host::PureHost::default();
    host.seed_file(std::path::Path::new("/project/a.c"), b"int f(){return 1;}")?;
    let result = crate::resolve(&model, &host, &[ConfigId("Debug".into())])?;
    let sh = result
        .targets
        .iter()
        .find(|t| t.id == lib_id)
        .ok_or("missing sh")?;
    let defs: Vec<_> = sh.compile_groups[0]
        .defines
        .iter()
        .map(|d| d.value.as_slice())
        .collect();
    assert!(
        defs.contains(&b"_MBCS".as_slice()),
        "CMP0204 _MBCS missing: {defs:?}"
    );
    assert!(
        defs.contains(&b"_WINDLL".as_slice()),
        "CMP0203 _WINDLL missing: {defs:?}"
    );

    // CMP0202 NEW appends postfix once to explicit PDB_NAME, POSTFIX:EXCLUDE omits it.
    let mut usage = crate::usage::UsageResolver::new(&model);
    let ctx = rcmake_genex::EvalContext {
        config: ConfigId("Debug".into()),
        head: Some(crate::usage::handle(lib_id)),
        ..Default::default()
    };
    let (with_pf, _) = rcmake_genex::evaluate(
        &rcmake_genex::parse(b"$<TARGET_PDB_FILE_BASE_NAME:sh>")?,
        &ctx,
        &mut usage.host(DirId::ROOT),
    )?;
    let (no_pf, _) = rcmake_genex::evaluate(
        &rcmake_genex::parse(b"$<TARGET_PDB_FILE_BASE_NAME:sh,POSTFIX:EXCLUDE>")?,
        &ctx,
        &mut usage.host(DirId::ROOT),
    )?;
    assert_eq!(with_pf, "custom_d");
    assert_eq!(no_pf, "custom");

    // CMP0184 combines StackFrameErrorCheck + UninitializedVariable -> RTCsu -> -RTC1.
    let compile_cmd = result.graphs[0]
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(invocation),
            _ => None,
        })
        .ok_or("compile step")?;
    assert!(
        compile_cmd.shell.windows(5).any(|w| w == b"-RTC1"),
        "expected combined -RTC1 in compile flags: {}",
        compile_cmd.shell
    );

    // CMP0202 Warn emits exactly one policy warning via genex evaluation for both
    // explicit POSTFIX:INCLUDE and POSTFIX:EXCLUDE, and does NOT warn for 1-arg form
    // or when CMP0202 is OLD/NEW.
    for (status, expr, expect_warn) in [
        (
            rcmake_diag::PolicyStatus::Warn,
            b"$<TARGET_PDB_FILE_BASE_NAME:sh>".as_slice(),
            false,
        ),
        (
            rcmake_diag::PolicyStatus::Warn,
            b"$<TARGET_PDB_FILE_BASE_NAME:sh,POSTFIX:INCLUDE>".as_slice(),
            true,
        ),
        (
            rcmake_diag::PolicyStatus::Warn,
            b"$<TARGET_PDB_FILE_BASE_NAME:sh,POSTFIX:EXCLUDE>".as_slice(),
            true,
        ),
        (
            rcmake_diag::PolicyStatus::Old,
            b"$<TARGET_PDB_FILE_BASE_NAME:sh,POSTFIX:INCLUDE>".as_slice(),
            false,
        ),
        (
            rcmake_diag::PolicyStatus::Old,
            b"$<TARGET_PDB_FILE_BASE_NAME:sh,POSTFIX:EXCLUDE>".as_slice(),
            false,
        ),
    ] {
        let mut warn_model = model.clone();
        warn_model
            .target_mut(lib_id)
            .ok_or("lib mut")?
            .policies
            .policies
            .set(rcmake_diag::PolicyId::new(202), status)?;
        let warn_host = rcmake_host::PureHost::default();
        let warn_paths = crate::sources::SourcePaths::new(&warn_host);
        let mut warn_usage =
            crate::usage::UsageResolver::with_source_paths(&warn_model, &warn_paths);
        let _ = rcmake_genex::evaluate(
            &rcmake_genex::parse(expr)?,
            &ctx,
            &mut warn_usage.host(DirId::ROOT),
        )?;
        let stderr = warn_host
            .stream_bytes(rcmake_host::Stream::Stderr)
            .to_string();
        let count = stderr.matches("Policy CMP0202 is not set").count();
        assert_eq!(
            count,
            usize::from(expect_warn),
            "CMP0202 {status:?} warning count mismatch for {}: {stderr}",
            BString::from(expr)
        );
    }

    // Empty CMAKE_IMPORT_LIBRARY_SUFFIX produces no import library.
    let mut no_imp = model.clone();
    var(&mut no_imp, "CMAKE_IMPORT_LIBRARY_SUFFIX", "");
    let imp_usage = crate::usage::UsageResolver::new(&no_imp);
    assert_eq!(
        imp_usage.artifact(
            lib_id,
            rcmake_genex::ArtifactKind::ImportFile,
            rcmake_genex::ArtifactPart::Full,
            &ConfigId("Debug".into()),
        ),
        None
    );

    // CMP0141 diagnoses unknown nonempty MSVC_DEBUG_INFORMATION_FORMAT on MSVC ABI.
    let mut bad_dbg = model.clone();
    bad_dbg
        .target_mut(lib_id)
        .ok_or("lib mut")?
        .props
        .set("MSVC_DEBUG_INFORMATION_FORMAT".into(), "BogusFormat".into());
    assert!(crate::resolve(&bad_dbg, &host, &[ConfigId("Debug".into())]).is_err());

    // CMP0184 diagnoses unknown MSVC_RUNTIME_CHECKS on MSVC ABI, honors empty override.
    let mut bad_rtc = model.clone();
    bad_rtc
        .target_mut(lib_id)
        .ok_or("lib mut")?
        .props
        .set("MSVC_RUNTIME_CHECKS".into(), "BogusCheck".into());
    assert!(crate::resolve(&bad_rtc, &host, &[ConfigId("Debug".into())]).is_err());
    let mut empty_rtc = model.clone();
    empty_rtc
        .target_mut(lib_id)
        .ok_or("lib mut")?
        .props
        .set("MSVC_RUNTIME_CHECKS".into(), "".into());
    assert!(crate::resolve(&empty_rtc, &host, &[ConfigId("Debug".into())]).is_ok());

    // PDB_NAME recursion through OUTPUT_NAME reports `PDB_NAME depends on itself`.
    let mut cyc = model.clone();
    let t = cyc.target_mut(lib_id).ok_or("lib mut")?;
    t.props.set("PDB_NAME".into(), "".into());
    t.props.set(
        "OUTPUT_NAME".into(),
        "$<TARGET_PDB_FILE_BASE_NAME:sh>".into(),
    );
    let mut cyc_usage = crate::usage::UsageResolver::new(&cyc);
    let err = rcmake_genex::evaluate(
        &rcmake_genex::parse(b"$<TARGET_PDB_FILE_BASE_NAME:sh>")?,
        &ctx,
        &mut cyc_usage.host(DirId::ROOT),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("PDB_NAME depends on itself"),
        "unexpected cycle error: {err}"
    );
    Ok(())
}

#[test]
fn cluster6_and_7_stdlib_cmp0155_bmi_abs_headers_soxr_unity_and_vs_short() -> TestResult {
    // Cluster 6.3: Windows `/lib/bmi` recognized as absolute destination.
    let mut bmi_model = fixture("Windows");
    bmi_model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let mut mod_t = raw_target("m", TargetKind::StaticLibrary);
    mod_t.sources.push(bt("a.cxx"));
    let mod_id = bmi_model.add_target(mod_t)?;
    bmi_model.add_file_set(rcmake_model::FileSet {
        target: mod_id,
        name: "mods".into(),
        kind: rcmake_model::FileSetKind::CxxModules,
        scope: rcmake_model::UsageScope::Public,
        base_dirs: vec![bt("/project")],
        files: vec![bt("a.cxx")],
        props: Default::default(),
    })?;
    bmi_model.add_install(rcmake_model::InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: rcmake_model::InstallKind::Targets {
            targets: vec![mod_id],
            export: None,
        },
        options: rcmake_model::InstallOptions::default(),
    });
    for (idx, arg) in ["TARGETS", "CXX_MODULES_BMI", "DESTINATION", "/lib/bmi"]
        .into_iter()
        .enumerate()
    {
        bmi_model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_0_ARG{idx}").into(), arg.into());
    }
    bmi_model.global.props.set(
        "_RCMAKE_INSTALL_0_ABSOLUTE_ACTION".into(),
        "FATAL_ERROR".into(),
    );
    let host = rcmake_host::PureHost::default();
    assert!(
        crate::InstallManifest::validate_bmi_destinations(
            &bmi_model,
            &[ConfigId::default()],
            &host,
        )
        .is_err()
    );

    // Cluster 7.1: Empty recorded DESTINATION for FILE_SET HEADERS falls back to generic/default.
    let mut fs_model = fixture("Linux");
    fs_model.add_install(rcmake_model::InstallRule {
        dir: DirId::ROOT,
        backtrace: Default::default(),
        kind: rcmake_model::InstallKind::Targets {
            targets: vec![],
            export: None,
        },
        options: rcmake_model::InstallOptions::default(),
    });
    for (idx, arg) in ["TARGETS", "FILE_SET", "HEADERS", "DESTINATION", ""]
        .into_iter()
        .enumerate()
    {
        fs_model
            .global
            .props
            .set(format!("_RCMAKE_INSTALL_0_ARG{idx}").into(), arg.into());
    }
    let opts = crate::file_sets::options_for_set(&fs_model, 0, b"HEADERS").ok_or("opts")?;
    assert_eq!(opts.destination, None);

    // Cluster 7.2: Extensionless source property record (`pffft64s`) binds to only one source file.
    let mut soxr = fixture("Linux");
    let mut old_p = rcmake_diag::PolicyMap::default();
    old_p.set(
        rcmake_diag::PolicyId::new(115),
        rcmake_diag::PolicyStatus::Old,
    )?;
    soxr.dir_mut(DirId::ROOT).final_policies = Some(old_p);
    soxr.dir_mut(DirId::ROOT)
        .source_props
        .entry("/project/pffft64s".into())
        .or_default()
        .set("COMPILE_FLAGS".into(), "-O3".into());
    let mut soxr_lib = raw_target("soxr", TargetKind::StaticLibrary);
    soxr_lib.sources.push(bt("pffft64s.c"));
    soxr_lib.sources.push(bt("pffft64s.cpp"));
    soxr.add_target(soxr_lib)?;
    host.seed_file(
        std::path::Path::new("/project/pffft64s.c"),
        b"int p(){return 0;}",
    )?;
    host.seed_file(
        std::path::Path::new("/project/pffft64s.cpp"),
        b"int q(){return 1;}",
    )?;
    let soxr_res = crate::resolve(&soxr, &host, &[])?;
    let soxr_tgt = &soxr_res.targets[0];
    let c_unit = soxr_tgt
        .compile_units
        .iter()
        .find(|u| u.source.value == "/project/pffft64s.c")
        .ok_or("missing pffft64s.c")?;
    let cpp_unit = soxr_tgt
        .compile_units
        .iter()
        .find(|u| u.source.value == "/project/pffft64s.cpp")
        .ok_or("missing pffft64s.cpp")?;
    assert!(
        soxr_tgt.compile_groups[c_unit.group]
            .flags
            .iter()
            .any(|f| f.value == "SHELL:-O3"),
        "extensionless COMPILE_FLAGS must apply to pffft64s.c: {:?}",
        soxr_tgt.compile_groups[c_unit.group].flags
    );
    assert!(
        !soxr_tgt.compile_groups[cpp_unit.group]
            .flags
            .iter()
            .any(|f| f.value == "SHELL:-O3"),
        "extensionless COMPILE_FLAGS must not also apply to pffft64s.cpp: {:?}",
        soxr_tgt.compile_groups[cpp_unit.group].flags
    );

    // Cluster 7.4 & 7.5: VS SHORT mode uses CMakeFiles/<hash> for PCH & Unity,
    // and IdeFacts exposes member_sources per unity batch.
    let mut vs_short = fixture("Windows");
    vs_short.generator = Some(rcmake_model::GeneratorKind::VisualStudio(
        rcmake_model::VsVersion::Vs17,
    ));
    vs_short.cache.insert(
        "CMAKE_INTERMEDIATE_DIR_STRATEGY".into(),
        rcmake_model::CacheEntry {
            value: "SHORT".into(),
            ty: rcmake_model::CacheType::String,
            help: Default::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    var(
        &mut vs_short,
        "CMAKE_C_COMPILE_OPTIONS_CREATE_PCH",
        "/Yc<PCH_HEADER>;/Fp<PCH_FILE>",
    );
    var(
        &mut vs_short,
        "CMAKE_C_COMPILE_OPTIONS_USE_PCH",
        "/Yu<PCH_HEADER>;/Fp<PCH_FILE>",
    );
    let mut u = raw_target("utgt", TargetKind::StaticLibrary);
    u.props.set("UNITY_BUILD".into(), "ON".into());
    u.usage
        .push(rcmake_model::UsageField::PrecompileHeaders, bt("<stdio.h>"));
    u.sources.push(bt("u1.c"));
    u.sources.push(bt("u2.c"));
    vs_short.add_target(u)?;
    host.seed_file(
        std::path::Path::new("/project/u1.c"),
        b"int u1(){return 1;}",
    )?;
    host.seed_file(
        std::path::Path::new("/project/u2.c"),
        b"int u2(){return 2;}",
    )?;
    let vs_res = crate::resolve_with_options(
        &vs_short,
        &host,
        &[ConfigId("Debug".into())],
        &crate::ResolveOptions {
            ide: Some(crate::ide::IdeFlavor::VisualStudio),
            ..Default::default()
        },
    )?;
    let ide = vs_res.targets[0].ide.as_ref().ok_or("missing ide")?;
    assert_eq!(ide.unity_files.len(), 1);
    assert!(
        ide.unity_files[0]
            .path
            .windows(12)
            .any(|w| w == b"/CMakeFiles/"),
        "VS SHORT unity dir must use CMakeFiles/<hash>: {}",
        ide.unity_files[0].path
    );
    assert_eq!(
        ide.unity_files[0].member_sources,
        vec![
            BString::from("/project/u1.c"),
            BString::from("/project/u2.c")
        ]
    );
    assert!(
        ide.pch
            .iter()
            .all(|pch| pch.header.windows(12).any(|w| w == b"/CMakeFiles/")),
        "VS SHORT PCH header must use CMakeFiles/<hash>: {:?}",
        ide.pch
    );
    Ok(())
}
