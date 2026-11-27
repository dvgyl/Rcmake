use std::path::Path;

use rcmake_host::{PureHost, ReadRange};

use super::*;

#[test]
fn error_index_preserves_successful_reply() -> Result<(), Box<dyn std::error::Error>> {
    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let api = build.join(API_DIR);
    let query = api.join("query");
    let reply = api.join("reply");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    host.create_dir_all(&query)?;
    for query_file in ["cache-v2", "configureLog-v1"] {
        host.write(&query.join(query_file), b"", WriteMode::CreateNew)?;
    }

    let result = GenerationResult::default();
    write_replies(&model, &result, &host, build)?;
    let mut saved = BTreeMap::new();
    let range = || ReadRange::All { limit: 65_536 };
    for entry in host.read_dir(&reply)? {
        let path = reply.join(to_native(&entry.name));
        saved.insert(path.clone(), host.read(&path, range())?);
    }
    assert!(saved.keys().any(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("index-"))
    }));

    write_error_replies(&model, &host, build)?;
    for (path, data) in saved {
        assert_eq!(host.read(&path, range())?, data);
    }
    let name = host
        .read_dir(&reply)?
        .into_iter()
        .find(|entry| entry.name.starts_with(b"error-"))
        .ok_or("Missing error index.")?
        .name;
    let bytes = host.read(&reply.join(to_native(&name)), range())?;
    let Object(fields) = rcmake_json::parse(&bytes, &Default::default())? else {
        return Err("Expected an error index object.".into());
    };
    let Some(Object(replies)) = fields.get(b"reply".as_slice()) else {
        return Err("Missing query replies.".into());
    };
    assert_eq!(
        replies.get(b"cache-v2".as_slice()),
        Some(&queries::error("no buildsystem generated"))
    );
    let Some(Object(configure_log)) = replies.get(b"configureLog-v1".as_slice()) else {
        return Err("Missing configure log reply.".into());
    };
    assert!(configure_log.contains_key(b"jsonFile".as_slice()));
    assert!(
        host.metadata(
            &build.join("CMakeFiles/CMakeConfigureLog.yaml"),
            Follow::Yes
        )
        .is_ok()
    );

    write_replies(&model, &result, &host, build)?;
    assert!(
        !host
            .read_dir(&reply)?
            .iter()
            .any(|entry| entry.name.starts_with(b"error-"))
    );
    Ok(())
}

#[test]
fn codemodel_preserves_multi_element_source_options_and_multi_dir_header_suppression()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyMap};
    use rcmake_model::{
        BT, FileSet, FileSetKind, GeneratorKind, PropOrigin, PropScope, Target, TargetKind,
        TargetOrigin, UsageScope,
    };

    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    host.seed_file(
        Path::new("/src/shared.c"),
        b"int shared(void) { return 0; }\n",
    )?;
    host.seed_file(Path::new("/src/main.c"), b"int main(void) { return 0; }\n")?;
    let build = Path::new("/build");
    let query = build.join(API_DIR).join("query");
    let reply = build.join(API_DIR).join("reply");
    host.create_dir_all(&query)?;
    host.write(&query.join("codemodel-v2"), b"", WriteMode::CreateNew)?;

    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    model.set_root_dirs("/src".into(), "/build".into());
    for (name, value) in [
        ("CMAKE_SYSTEM_NAME", "Darwin"),
        ("CMAKE_C_COMPILER", "/usr/bin/cc"),
        ("CMAKE_C_COMPILER_ID", "AppleClang"),
        ("CMAKE_CONFIGURATION_TYPES", "Debug;Release"),
    ] {
        model.global.props.set(name.into(), value.into());
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(name.into(), value.into());
    }

    let bt = |file: &[u8], line: u32, cmd: &[u8]| {
        Backtrace::default().push(Frame {
            file: file.into(),
            line,
            name: Some(cmd.into()),
            kind: FrameKind::Command,
        })
    };
    let root_list = b"/src/CMakeLists.txt";
    let sub_list = b"/src/sub/CMakeLists.txt";
    let sub_dir = model.add_directory(
        DirId::ROOT,
        "/src/sub".into(),
        "/build/sub".into(),
        bt(root_list, 2, b"add_subdirectory"),
    )?;
    model
        .dir_mut(sub_dir)
        .vars
        .insert("CMAKE_C_COMPILER".into(), "/usr/bin/cc".into());

    let root_app = model.add_target(Target::new(
        "root_app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        bt(root_list, 5, b"add_executable"),
        PolicyMap::default(),
    ))?;
    let root_target = model.target_mut(root_app).ok_or("missing root_app")?;
    root_target.sources.push(BT::new(
        "main.c".into(),
        bt(root_list, 5, b"add_executable"),
    ));
    root_target.sources.push(BT::new(
        "shared.c".into(),
        bt(root_list, 5, b"add_executable"),
    ));
    let headers_set = model.add_file_set(FileSet {
        target: root_app,
        name: "HEADERS".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![BT::new("/src".into(), bt(root_list, 6, b"target_sources"))],
        files: vec![BT::new(
            "$<$<CONFIG:Debug>:/src/shared.c>".into(),
            bt(root_list, 6, b"target_sources"),
        )],
        props: Default::default(),
    })?;
    model
        .target_mut(root_app)
        .ok_or("missing root_app")?
        .file_sets
        .push(headers_set);

    let opt_bt = bt(root_list, 11, b"set_property");
    model.set_prop(
        &PropScope::Source {
            dir: DirId::ROOT,
            path: "/src/main.c".into(),
        },
        b"COMPILE_OPTIONS",
        Some("-DOPT_A=1;-DOPT_B=two words".into()),
        PropOrigin {
            dir: DirId::ROOT,
            backtrace: &opt_bt,
        },
    )?;

    let sub_lib = model.add_target(Target::new(
        "sub_lib".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        sub_dir,
        bt(sub_list, 3, b"add_library"),
        PolicyMap::default(),
    ))?;
    model
        .target_mut(sub_lib)
        .ok_or("missing sub_lib")?
        .sources
        .push(BT::new(
            "/src/shared.c".into(),
            bt(sub_list, 3, b"add_library"),
        ));

    let configs = vec![
        rcmake_genex::ConfigId("Debug".into()),
        rcmake_genex::ConfigId("Release".into()),
    ];
    let result = rcmake_resolve::resolve(&model, &host, &configs)?;
    write_replies(&model, &result, &host, build)?;

    let range = || ReadRange::All { limit: 4_000_000 };
    let index_name = host
        .read_dir(&reply)?
        .into_iter()
        .find(|entry| entry.name.starts_with(b"index-"))
        .ok_or("missing index")?
        .name;
    let index_json = rcmake_json::parse(
        &host.read(&reply.join(to_native(&index_name)), range())?,
        &Default::default(),
    )?;
    let Object(index_fields) = &index_json else {
        return Err("expected index object".into());
    };
    let Some(Object(replies)) = index_fields.get(b"reply".as_slice()) else {
        return Err("missing reply".into());
    };
    let Some(Object(cm_ref)) = replies.get(b"codemodel-v2".as_slice()) else {
        return Err("missing codemodel-v2".into());
    };
    let Some(Value::String(cm_file)) = cm_ref.get(b"jsonFile".as_slice()) else {
        return Err("missing codemodel jsonFile".into());
    };
    let codemodel = rcmake_json::parse(
        &host.read(&reply.join(to_native(cm_file)), range())?,
        &Default::default(),
    )?;
    let Object(cm_fields) = &codemodel else {
        return Err("expected codemodel object".into());
    };
    let Some(Array(cfg_entries)) = cm_fields.get(b"configurations".as_slice()) else {
        return Err("missing configurations".into());
    };
    assert_eq!(cfg_entries.len(), 2);

    for cfg in cfg_entries {
        let Object(cfg_obj) = cfg else {
            return Err("expected config object".into());
        };
        let Some(Array(targets)) = cfg_obj.get(b"targets".as_slice()) else {
            return Err("missing targets".into());
        };
        for target_ref in targets {
            let Object(tref) = target_ref else { continue };
            let Some(Value::String(name)) = tref.get(b"name".as_slice()) else {
                continue;
            };
            let Some(Value::String(tfile)) = tref.get(b"jsonFile".as_slice()) else {
                continue;
            };
            let target_json = rcmake_json::parse(
                &host.read(&reply.join(to_native(tfile)), range())?,
                &Default::default(),
            )?;
            let Object(tobj) = &target_json else {
                return Err("expected target object".into());
            };
            let Some(Array(sources)) = tobj.get(b"sources".as_slice()) else {
                return Err("missing sources".into());
            };
            if name.as_slice() == b"root_app" {
                let shared_entry = sources
                    .iter()
                    .find_map(|s| match s {
                        Object(fields)
                            if matches!(fields.get(b"path".as_slice()), Some(Value::String(p)) if p.as_slice() == b"shared.c") =>
                        {
                            Some(fields)
                        }
                        _ => None,
                    })
                    .ok_or("missing shared.c in root_app")?;
                assert!(
                    !shared_entry.contains_key(b"compileGroupIndex".as_slice()),
                    "shared.c in root_app should be suppressed as header-only in both Debug and Release"
                );
                let Some(Array(groups)) = tobj.get(b"compileGroups".as_slice()) else {
                    return Err("missing compileGroups in root_app".into());
                };
                let Some(Object(group0)) = groups.first() else {
                    return Err("missing group 0".into());
                };
                let Some(Array(frags)) = group0.get(b"compileCommandFragments".as_slice()) else {
                    return Err("missing compileCommandFragments".into());
                };
                let multi = frags
                    .iter()
                    .find_map(|f| match f {
                        Object(fields) => match fields.get(b"fragment".as_slice()) {
                            Some(Value::String(text))
                                if text.as_slice().contains(&b'O')
                                    && text.starts_with(b"-DOPT_A=1") =>
                            {
                                Some(fields)
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .ok_or("missing joined multi-element source option fragment")?;
                assert_eq!(
                    multi.get(b"fragment".as_slice()),
                    Some(&Value::String(
                        b"-DOPT_A=1 \"-DOPT_B=two words\"".as_slice().into()
                    ))
                );
                assert!(multi.contains_key(b"backtrace".as_slice()));
            } else if name.as_slice() == b"sub_lib" {
                let shared_entry = sources
                    .iter()
                    .find_map(|s| match s {
                        Object(fields)
                            if matches!(fields.get(b"path".as_slice()), Some(Value::String(p)) if p.as_slice() == b"shared.c") =>
                        {
                            Some(fields)
                        }
                        _ => None,
                    })
                    .ok_or("missing shared.c in sub_lib")?;
                assert!(
                    shared_entry.contains_key(b"compileGroupIndex".as_slice()),
                    "shared.c in sub_lib must still compile in another directory"
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn codemodel_file_set_membership_preserves_literal_backslashes_and_semicolons()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{
        BT, FileSet, FileSetKind, GeneratorKind, Target, TargetKind, TargetOrigin, UsageScope,
    };

    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    host.seed_file(Path::new("/src/a.c"), b"int a(void) { return 0; }\n")?;
    host.seed_file(Path::new(r"/src/dir/a\b.h"), b"")?;
    host.seed_file(Path::new(r"/src/dir/a\;b.h"), b"")?;
    host.seed_file(Path::new("/src/dir/h;1.h"), b"")?;
    host.seed_file(Path::new("/src/dir/p;1.h"), b"")?;

    let build = Path::new("/build");
    let query = build.join(API_DIR).join("query");
    let reply = build.join(API_DIR).join("reply");
    host.create_dir_all(&query)?;
    host.write(&query.join("codemodel-v2"), b"", WriteMode::CreateNew)?;

    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("/src".into(), "/build".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_C_COMPILER".into(), "/usr/bin/cc".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_C_SOURCE_FILE_EXTENSIONS".into(), "c".into());
    let mut lib = Target::new(
        "lib".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    lib.sources.push(BT::new("a.c".into(), Default::default()));
    // Target SOURCES applies a single list split, decoding to `dir/a\b.h`,
    // `dir/a\;b.h` (literal backslash before semicolon), and `dir/h;1.h`.
    lib.sources
        .push(BT::new(r"dir/a\b.h".into(), Default::default()));
    lib.sources
        .push(BT::new(r"dir/a\\;b.h".into(), Default::default()));
    lib.sources
        .push(BT::new(r"dir/h\;1.h".into(), Default::default()));
    let lib_id = model.add_target(lib)?;
    model.add_file_set(FileSet {
        target: lib_id,
        name: "HEADERS".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Interface,
        base_dirs: vec![BT::new("/src/dir".into(), Default::default())],
        // EvaluateFileEntry performs exactly two list splits:
        // - `a\b.h` -> `a\b.h`
        // - `a\\\;b.h` -> `a\;b.h` (a 3rd split would strip the literal `\`)
        // - `h\\;1.h` -> `h;1.h` (a 3rd split would split into `h` and `1.h`)
        files: vec![
            BT::new(r"/src/dir/a\b.h".into(), Default::default()),
            BT::new(r"/src/dir/a\\\;b.h".into(), Default::default()),
            BT::new(r"/src/dir/h\\;1.h".into(), Default::default()),
        ],
        props: Default::default(),
    })?;
    // A PRIVATE file set enters target SOURCES through FileSetPropertyEntry
    // (3rd split), so 3 backslashes decode to `dir/p;1.h` during target-source
    // resolution (matching CMake's FileSetSemicolon).
    model.add_file_set(FileSet {
        target: lib_id,
        name: "priv_headers".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![BT::new("/src/dir".into(), Default::default())],
        files: vec![BT::new(r"/src/dir/p\\\;1.h".into(), Default::default())],
        props: Default::default(),
    })?;

    let result = rcmake_resolve::resolve(&model, &host, &[])?;
    write_replies(&model, &result, &host, build)?;

    let range = || ReadRange::All { limit: 131_072 };
    let target_json = host
        .read_dir(&reply)?
        .into_iter()
        .find(|entry| entry.name.starts_with(b"target-lib-"))
        .ok_or("missing target-lib reply")?;
    let bytes = host.read(&reply.join(to_native(&target_json.name)), range())?;
    let Object(fields) = rcmake_json::parse(&bytes, &Default::default())? else {
        return Err("expected target object".into());
    };
    let Some(Value::Array(sources)) = fields.get(b"sources".as_slice()) else {
        return Err("missing sources array".into());
    };
    let find_source = |path: &[u8]| {
        sources.iter().find_map(|entry| match entry {
            Object(obj)
                if matches!(obj.get(b"path".as_slice()), Some(Value::String(p)) if p.as_slice() == path) =>
            {
                Some(obj)
            }
            _ => None,
        })
    };
    let backslash = find_source(br"dir/a\b.h").ok_or("missing dir/a\\b.h")?;
    assert_eq!(
        backslash.get(b"fileSetIndex".as_slice()),
        Some(&Value::Int(0))
    );
    let escaped_backslash_semi = find_source(br"dir/a\;b.h").ok_or("missing dir/a\\;b.h")?;
    assert_eq!(
        escaped_backslash_semi.get(b"fileSetIndex".as_slice()),
        Some(&Value::Int(0))
    );
    let semicolon = find_source(b"dir/h;1.h").ok_or("missing dir/h;1.h")?;
    assert_eq!(
        semicolon.get(b"fileSetIndex".as_slice()),
        Some(&Value::Int(0))
    );
    assert!(find_source(b"dir/h").is_none());
    assert!(find_source(b"1.h").is_none());
    Ok(())
}

#[test]
fn codemodel_wave_c2_backtraces_artifacts_dependencies_installers_and_source_groups()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyMap};
    use rcmake_model::{
        BT, CustomCommand, CustomCommandKind, ExportSet, FileSet, FileSetKind, GeneratorKind,
        ImportedVisibility, InstallKind, InstallOptions, InstallRule, SourceGroupDef, Target,
        TargetKind, TargetOrigin, UsageScope,
    };
    use rcmake_resolve::{ArtifactSet, CompileUnit, ResolvedTarget, TargetMetadata};

    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let query = build.join(API_DIR).join("query");
    let reply = build.join(API_DIR).join("reply");
    host.create_dir_all(&query)?;
    host.write(&query.join("codemodel-v2"), b"", WriteMode::CreateNew)?;

    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("/src".into(), "/build".into());
    let sub_dir = model.add_directory(
        DirId::ROOT,
        "/src/sub".into(),
        "/build/sub".into(),
        Backtrace::default(),
    )?;

    // Multi-frame macro backtrace: outermost CMakeLists.txt:10 -> innermost macros.cmake:3.
    let macro_bt = Backtrace::default()
        .push(Frame {
            file: b"/src/CMakeLists.txt".as_slice().into(),
            line: 10,
            name: Some(b"my_add".as_slice().into()),
            kind: FrameKind::Command,
        })
        .push(Frame {
            file: b"/src/macros.cmake".as_slice().into(),
            line: 3,
            name: Some(b"add_executable".as_slice().into()),
            kind: FrameKind::Command,
        });

    // Directory source_groups in creation order (defaults first, user groups after).
    {
        let root_dir = model.dir_mut(DirId::ROOT);
        root_dir.source_groups = vec![
            SourceGroupDef {
                name: "Source Files".into(),
                regex: Some(br"\.c$|\.cpp$".as_slice().into()),
                files: ["/src/gen.c".into()].into_iter().collect(),
                children: Vec::new(),
            },
            SourceGroupDef {
                name: "Generated".into(),
                regex: None,
                files: ["/src/gen.c".into()].into_iter().collect(),
                children: Vec::new(),
            },
        ];
    }

    // Two object libraries (created in reverse ID order) to verify objectDependencies string sort.
    let obj_z = model.add_target(Target::new(
        "obj_z".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    let obj_a = model.add_target(Target::new(
        "obj_a".into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    let imp_tool = model.add_target(Target::new(
        "Imp::Tool".into(),
        TargetKind::Executable,
        TargetOrigin::Imported(ImportedVisibility::Global),
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;

    // Versioned executable (`clang-20` with linker_file `clang`) + ./ collapsed artifact +
    // self/imported/reserved utility_deps + TARGET_OBJECTS.
    let mut exe_raw = Target::new(
        "clang".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        macro_bt.clone(),
        PolicyMap::default(),
    );
    exe_raw
        .sources
        .push(BT::new("gen.c".into(), macro_bt.clone()));
    exe_raw.sources.push(BT::new(
        "$<TARGET_OBJECTS:obj_z>".into(),
        Backtrace::default(),
    ));
    exe_raw.sources.push(BT::new(
        "$<TARGET_OBJECTS:obj_a>".into(),
        Backtrace::default(),
    ));
    exe_raw
        .utility_deps
        .push(BT::new("clang".into(), Backtrace::default()));
    exe_raw
        .utility_deps
        .push(BT::new("Imp::Tool".into(), Backtrace::default()));
    exe_raw
        .utility_deps
        .push(BT::new("all".into(), Backtrace::default()));
    exe_raw
        .utility_deps
        .push(BT::new("install".into(), Backtrace::default()));
    let exe_id = model.add_target(exe_raw)?;

    // Utility target whose name ends in `.cpp` (C12) + empty utility target (C8).
    let util_cpp = model.add_target(Target::new(
        "clang-format-x.cpp".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    let mut util_cmd = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(util_cpp),
        Backtrace::default(),
        PolicyMap::default(),
    );
    util_cmd
        .command_lines
        .push(vec!["echo".into(), "ok".into()]);
    model.add_command(util_cmd)?;

    let empty_util = model.add_target(Target::new(
        "empty_util".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    model.add_command(CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::Utility(empty_util),
        Backtrace::default(),
        PolicyMap::default(),
    ))?;

    // Abstract INTERFACE target with base_dirs in reverse-alpha declaration order
    // (`/src/zzz` before `/build/aaa`), exported from `sub_dir`, with FILE_SET and BMI install.
    let mut iface = Target::new(
        "iface_lib".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        sub_dir,
        Backtrace::default(),
        PolicyMap::default(),
    );
    iface.usage.link_libraries = Some(vec![rcmake_model::LinkItem {
        opens_lookup_scope: false,
        closes_lookup_scope: false,
        scope: None,
        text: BT::new("clang".into(), Backtrace::default()),
        lookup_dir: sub_dir,
    }]);
    let iface_id = model.add_target(iface)?;
    let _fs_id = model.add_file_set(FileSet {
        target: iface_id,
        name: "HEADERS".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Interface,
        base_dirs: vec![
            BT::new("/src/zzz".into(), Backtrace::default()),
            BT::new("/build/aaa".into(), Backtrace::default()),
        ],
        files: vec![
            BT::new("/src/zzz/one.h".into(), Backtrace::default()),
            BT::new("/build/aaa/two.h".into(), Backtrace::default()),
        ],
        props: Default::default(),
    })?;

    model.exports.insert(
        "MyExp".into(),
        ExportSet {
            name: "MyExp".into(),
            targets: vec![exe_id, iface_id],
            install_rules: Vec::new(),
        },
    );
    // Rule 0: install(TARGETS iface_lib FILE_SET HEADERS DESTINATION include/ CXX_MODULES_BMI DESTINATION bmi/)
    let rule_idx = model.installs.len();
    for (arg_idx, arg) in [
        "TARGETS",
        "iface_lib",
        "FILE_SET",
        "HEADERS",
        "DESTINATION",
        "include/",
        "CXX_MODULES_BMI",
        "DESTINATION",
        "bmi/",
    ]
    .iter()
    .enumerate()
    {
        model.global.props.set(
            format!("_RCMAKE_INSTALL_{rule_idx}_ARG{arg_idx}").into(),
            (*arg).into(),
        );
    }
    model.add_install(InstallRule {
        dir: sub_dir,
        backtrace: Backtrace::default(),
        kind: InstallKind::Targets {
            targets: vec![iface_id],
            export: Some("MyExp".into()),
        },
        options: InstallOptions::default(),
    });
    // Rule 1: install(EXPORT MyExp DESTINATION lib/cmake/) in sub_dir
    model.add_install(InstallRule {
        dir: sub_dir,
        backtrace: Backtrace::default(),
        kind: InstallKind::Export {
            set: "MyExp".into(),
            file: None,
            namespace: BString::default(),
            cxx_modules_directory: None,
        },
        options: InstallOptions {
            destination: Some("lib/cmake/".into()),
            ..InstallOptions::default()
        },
    });
    // Rule 2: install(FILES $<$<CONFIG:Debug>:/src/only_debug.txt>) — empty in Release, must drop.
    model.add_install(InstallRule {
        dir: sub_dir,
        backtrace: Backtrace::default(),
        kind: InstallKind::Files(vec![BT::new(
            "$<$<CONFIG:Debug>:/src/only_debug.txt>".into(),
            Backtrace::default(),
        )]),
        options: InstallOptions {
            destination: Some("share/x/".into()),
            ..InstallOptions::default()
        },
    });
    // Rule 3: install(FILES $<$<CONFIG:Release>:/src/gen_rel.txt>) — kept and evaluated in Release.
    model.add_install(InstallRule {
        dir: sub_dir,
        backtrace: Backtrace::default(),
        kind: InstallKind::Files(vec![BT::new(
            "$<$<CONFIG:Release>:/src/gen_rel.txt>".into(),
            Backtrace::default(),
        )]),
        options: InstallOptions {
            destination: Some("share/y/".into()),
            ..InstallOptions::default()
        },
    });

    let release = rcmake_genex::ConfigId("Release".into());
    let mk_target = |id, name: &str, kind, backtrace, artifacts, compile_units| ResolvedTarget {
        id,
        config: release.clone(),
        metadata: TargetMetadata {
            name: name.into(),
            kind,
            source_dir: "/src".into(),
            binary_dir: "/build".into(),
            backtrace,
            folder: None,
            exclude_from_all: false,
            generator_provided: false,
        },
        artifacts,
        compile_groups: Vec::new(),
        compile_units,
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
        fortran: None,
        ide: None,
    };
    let result = GenerationResult {
        configs: vec![release.clone()],
        targets: vec![
            mk_target(
                obj_z,
                "obj_z",
                TargetKind::ObjectLibrary,
                Backtrace::default(),
                ArtifactSet::default(),
                vec![CompileUnit {
                    source: BT::new("/src/z.c".into(), Backtrace::default()),
                    object: "/build/CMakeFiles/obj_z.dir/./z.c.o".into(),
                    ..Default::default()
                }],
            ),
            mk_target(
                obj_a,
                "obj_a",
                TargetKind::ObjectLibrary,
                Backtrace::default(),
                ArtifactSet::default(),
                vec![CompileUnit {
                    source: BT::new("/src/a.c".into(), Backtrace::default()),
                    object: "/build/CMakeFiles/obj_a.dir/./a.c.o".into(),
                    ..Default::default()
                }],
            ),
            mk_target(
                exe_id,
                "clang",
                TargetKind::Executable,
                macro_bt,
                ArtifactSet {
                    file: Some("/build/bin/./clang-20".into()),
                    linker_file: Some("/build/bin/./clang".into()),
                    ..Default::default()
                },
                Vec::new(),
            ),
            mk_target(
                util_cpp,
                "clang-format-x.cpp",
                TargetKind::Utility,
                Backtrace::default(),
                ArtifactSet::default(),
                Vec::new(),
            ),
            mk_target(
                empty_util,
                "empty_util",
                TargetKind::Utility,
                Backtrace::default(),
                ArtifactSet::default(),
                Vec::new(),
            ),
        ],
        ..Default::default()
    };
    let _ = imp_tool;
    write_replies(&model, &result, &host, build)?;

    let range = || ReadRange::All { limit: 1_000_000 };
    let read_json = |name: &[u8]| -> Result<Value, Box<dyn std::error::Error>> {
        Ok(rcmake_json::parse(
            &host.read(&reply.join(to_native(name)), range())?,
            &Default::default(),
        )?)
    };
    let index_name = host
        .read_dir(&reply)?
        .into_iter()
        .find(|entry| entry.name.starts_with(b"index-"))
        .ok_or("missing index")?
        .name;
    let index_val = read_json(&index_name)?;
    let Object(idx_obj) = &index_val else {
        return Err("expected index object".into());
    };
    let Some(Object(replies)) = idx_obj.get(b"reply".as_slice()) else {
        return Err("missing reply".into());
    };
    let Some(Object(cm_ref)) = replies.get(b"codemodel-v2".as_slice()) else {
        return Err("missing codemodel".into());
    };
    let Some(Value::String(cm_file)) = cm_ref.get(b"jsonFile".as_slice()) else {
        return Err("missing cm file".into());
    };
    let cm_val = read_json(cm_file)?;
    let Object(cm_obj) = &cm_val else {
        return Err("expected cm object".into());
    };
    let Some(Array(cfgs)) = cm_obj.get(b"configurations".as_slice()) else {
        return Err("missing configs".into());
    };
    let Some(Object(cfg)) = cfgs.first() else {
        return Err("missing cfg 0".into());
    };
    let find_ref = |list_key: &[u8], target_name: &[u8]| -> Option<&Members> {
        let Array(list) = cfg.get(list_key)? else {
            return None;
        };
        list.iter().find_map(|item| match item {
            Object(obj)
                if matches!(obj.get(b"name".as_slice()), Some(Value::String(n)) if n.as_slice() == target_name) =>
            {
                Some(obj)
            }
            _ => None,
        })
    };
    let load_target =
        |list_key: &[u8], target_name: &[u8]| -> Result<Members, Box<dyn std::error::Error>> {
            let tref = find_ref(list_key, target_name).ok_or("missing target ref")?;
            let Some(Value::String(file)) = tref.get(b"jsonFile".as_slice()) else {
                return Err("missing jsonFile".into());
            };
            let Object(obj) = read_json(file)? else {
                return Err("expected target object".into());
            };
            Ok(obj)
        };

    // 1. Versioned executable: nameOnDisk == "clang", collapsed artifact "bin/clang",
    //    multi-frame backtrace top-first (macros.cmake before CMakeLists.txt),
    //    objectDependencies sorted by id string (obj_a before obj_z),
    //    orderDependencies keeps self + imported + objects, drops reserved (all/install),
    //    source_groups reverse precedence (Generated wins over Source Files for gen.c).
    let clang = load_target(b"targets", b"clang")?;
    assert_eq!(
        clang.get(b"nameOnDisk".as_slice()),
        Some(&Value::string("clang"))
    );
    assert_eq!(
        clang.get(b"artifacts".as_slice()),
        Some(&array([object([("path", Value::string("bin/clang"))])]))
    );
    let Some(Object(bt_graph)) = clang.get(b"backtraceGraph".as_slice()) else {
        return Err("missing backtraceGraph".into());
    };
    assert_eq!(
        bt_graph.get(b"files".as_slice()),
        Some(&array([
            Value::string("macros.cmake"),
            Value::string("CMakeLists.txt"),
        ]))
    );
    assert_eq!(
        bt_graph.get(b"commands".as_slice()),
        Some(&array([
            Value::string("add_executable"),
            Value::string("my_add"),
        ]))
    );
    let Some(Array(obj_deps)) = clang.get(b"objectDependencies".as_slice()) else {
        return Err("missing objectDependencies".into());
    };
    let obj_dep_ids: Vec<&[u8]> = obj_deps
        .iter()
        .filter_map(|v| match v {
            Object(o) => match o.get(b"id".as_slice()) {
                Some(Value::String(s)) => Some(s.as_slice()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(obj_dep_ids.len(), 2);
    assert!(obj_dep_ids[0].starts_with(b"obj_a::@"));
    assert!(obj_dep_ids[1].starts_with(b"obj_z::@"));

    let Some(Array(ord_deps)) = clang.get(b"orderDependencies".as_slice()) else {
        return Err("missing orderDependencies".into());
    };
    let ord_dep_ids: Vec<&[u8]> = ord_deps
        .iter()
        .filter_map(|v| match v {
            Object(o) => match o.get(b"id".as_slice()) {
                Some(Value::String(s)) => Some(s.as_slice()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(ord_dep_ids.iter().any(|id| id.starts_with(b"clang::@")));
    assert!(ord_dep_ids.iter().any(|id| id.starts_with(b"Imp::Tool::@")));
    assert!(ord_dep_ids.iter().any(|id| id.starts_with(b"obj_a::@")));
    assert!(
        !ord_dep_ids
            .iter()
            .any(|id| id.starts_with(b"all::@") || id.starts_with(b"install::@"))
    );

    let Some(Array(sg_list)) = clang.get(b"sourceGroups".as_slice()) else {
        return Err("missing sourceGroups".into());
    };
    assert!(sg_list.iter().any(
        |v| matches!(v, Object(o) if o.get(b"name".as_slice()) == Some(&Value::string("Generated")))
    ));
    assert!(sg_list.iter().any(|v| matches!(v, Object(o) if o.get(b"name".as_slice()) == Some(&Value::string("Object Libraries")))));

    // 2. ObjectLibrary artifact preserves interior `./`.
    let obj_a_json = load_target(b"targets", b"obj_a")?;
    assert_eq!(
        obj_a_json.get(b"artifacts".as_slice()),
        Some(&array([object([(
            "path",
            Value::string("CMakeFiles/obj_a.dir/./a.c.o"),
        )])]))
    );

    // 3. Utility `.cpp` target source matches "Source Files" (C12); empty utility has no CMakeFiles/<n> (C8).
    let util_json = load_target(b"targets", b"clang-format-x.cpp")?;
    let Some(Array(util_sgs)) = util_json.get(b"sourceGroups".as_slice()) else {
        return Err("missing util sourceGroups".into());
    };
    assert!(util_sgs.iter().any(|v| matches!(v, Object(o) if o.get(b"name".as_slice()) == Some(&Value::string("Source Files")))));
    let empty_util_json = load_target(b"targets", b"empty_util")?;
    assert_eq!(empty_util_json.get(b"sources".as_slice()), Some(&array([])));

    // 4. Abstract INTERFACE target: no `dependencies` key (C3), has `install: {destinations: []}` (2d).
    let iface_json = load_target(b"abstractTargets", b"iface_lib")?;
    assert!(!iface_json.contains_key(b"dependencies".as_slice()));
    assert!(iface_json.contains_key(b"install".as_slice()));

    // 5. Subdirectory installers: fileSet with declaration-order fileSetDirectories (2c/F5),
    //    cxxModuleBmi on abstract target (2c), export with `sub/CMakeFiles/Export/...` + index on
    //    abstract target (F5), genex FILES empty skipped & non-empty evaluated (2e), trailing `/` stripped.
    let Some(Array(dirs)) = cfg.get(b"directories".as_slice()) else {
        return Err("missing directories".into());
    };
    let Some(Object(sub_ref)) = dirs.get(1) else {
        return Err("missing sub dir".into());
    };
    let Some(Value::String(sub_file)) = sub_ref.get(b"jsonFile".as_slice()) else {
        return Err("missing sub jsonFile".into());
    };
    let Object(sub_obj) = read_json(sub_file)? else {
        return Err("expected sub dir object".into());
    };
    let Some(Array(installers)) = sub_obj.get(b"installers".as_slice()) else {
        return Err("missing installers".into());
    };
    // Should have: fileSet, cxxModuleBmi, export, file (Release only; Debug skipped!).
    assert_eq!(installers.len(), 4, "installers={installers:?}");
    let Object(fs_inst) = &installers[0] else {
        return Err("expected fileSet installer".into());
    };
    assert_eq!(
        fs_inst.get(b"type".as_slice()),
        Some(&Value::string("fileSet"))
    );
    assert_eq!(
        fs_inst.get(b"destination".as_slice()),
        Some(&Value::string("include"))
    );
    assert_eq!(
        fs_inst.get(b"fileSetDirectories".as_slice()),
        Some(&array([Value::string("zzz"), Value::string("/build/aaa"),]))
    );
    let Object(bmi_inst) = &installers[1] else {
        return Err("expected cxxModuleBmi installer".into());
    };
    assert_eq!(
        bmi_inst.get(b"type".as_slice()),
        Some(&Value::string("cxxModuleBmi"))
    );
    assert_eq!(
        bmi_inst.get(b"destination".as_slice()),
        Some(&Value::string("bmi"))
    );
    let Object(exp_inst) = &installers[2] else {
        return Err("expected export installer".into());
    };
    assert_eq!(
        exp_inst.get(b"destination".as_slice()),
        Some(&Value::string("lib/cmake"))
    );
    let Some(Array(exp_paths)) = exp_inst.get(b"paths".as_slice()) else {
        return Err("missing export paths".into());
    };
    assert!(
        matches!(exp_paths.first(), Some(Value::String(p)) if p.starts_with(b"sub/CMakeFiles/Export/"))
    );
    let Some(Array(exp_targets)) = exp_inst.get(b"exportTargets".as_slice()) else {
        return Err("missing exportTargets".into());
    };
    assert_eq!(exp_targets.len(), 2);
    for entry in exp_targets {
        let Object(obj) = entry else {
            return Err("expected export target".into());
        };
        assert!(
            obj.contains_key(b"index".as_slice()),
            "missing index in {obj:?}"
        );
    }
    let Object(file_inst) = &installers[3] else {
        return Err("expected file installer".into());
    };
    assert_eq!(
        file_inst.get(b"type".as_slice()),
        Some(&Value::string("file"))
    );
    assert_eq!(
        file_inst.get(b"destination".as_slice()),
        Some(&Value::string("share/y"))
    );
    assert_eq!(
        file_inst.get(b"paths".as_slice()),
        Some(&array([Value::string("gen_rel.txt")]))
    );
    Ok(())
}

#[test]
fn codemodel_wave_c2_target_file_installers_config_destinations_and_runtime_paths()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_diag::{Backtrace, Frame, FrameKind};
    use rcmake_json::Value::{Array, Bool, Object};
    use rcmake_model::{
        BT, DirId, GeneratorKind, InstallKind, InstallOptions, InstallRule, ProjectModel, Target,
        TargetKind, TargetOrigin,
    };
    use rcmake_resolve::{
        ArtifactSet, BuildGraph, LinkFragment, LinkFragmentRole, LinkKind, LinkRuntimePaths,
        LinkStep, NamelinkMode, ResolvedTarget, TargetFileInstaller, TargetFileKind,
        TargetInstaller, TargetMetadata,
    };

    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    model.generator = Some(GeneratorKind::Ninja);

    let lib_id = model.add_target(Target::new(
        "mylib".into(),
        TargetKind::SharedLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        Default::default(),
    ))?;
    let iface_id = model.add_target(Target::new(
        "myiface".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        Default::default(),
    ))?;

    let rule_bt = Backtrace::default().push(Frame {
        file: b"/src/CMakeLists.txt".as_slice().into(),
        line: 20,
        name: Some(b"install".as_slice().into()),
        kind: FrameKind::Command,
    });
    model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: rule_bt,
        kind: InstallKind::Targets {
            targets: vec![lib_id, iface_id],
            export: None,
        },
        options: InstallOptions::default(),
    });

    let cfg = rcmake_genex::ConfigId("Debug".into());
    let mut graph = BuildGraph {
        config: cfg.clone(),
        ..BuildGraph::default()
    };
    graph.link_runtime_paths.insert(
        "libmylib.so".into(),
        LinkRuntimePaths {
            rpath: Some(r#"-Wl,-rpath,"\$ORIGIN:/opt/lib""#.into()),
            rpath_link: Some("-Wl,-rpath-link,/opt/indirect".into()),
        },
    );

    let mut result = GenerationResult {
        configs: vec![cfg.clone()],
        graphs: vec![graph],
        targets: vec![ResolvedTarget {
            id: lib_id,
            config: cfg.clone(),
            metadata: TargetMetadata {
                name: "mylib".into(),
                kind: TargetKind::SharedLibrary,
                source_dir: "/src".into(),
                binary_dir: "/build".into(),
                backtrace: Backtrace::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet {
                file: Some("libmylib.so".into()),
                ..ArtifactSet::default()
            },
            compile_units: Vec::new(),
            compile_groups: Vec::new(),
            link: Some(LinkStep {
                kind: LinkKind::SharedLibrary,
                language: "C".into(),
                fragments: vec![
                    LinkFragment {
                        role: LinkFragmentRole::LibraryPath,
                        text: BT::new("-L/opt/lib".into(), Backtrace::default()),
                    },
                    LinkFragment {
                        role: LinkFragmentRole::Libraries,
                        text: BT::new("-lm".into(), Backtrace::default()),
                    },
                ],
                output: "libmylib.so".into(),
            }),
            dependencies: Vec::new(),
            cxx_modules: None,
            fortran: None,
            ide: None,
        }],
        ..GenerationResult::default()
    };
    // TargetInstaller has CONFIGURATIONS Release (does NOT match Debug):
    // 2f requires target's `install.destinations` to keep it while directory `installers[]` skips it.
    result.install.target_installers.push(TargetInstaller {
        rule: 0,
        target: lib_id,
        config: cfg.clone(),
        destination: "lib/release".into(),
        component: "Runtime".into(),
        optional: false,
        exclude_from_all: false,
        import_library: false,
        namelink: NamelinkMode::None,
        paths: vec![("/build/libmylib.so".into(), "libmylib.so".into())],
        configurations: vec!["Release".into()],
    });
    // 2d: have_install_rule marks abstract target `myiface` so it emits `install: {destinations: []}`.
    result.install_targets.have_install_rule.insert(lib_id);
    result.install_targets.have_install_rule.insert(iface_id);
    // 2b: PUBLIC_HEADER TargetFileInstaller emitted in directory installers[] for matching config.
    result
        .install_targets
        .file_installers
        .push(TargetFileInstaller {
            rule: 0,
            target: lib_id,
            config: cfg,
            kind: TargetFileKind::PublicHeader,
            destination: "include/mylib/".into(),
            component: "Devel".into(),
            optional: true,
            exclude_from_all: false,
            configurations: vec!["Debug".into()],
            files: vec!["/src/include/mylib.h".into()],
        });

    let api = build.join(API_DIR);
    let query = api.join("query");
    let reply = api.join("reply");
    host.create_dir_all(&query)?;
    host.write(&query.join("codemodel-v2"), b"", WriteMode::CreateNew)?;
    write_replies(&model, &result, &host, build)?;

    let read_reply = |prefix: &str| -> Result<Value, Box<dyn std::error::Error>> {
        let name = host
            .read_dir(&reply)?
            .into_iter()
            .find(|entry| entry.name.starts_with(prefix.as_bytes()))
            .ok_or_else(|| format!("missing reply file starting with {prefix}"))?
            .name;
        let bytes = host.read(
            &reply.join(to_native(&name)),
            ReadRange::All { limit: 256_000 },
        )?;
        Ok(rcmake_json::parse(&bytes, &Default::default())?)
    };

    // 2f + R5 on `mylib`.
    let Object(mylib) = read_reply("target-mylib-Debug-")? else {
        return Err("expected mylib object".into());
    };
    let Some(Object(install)) = mylib.get(b"install".as_slice()) else {
        return Err("missing mylib install".into());
    };
    let Some(Array(dests)) = install.get(b"destinations".as_slice()) else {
        return Err("missing mylib install destinations".into());
    };
    assert_eq!(dests.len(), 1);
    let Object(dest0) = &dests[0] else {
        return Err("expected destination object".into());
    };
    assert_eq!(
        dest0.get(b"path".as_slice()),
        Some(&Value::string("lib/release"))
    );

    let Some(Object(link)) = mylib.get(b"link".as_slice()) else {
        return Err("missing mylib link".into());
    };
    let Some(Array(frags)) = link.get(b"commandFragments".as_slice()) else {
        return Err("missing mylib commandFragments".into());
    };
    let pairs: Vec<_> = frags
        .iter()
        .filter_map(|f| match f {
            Object(m) => Some((
                m.get(b"role".as_slice())?.clone(),
                m.get(b"fragment".as_slice())?.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            (Value::string("libraryPath"), Value::string("-L/opt/lib")),
            (
                Value::string("libraries"),
                Value::string(r#"-Wl,-rpath,"\$$ORIGIN:/opt/lib""#)
            ),
            (Value::string("libraries"), Value::string("-lm")),
            (
                Value::string("libraries"),
                Value::string("-Wl,-rpath-link,/opt/indirect")
            ),
        ]
    );

    // 2d on abstract `myiface`.
    let Object(myiface) = read_reply("target-myiface-Debug-")? else {
        return Err("expected myiface object".into());
    };
    let Some(Object(iface_install)) = myiface.get(b"install".as_slice()) else {
        return Err("missing myiface install".into());
    };
    let Some(Array(iface_dests)) = iface_install.get(b"destinations".as_slice()) else {
        return Err("missing myiface destinations".into());
    };
    assert!(iface_dests.is_empty());

    // 2b + 2f in root directory installers: target installer filtered out by Debug, PUBLIC_HEADER kept!
    let Object(root_dir) = read_reply("directory-.-Debug-")? else {
        return Err("expected root dir object".into());
    };
    let Some(Array(insts)) = root_dir.get(b"installers".as_slice()) else {
        return Err("missing installers".into());
    };
    assert_eq!(insts.len(), 1, "insts={insts:?}");
    let Object(inst0) = &insts[0] else {
        return Err("expected installer object".into());
    };
    assert_eq!(inst0.get(b"type".as_slice()), Some(&Value::string("file")));
    assert_eq!(
        inst0.get(b"destination".as_slice()),
        Some(&Value::string("include/mylib"))
    );
    assert_eq!(
        inst0.get(b"component".as_slice()),
        Some(&Value::string("Devel"))
    );
    assert_eq!(inst0.get(b"isOptional".as_slice()), Some(&Bool(true)));
    assert_eq!(
        inst0.get(b"paths".as_slice()),
        Some(&array([Value::string("include/mylib.h")]))
    );
    Ok(())
}

#[test]
fn codemodel_source_group_precedence_unity_backtrace_and_cross_order_dependencies()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyMap};
    use rcmake_model::{
        BT, CustomCommand, CustomCommandKind, FileSet, FileSetKind, GeneratorKind, SourceGroupDef,
        Target, TargetKind, TargetOrigin, UsageScope,
    };
    use rcmake_resolve::{ArtifactSet, CompileGroup, CompileUnit, ResolvedTarget, TargetMetadata};

    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    host.seed_file(Path::new("/src/a.c"), b"int a(void) { return 0; }\n")?;
    host.seed_file(Path::new("/src/b.c"), b"int b(void) { return 0; }\n")?;
    host.seed_file(Path::new("/src/tool.c"), b"int main(void) { return 0; }\n")?;
    host.seed_file(Path::new("/src/override.c"), b"int o(void) { return 0; }\n")?;
    host.seed_file(Path::new("/src/fs.h"), b"int f(void);\n")?;
    host.seed_file(Path::new("/src/fs_prop.h"), b"int p(void);\n")?;
    host.seed_file(Path::new("/src/plain_header"), b"int h(void);\n")?;
    host.seed_file(
        Path::new(r"/src/back\slash.c"),
        b"int s(void) { return 0; }\n",
    )?;

    let build = Path::new("/build");
    let query = build.join(API_DIR).join("query");
    let reply = build.join(API_DIR).join("reply");
    host.create_dir_all(&query)?;
    host.write(&query.join("codemodel-v2"), b"", WriteMode::CreateNew)?;

    let bt = |line: u32, cmd: &[u8]| {
        Backtrace::default().push(Frame {
            file: b"/src/CMakeLists.txt".as_slice().into(),
            line,
            name: Some(cmd.into()),
            kind: FrameKind::Command,
        })
    };

    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    model.set_root_dirs("/src".into(), "/build".into());
    for (name, value) in [
        ("CMAKE_SYSTEM_NAME", "Darwin"),
        ("CMAKE_C_COMPILER", "/usr/bin/cc"),
        ("CMAKE_C_COMPILER_ID", "AppleClang"),
        ("CMAKE_C_SOURCE_FILE_EXTENSIONS", "c"),
    ] {
        model.global.props.set(name.into(), value.into());
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(name.into(), value.into());
    }

    // Populate directory source_groups with a catch-all regex, a CMake bracket-class
    // `[\]` regex (literal backslash inside `[...]`), and a potentially quadratic `a+b`
    // regex to verify bounded matching and property/file-set precedence.
    let mut groups = rcmake_resolve::ide::default_source_groups();
    groups.push(SourceGroupDef {
        name: "RegexAll".into(),
        regex: Some(br"\.(c|h)$".as_slice().into()),
        ..SourceGroupDef::default()
    });
    groups.push(SourceGroupDef {
        name: "Backslash".into(),
        regex: Some(br"[\]".as_slice().into()),
        ..SourceGroupDef::default()
    });
    groups.push(SourceGroupDef {
        name: "Quadratic".into(),
        regex: Some(b"a+b".as_slice().into()),
        ..SourceGroupDef::default()
    });
    {
        let root_dir = model.dir_mut(DirId::ROOT);
        root_dir.source_groups = groups;
        // Explicit SOURCE_GROUP property must override RegexAll.
        root_dir
            .source_props
            .entry("/src/override.c".into())
            .or_default()
            .set("SOURCE_GROUP".into(), "Explicit/Source".into());
        // Simulated legacy mirrored regex property must NOT override directory source_groups.
        let mirrored = root_dir
            .source_props
            .entry(r"/src/back\slash.c".into())
            .or_default();
        mirrored.set("SOURCE_GROUP".into(), "RegexAll".into());
        mirrored.set("\0source_group_regex".into(), "RegexAll".into());
    }

    let mut tool = Target::new(
        "tool".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        bt(3, b"add_executable"),
        PolicyMap::default(),
    );
    tool.sources
        .push(BT::new("tool.c".into(), bt(3, b"add_executable")));
    let _tool_id = model.add_target(tool)?;

    let long_name = format!("{}.txt", "a".repeat(2000));
    host.seed_file(Path::new(&format!("/src/{long_name}")), b"")?;
    let mut app = Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        bt(5, b"add_executable"),
        PolicyMap::default(),
    );
    app.sources
        .push(BT::new("a.c".into(), bt(5, b"add_executable")));
    app.sources
        .push(BT::new("override.c".into(), bt(5, b"add_executable")));
    app.sources
        .push(BT::new(r"back\slash.c".into(), bt(5, b"add_executable")));
    app.sources
        .push(BT::new(long_name.clone().into(), bt(5, b"add_executable")));
    // Regular utility dependency on `tool` (cross = false, line 10).
    app.utility_deps
        .push(BT::new("tool".into(), bt(10, b"add_dependencies")));
    let app_id = model.add_target(app)?;

    // Cross-config utility dependency on `tool` via PRE_BUILD COMMAND `tool` (cross = true, line 15).
    let mut event = CustomCommand::new(
        DirId::ROOT,
        CustomCommandKind::TargetEvent {
            target: app_id,
            event: rcmake_model::TargetEvent::PreBuild,
        },
        bt(15, b"add_custom_command"),
        PolicyMap::default(),
    );
    event.command_lines.push(vec!["tool".into()]);
    model.add_command(event)?;

    // File set with explicit SOURCE_GROUP property overrides both source SOURCE_GROUP and regex.
    let mut fs_prop_props = rcmake_model::PropertyMap::default();
    fs_prop_props.set("SOURCE_GROUP".into(), "FileSet/Prop".into());
    let fs_prop = model.add_file_set(FileSet {
        target: app_id,
        name: "fs_prop".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![BT::new("/src".into(), bt(7, b"target_sources"))],
        files: vec![BT::new("/src/fs_prop.h".into(), bt(7, b"target_sources"))],
        props: fs_prop_props,
    })?;
    // File set assigned via source_group(... FILE_SETS ...).
    let mut fs_cmd_props = rcmake_model::PropertyMap::default();
    fs_cmd_props.set("\0source_group_file_set".into(), r"FileSet\Cmd".into());
    let fs_cmd = model.add_file_set(FileSet {
        target: app_id,
        name: "fs_cmd".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![BT::new("/src".into(), bt(8, b"target_sources"))],
        files: vec![BT::new("/src/fs.h".into(), bt(8, b"target_sources"))],
        props: fs_cmd_props,
    })?;
    // Header file set without a group falls back to "Header Files" for extensionless headers.
    let fs_plain = model.add_file_set(FileSet {
        target: app_id,
        name: "fs_plain".into(),
        kind: FileSetKind::Headers,
        scope: UsageScope::Private,
        base_dirs: vec![BT::new("/src".into(), bt(9, b"target_sources"))],
        files: vec![BT::new(
            "/src/plain_header".into(),
            bt(9, b"target_sources"),
        )],
        props: Default::default(),
    })?;
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry("/src/fs_prop.h".into())
        .or_default()
        .set("SOURCE_GROUP".into(), "SourceLowerPriority".into());
    let app_mut = model.target_mut(app_id).ok_or("missing app")?;
    app_mut.file_sets.extend([fs_prop, fs_cmd, fs_plain]);

    // Unity target to verify unity generated source uses file_trace (backtrace 0).
    let mut unity = Target::new(
        "u".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        bt(20, b"add_library"),
        PolicyMap::default(),
    );
    unity.props.set("UNITY_BUILD".into(), "ON".into());
    unity
        .sources
        .push(BT::new("b.c".into(), bt(20, b"add_library")));
    let unity_id = model.add_target(unity)?;

    let mut result = rcmake_resolve::resolve(&model, &host, &[])?;
    // Ensure a generated unity compile unit with non-zero source backtrace is present on target `u`.
    if let Some(resolved_u) = result.targets.iter_mut().find(|t| t.id == unity_id) {
        if resolved_u.compile_groups.is_empty() {
            resolved_u.compile_groups.push(CompileGroup {
                language: "C".into(),
                ..CompileGroup::default()
            });
        }
        resolved_u.compile_units.push(CompileUnit {
            source: BT::new(
                "/build/CMakeFiles/u.dir/Unity/unity_0_c.c".into(),
                bt(20, b"add_library"),
            ),
            object: "CMakeFiles/u.dir/Unity/unity_0_c.c.o".into(),
            group: 0,
            ..CompileUnit::default()
        });
    } else {
        result.targets.push(ResolvedTarget {
            id: unity_id,
            config: rcmake_genex::ConfigId::default(),
            metadata: TargetMetadata {
                name: "u".into(),
                kind: TargetKind::StaticLibrary,
                source_dir: "/src".into(),
                binary_dir: "/build".into(),
                backtrace: bt(20, b"add_library"),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet::default(),
            compile_groups: vec![CompileGroup {
                language: "C".into(),
                ..CompileGroup::default()
            }],
            compile_units: vec![CompileUnit {
                source: BT::new(
                    "/build/CMakeFiles/u.dir/Unity/unity_0_c.c".into(),
                    bt(20, b"add_library"),
                ),
                object: "CMakeFiles/u.dir/Unity/unity_0_c.c.o".into(),
                group: 0,
                ..CompileUnit::default()
            }],
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            fortran: None,
            ide: None,
        });
    }

    write_replies(&model, &result, &host, build)?;

    let read_reply = |prefix: &str| -> Result<Members, Box<dyn std::error::Error>> {
        let name = host
            .read_dir(&reply)?
            .into_iter()
            .find(|entry| entry.name.starts_with(prefix.as_bytes()))
            .ok_or_else(|| format!("missing reply file starting with {prefix}"))?
            .name;
        let bytes = host.read(
            &reply.join(to_native(&name)),
            ReadRange::All { limit: 1_000_000 },
        )?;
        let Object(obj) = rcmake_json::parse(&bytes, &Default::default())? else {
            return Err("expected json object".into());
        };
        Ok(obj)
    };

    // Verify source groups & orderDependencies on `app`.
    let app_json = read_reply("target-app-")?;
    let Some(Array(app_sources)) = app_json.get(b"sources".as_slice()) else {
        return Err("missing app sources".into());
    };
    let Some(Array(app_groups)) = app_json.get(b"sourceGroups".as_slice()) else {
        return Err("missing app sourceGroups".into());
    };
    let group_of = |path: &[u8]| -> Option<&[u8]> {
        let idx = app_sources.iter().find_map(|entry| match entry {
            Object(obj)
                if matches!(obj.get(b"path".as_slice()), Some(Value::String(p)) if p.as_slice() == path) =>
            {
                match obj.get(b"sourceGroupIndex".as_slice()) {
                    Some(Value::Int(i)) => usize::try_from(*i).ok(),
                    Some(Value::UInt(i)) => usize::try_from(*i).ok(),
                    _ => None,
                }
            }
            _ => None,
        })?;
        match app_groups.get(idx)? {
            Object(group) => match group.get(b"name".as_slice()) {
                Some(Value::String(name)) => Some(name.as_slice()),
                _ => None,
            },
            _ => None,
        }
    };
    assert_eq!(
        group_of(b"override.c"),
        Some(br"Explicit\Source".as_slice())
    );
    assert_eq!(group_of(b"fs_prop.h"), Some(br"FileSet\Prop".as_slice()));
    assert_eq!(group_of(b"fs.h"), Some(br"FileSet\Cmd".as_slice()));
    assert_eq!(group_of(b"plain_header"), Some(b"Header Files".as_slice()));
    let backslash_path: &[u8] = if cfg!(windows) {
        b"back/slash.c"
    } else {
        br"back\slash.c"
    };
    assert_eq!(group_of(backslash_path), Some(b"Backslash".as_slice()));
    assert_eq!(group_of(b"a.c"), Some(b"RegexAll".as_slice()));
    assert_eq!(group_of(long_name.as_bytes()), Some(b"".as_slice()));

    // Verify distinct (target_id, cross) utility items emit two orderDependencies rows
    // with distinct backtraces (line 10 regular before line 15 cross-config).
    let Some(Array(ord_deps)) = app_json.get(b"orderDependencies".as_slice()) else {
        return Err("missing app orderDependencies".into());
    };
    let Some(Object(app_bt)) = app_json.get(b"backtraceGraph".as_slice()) else {
        return Err("missing app backtraceGraph".into());
    };
    let Some(Array(app_nodes)) = app_bt.get(b"nodes".as_slice()) else {
        return Err("missing app backtrace nodes".into());
    };
    let tool_order_lines: Vec<u64> = ord_deps
        .iter()
        .filter_map(|item| {
            let Object(obj) = item else { return None };
            let Value::String(id) = obj.get(b"id".as_slice())? else {
                return None;
            };
            if !id.starts_with(b"tool::@") {
                return None;
            }
            let bt_idx = match obj.get(b"backtrace".as_slice())? {
                Value::Int(i) => usize::try_from(*i).ok()?,
                Value::UInt(i) => usize::try_from(*i).ok()?,
                _ => return None,
            };
            let Object(node) = app_nodes.get(bt_idx)? else {
                return None;
            };
            match node.get(b"line".as_slice())? {
                Value::Int(line) => u64::try_from(*line).ok(),
                Value::UInt(line) => Some(*line),
                _ => None,
            }
        })
        .collect();
    assert_eq!(tool_order_lines, vec![10, 15]);

    // Verify unity generated source emits file-level backtrace 0 (no line / command).
    let unity_json = read_reply("target-u-")?;
    let Some(Array(u_sources)) = unity_json.get(b"sources".as_slice()) else {
        return Err("missing unity sources".into());
    };
    let unity_entry = u_sources
        .iter()
        .find_map(|entry| match entry {
            Object(obj)
                if matches!(obj.get(b"path".as_slice()), Some(Value::String(p)) if p.ends_with(b"/Unity/unity_0_c.c")) =>
            {
                Some(obj)
            }
            _ => None,
        })
        .ok_or("missing generated unity source")?;
    assert_eq!(
        unity_entry.get(b"backtrace".as_slice()),
        Some(&Value::Int(0))
    );
    Ok(())
}

#[test]
fn file_api_reports_cuda_hip_and_swift_toolchains_and_compile_groups()
-> Result<(), Box<dyn std::error::Error>> {
    use rcmake_model::{BT, Target, TargetKind, TargetOrigin};

    let host = PureHost::default();
    let build = Path::new("/work/build");
    host.create_dir_all(Path::new("/work/src"))?;
    for src in ["k.cu", "h.hip", "s.swift"] {
        host.write(&Path::new("/work/src").join(src), b"", WriteMode::Truncate)?;
    }
    host.create_dir_all(&build.join(".cmake/api/v1/query"))?;
    host.write(
        &build.join(".cmake/api/v1/query/codemodel-v2"),
        b"",
        WriteMode::Truncate,
    )?;
    host.write(
        &build.join(".cmake/api/v1/query/toolchains-v1"),
        b"",
        WriteMode::Truncate,
    )?;

    let mut model = ProjectModel::default();
    model.set_root_dirs("/work/src".into(), "/work/build".into());
    model.generator = Some(GeneratorKind::Ninja);
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "CUDA;HIP;Swift".into());
    let root = model.dir_mut(DirId::ROOT);
    root.vars.insert("PROJECT_NAME".into(), "MultiLang".into());
    root.vars
        .insert("CMAKE_CUDA_COMPILER".into(), "/opt/cuda/bin/nvcc".into());
    root.vars
        .insert("CMAKE_CUDA_COMPILER_ID".into(), "NVIDIA".into());
    root.vars
        .insert("CMAKE_CUDA_COMPILER_VERSION".into(), "12.6.85".into());
    root.vars
        .insert("CMAKE_CUDA_SOURCE_FILE_EXTENSIONS".into(), "cu".into());
    root.vars
        .insert("CMAKE_CUDA_OUTPUT_EXTENSION".into(), ".o".into());
    root.vars
        .insert("CMAKE_CUDA_ARCHITECTURES".into(), "75".into());
    root.vars
        .insert("CMAKE_CUDA_STANDARD_DEFAULT".into(), "17".into());
    root.vars.insert(
        "CMAKE_CUDA17_STANDARD_COMPILE_OPTION".into(),
        "-std=c++17".into(),
    );
    root.vars.insert("CMAKE_HIP_PLATFORM".into(), "amd".into());
    root.vars
        .insert("CMAKE_HIP_COMPILER".into(), "/opt/rocm/bin/clang++".into());
    root.vars
        .insert("CMAKE_HIP_COMPILER_ID".into(), "Clang".into());
    root.vars
        .insert("CMAKE_HIP_COMPILER_VERSION".into(), "18.1.0".into());
    root.vars
        .insert("CMAKE_HIP_SOURCE_FILE_EXTENSIONS".into(), "hip".into());
    root.vars
        .insert("CMAKE_HIP_OUTPUT_EXTENSION".into(), ".o".into());
    root.vars
        .insert("CMAKE_HIP_ARCHITECTURES".into(), "gfx90a".into());
    root.vars
        .insert("CMAKE_HIP_STANDARD_DEFAULT".into(), "17".into());
    root.vars.insert(
        "CMAKE_HIP17_STANDARD_COMPILE_OPTION".into(),
        "-std=c++17".into(),
    );
    root.vars
        .insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    root.vars
        .insert("CMAKE_Swift_COMPILER_ID".into(), "Apple".into());
    root.vars
        .insert("CMAKE_Swift_COMPILER_VERSION".into(), "6.0".into());
    root.vars
        .insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    root.vars
        .insert("CMAKE_Swift_OUTPUT_EXTENSION".into(), ".o".into());
    root.vars.insert(
        "CMAKE_Swift_COMPILATION_MODE_DEFAULT".into(),
        "incremental".into(),
    );
    root.vars
        .insert("CMAKE_Swift_NUM_THREADS".into(), "2".into());
    root.vars.insert(
        "CMAKE_Swift_COMPILE_OBJECT".into(),
        "<CMAKE_Swift_COMPILER> -j 2 -num-threads 2 <DEFINES> <FLAGS> <INCLUDES> <SOURCE>".into(),
    );

    let policies = rcmake_diag::PolicyMap::default();
    for (name, src, props) in [
        (
            "cu_obj",
            "k.cu",
            [("CUDA_STANDARD", "17"), ("CUDA_ARCHITECTURES", "75")].as_slice(),
        ),
        (
            "hip_obj",
            "h.hip",
            [("HIP_STANDARD", "17"), ("HIP_ARCHITECTURES", "gfx90a")].as_slice(),
        ),
        ("sw_obj", "s.swift", [].as_slice()),
    ] {
        let mut t = Target::new(
            name.into(),
            TargetKind::ObjectLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            policies,
        );
        t.sources.push(BT::new(src.into(), Default::default()));
        for &(k, v) in props {
            t.props.set(k.into(), v.into());
        }
        model.add_target(t)?;
    }

    let result = rcmake_resolve::resolve(&model, &host, &[rcmake_genex::ConfigId::default()])?;
    super::write_replies(&model, &result, &host, build)?;

    let reply_dir = build.join(".cmake/api/v1/reply");
    let entries = host.read_dir(&reply_dir)?;
    let read_prefix = |prefix: &str| -> Result<String, Box<dyn std::error::Error>> {
        let file = entries
            .iter()
            .find(|e| e.name.starts_with(prefix.as_bytes()))
            .ok_or_else(|| format!("missing reply with prefix {prefix}"))?;
        let bytes = host.read(
            &reply_dir.join(String::from_utf8_lossy(&file.name).as_ref()),
            ReadRange::All { limit: 200_000 },
        )?;
        Ok(String::from_utf8(bytes)?)
    };

    let tc = read_prefix("toolchains-v1-")?;
    assert!(tc.contains("\"language\" : \"CUDA\""), "{tc}");
    assert!(tc.contains("\"language\" : \"HIP\""), "{tc}");
    assert!(tc.contains("\"language\" : \"Swift\""), "{tc}");

    let cu = read_prefix("target-cu_obj-")?;
    assert!(cu.contains("\"language\" : \"CUDA\""), "{cu}");
    assert!(cu.contains("\"standard\" : \"17\""), "{cu}");

    let hip = read_prefix("target-hip_obj-")?;
    assert!(hip.contains("\"language\" : \"HIP\""), "{hip}");
    assert!(hip.contains("--offload-arch=gfx90a"), "{hip}");
    assert!(hip.contains("\"standard\" : \"17\""), "{hip}");

    let sw = read_prefix("target-sw_obj-")?;
    assert!(sw.contains("\"language\" : \"Swift\""), "{sw}");
    Ok(())
}
