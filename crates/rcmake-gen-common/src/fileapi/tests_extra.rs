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
