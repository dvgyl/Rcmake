use std::path::Path;

use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyMap};
use rcmake_host::{PureHost, ReadRange};
use rcmake_json::ReadOptions;
use rcmake_model::{
    BT, CacheEntry, CacheType, InstallKind, InstallOptions, InstallRule, Target, TargetKind,
    TargetOrigin, VsVersion,
};
use rcmake_resolve::{
    ArtifactSet, CompileGroup, CompileUnit, InterfaceInclude, LinkFragment, LinkFragmentRole,
    LinkKind, LinkStep, ResolvedTarget, TargetMetadata, Toolchain,
};

use super::*;

const CLIENT_QUERY: &[u8] = br#"{
  "client": {"token": [1, null, "opaque"]},
  "ignored": "reserved",
  "requests": [
    {"kind": "codemodel", "version": [3, {"major": 2, "minor": 12}], "client": {"id": 7}},
    {"kind": "codemodel", "version": {"major": 2, "minor": 13}},
    {"kind": "cache", "version": 2},
    {"kind": "toolchains", "version": {"major": 1, "minor": 1}},
    {"kind": "cmakeFiles", "version": 1},
    {"kind": "configureLog", "version": 1},
    {"kind": "cache", "version": [2, false]},
    {"kind": "other", "version": 1}
  ]
}"#;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn parse(bytes: &[u8]) -> TestResult<Value> {
    Ok(rcmake_json::parse(bytes, &ReadOptions::default())?)
}

fn read_json(host: &dyn Host, path: &Path) -> TestResult<Value> {
    parse(&host.read(path, ReadRange::All { limit: 4_000_000 })?)
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    fields(value)
        .get(key.as_bytes())
        .unwrap_or_else(|| panic!("missing {key} in {value:?}"))
}

fn fields(value: &Value) -> &Members {
    let Object(fields) = value else {
        panic!("expected object, got {value:?}");
    };
    fields
}

fn items(value: &Value) -> &[Value] {
    let Array(items) = value else {
        panic!("expected array, got {value:?}");
    };
    items
}

fn string(value: &Value) -> &[u8] {
    let Value::String(value) = value else {
        panic!("expected string, got {value:?}");
    };
    value
}

fn uint(value: &Value) -> TestResult<usize> {
    match value {
        Value::Int(value) => Ok((*value).try_into()?),
        UInt(value) => Ok((*value).try_into()?),
        _ => Err(format!("expected non-negative integer, got {value:?}").into()),
    }
}

fn at(values: &[Value], index: usize) -> TestResult<&Value> {
    values
        .get(index)
        .ok_or_else(|| format!("missing array element {index}").into())
}

fn entry<'a>(values: &'a Value, key: &str, name: &str) -> &'a Value {
    items(values)
        .iter()
        .find(|value| string(get(value, key)) == name.as_bytes())
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn write(host: &dyn Host, path: &Path, content: &[u8]) -> TestResult {
    let parent = path.parent().ok_or("missing parent directory")?;
    host.create_dir_all(parent)?;
    host.write(path, content, WriteMode::Truncate)?;
    Ok(())
}

fn query_files(host: &dyn Host, build: &Path) -> TestResult {
    let query = build.join(API_DIR).join("query");
    for kind in Kind::ALL {
        write(
            host,
            &query.join(format!("{}-v{}", kind.name(), kind.version().0)),
            b"",
        )?;
    }
    write(host, &query.join("unknown"), b"")?;
    write(host, &query.join("client-test/cache-v2"), b"")?;
    write(host, &query.join("client-test/query.json"), CLIENT_QUERY)?;
    write(host, &query.join("client-malformed/query.json"), b"{")?;
    write(host, &query.join("client-missing/query.json"), b"{}")?;
    host.create_dir_all(&query.join("client-empty"))?;
    Ok(())
}

fn index(host: &dyn Host, reply: &Path) -> TestResult<(BString, Value)> {
    let entries = host.read_dir(reply)?;
    let names: Vec<_> = entries
        .into_iter()
        .map(|entry| entry.name)
        .filter(|name| name.starts_with(b"index-"))
        .collect();
    let [name] = names.as_slice() else {
        return Err("expected one index".into());
    };
    Ok((name.clone(), read_json(host, &reply.join(to_native(name)))?))
}

fn follow(host: &dyn Host, reply: &Path, reference: &Value) -> TestResult<Value> {
    read_json(
        host,
        &reply.join(to_native(string(get(reference, "jsonFile")))),
    )
}

fn trace(root: &Path, line: u32, command: &str) -> Backtrace {
    Backtrace::default().push(Frame {
        file: from_native_cmake(&root.join("CMakeLists.txt"))
            .to_vec()
            .into(),
        line,
        name: Some(command.as_bytes().into()),
        kind: FrameKind::Command,
    })
}

fn tagged(root: &Path, line: u32, command: &str, value: &[u8]) -> BT<BString> {
    BT::new(value.into(), trace(root, line, command))
}

fn cache_entry(model: &mut ProjectModel, name: &str, value: &[u8], ty: CacheType, help: &str) {
    model.cache.insert(
        name.into(),
        CacheEntry {
            value: value.into(),
            ty,
            help: help.into(),
            advanced: false,
            props: Default::default(),
        },
    );
}

fn fixture(source: &Path, build: &Path) -> TestResult<(ProjectModel, GenerationResult)> {
    let mut model = ProjectModel::default();
    model.generator = Some(GeneratorKind::Ninja);
    let source_dir = from_native_cmake(source);
    let binary_dir = from_native_cmake(build);
    model.set_root_dirs(source_dir.clone(), binary_dir.clone());
    cache_entry(
        &mut model,
        "CMAKE_PROJECT_NAME",
        b"Probe",
        CacheType::Static,
        "",
    );
    cache_entry(
        &mut model,
        "CMAKE_INSTALL_PREFIX",
        b"/probe",
        CacheType::Path,
        "",
    );
    cache_entry(&mut model, "PROBE", b"ON", CacheType::Bool, "Probe option");
    let probe = model
        .cache
        .get_mut(b"PROBE")
        .ok_or("missing PROBE cache entry")?;
    probe.advanced = true;
    probe.props.set("STRINGS".into(), "ON;OFF".into());

    let mut result = GenerationResult {
        configs: vec![Default::default()],
        ..Default::default()
    };
    let mut library = None;
    for (name, kind, line, command, output) in [
        (
            "lib",
            TargetKind::StaticLibrary,
            5,
            "add_library",
            "liblib.a",
        ),
        ("app", TargetKind::Executable, 6, "add_executable", "app"),
    ] {
        let backtrace = trace(source, line, command);
        let id = model.add_target(Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            backtrace.clone(),
            PolicyMap::default(),
        ))?;
        let mut group = CompileGroup {
            language: "C".into(),
            units: vec![0],
            ..Default::default()
        };
        let mut fragments = Vec::new();
        if kind == TargetKind::Executable {
            group.flags = vec![tagged(source, 9, "target_compile_options", b"-Wall")];
            group.defines = vec![tagged(source, 8, "target_compile_definitions", b"NAME=1")];
            group.includes = vec![InterfaceInclude {
                path: tagged(
                    source,
                    7,
                    "target_include_directories",
                    &from_native_cmake(&source.join("inc")),
                ),
                system: false,
            }];
            fragments.push(LinkFragment {
                text: tagged(source, 10, "target_link_libraries", b"liblib.a"),
                role: LinkFragmentRole::Libraries,
            });
            model.add_install(InstallRule {
                dir: DirId::ROOT,
                backtrace: trace(source, 11, "install"),
                kind: InstallKind::Targets {
                    targets: vec![id],
                    export: None,
                },
                options: InstallOptions::default(),
            });
        }
        result.targets.push(ResolvedTarget {
            id,
            config: Default::default(),
            metadata: TargetMetadata {
                name: name.into(),
                kind,
                source_dir: source_dir.clone(),
                binary_dir: binary_dir.clone(),
                backtrace: backtrace.clone(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: ArtifactSet {
                file: Some(output.into()),
                ..Default::default()
            },
            compile_groups: vec![group],
            compile_units: vec![CompileUnit {
                source: BT::new(from_native_cmake(&source.join("main.c")), backtrace),
                object: format!("CMakeFiles/{name}.dir/main.c.o").into(),
                group: 0,
                scan: false,
            }],
            link: Some(LinkStep {
                kind: if kind == TargetKind::Executable {
                    LinkKind::Executable
                } else {
                    LinkKind::StaticArchive
                },
                language: "C".into(),
                fragments,
                output: output.into(),
            }),
            dependencies: library.into_iter().collect(),
            cxx_modules: None,
            ide: None,
        });
        library = Some(id);
    }
    result.metadata.listfiles = vec![
        from_native_cmake(&source.join("CMakeLists.txt")),
        from_native_cmake(&build.join("generated.cmake")),
        "/elsewhere/module.cmake".into(),
        "/cmake/Modules/Check.cmake".into(),
    ];
    result.metadata.toolchains.insert(
        "C".into(),
        Toolchain {
            compiler_path: "/usr/bin/cc".into(),
            compiler_id: "Clang".into(),
            compiler_version: "21.0.0".into(),
            implicit_includes: vec!["/usr/include".into()],
            implicit_link_dirs: vec!["/usr/lib".into()],
            source_extensions: vec!["c".into()],
            scandep_rule: None,
            module_map_format: None,
            module_map_flag: None,
            compile_bmi_rule: None,
        },
    );
    cache_entry(&mut model, "CMAKE_ROOT", b"/cmake", CacheType::Internal, "");
    Ok((model, result))
}

#[test]
fn discovers_queries_and_replaces_stale_replies() -> TestResult {
    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let reply = build.join(API_DIR).join("reply");
    let (model, result) = fixture(Path::new("/src"), build)?;
    write_replies(&model, &result, &host, build)?;
    assert!(host.read_dir(&reply).is_err());

    query_files(&host, build)?;
    for stale in ["error-old.json", "index-old.json", "old.json"] {
        write(&host, &reply.join(stale), b"old")?;
    }
    write_replies(&model, &result, &host, build)?;
    let (before, index_json) = index(&host, &reply)?;
    let replies = get(&index_json, "reply");
    let client = get(get(replies, "client-test"), "query.json");
    let requests = parse(CLIENT_QUERY)?;
    assert_eq!(get(client, "client"), get(&requests, "client"));
    assert_eq!(get(client, "requests"), get(&requests, "requests"));
    assert!(!fields(client).contains_key(b"ignored".as_slice()));
    let responses = items(get(client, "responses"));
    assert_eq!(at(responses, 0)?, get(replies, "codemodel-v2"));
    assert_eq!(
        get(at(responses, 1)?, "error"),
        &Value::string("no supported version specified among: 2.13")
    );
    assert_eq!(
        at(responses, 2)?,
        get(get(replies, "client-test"), "cache-v2")
    );
    assert_eq!(
        get(at(responses, 6)?, "error"),
        &Value::string("'version' array entry is not a non-negative integer or object")
    );
    assert_eq!(
        get(at(responses, 7)?, "error"),
        &Value::string("unknown request kind 'other'")
    );
    assert_eq!(
        get(get(replies, "unknown"), "error"),
        &Value::string("unknown query file")
    );
    assert!(fields(get(replies, "client-empty")).is_empty());
    assert!(
        fields(get(get(replies, "client-malformed"), "query.json"))
            .contains_key(b"error".as_slice())
    );
    assert_eq!(
        get(
            get(
                get(get(replies, "client-missing"), "query.json"),
                "responses"
            ),
            "error"
        ),
        &Value::string("'requests' member missing")
    );
    assert_eq!(items(get(&index_json, "objects")).len(), 5);
    for reference in items(get(&index_json, "objects")) {
        let name = string(get(reference, "jsonFile"));
        let path = reply.join(to_native(name));
        let data = host.read(&path, ReadRange::All { limit: 4_000_000 })?;
        assert!(name.ends_with(format!("-{}.json", hash_prefix(&data)).as_bytes()));
        let object = parse(&data)?;
        assert_eq!(get(reference, "kind"), get(&object, "kind"));
        assert_eq!(get(reference, "version"), get(&object, "version"));
    }
    assert!(
        host.read_dir(&reply)?
            .iter()
            .all(|entry| !entry.name.ends_with(b"old.json"))
    );
    write_replies(&model, &result, &host, build)?;
    let (after, next_index) = index(&host, &reply)?;
    assert!(after > before);
    assert_eq!(next_index, index_json);

    host.remove_dir_all(&build.join(API_DIR).join("query"))?;
    write_replies(&model, &result, &host, build)?;
    assert!(host.read_dir(&reply)?.is_empty());
    Ok(())
}

#[test]
fn empty_directory_and_user_queries_publish_an_index() -> TestResult {
    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let reply = build.join(API_DIR).join("reply");
    let (model, result) = fixture(Path::new("/src"), build)?;
    host.create_dir_all(&build.join(API_DIR).join("query"))?;
    write_replies(&model, &result, &host, build)?;
    let (_, empty) = index(&host, &reply)?;
    assert!(fields(get(&empty, "reply")).is_empty());
    assert!(items(get(&empty, "objects")).is_empty());

    host.env_set(b"CMAKE_CONFIG_DIR", Some(b"/settings"));
    write(&host, Path::new("/settings/api/v1/query/__test-v1"), b"")?;
    write(&host, Path::new("/settings/api/v1/query/codemodel-v2"), b"")?;
    write_replies(&model, &result, &host, build)?;
    let (_, user) = index(&host, &reply)?;
    let objects = items(get(&user, "objects"));
    assert_eq!(objects.len(), 2);
    let test = get(get(&user, "reply"), "__test-v1");
    assert_eq!(test, entry(get(&user, "objects"), "kind", "__test"));
    let object = follow(&host, &reply, test)?;
    assert_eq!(uint(get(get(&object, "version"), "minor"))?, 3);
    Ok(())
}

#[test]
fn codemodel_merges_interface_includes_by_language_and_system_status() -> TestResult {
    let host = PureHost::new("/src".into(), "/bin/cmake".into());
    let build = Path::new("/build");
    let reply = build.join(API_DIR).join("reply");
    let (mut model, result) = fixture(Path::new("/src"), build)?;
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "C;CXX".into());
    let lib = model
        .find_target(DirId::ROOT, b"lib")
        .ok_or("missing lib target")?;
    let target = model.target_mut(lib).ok_or("missing lib target")?;
    target.props.set(
        "INTERFACE_INCLUDE_DIRECTORIES".into(),
        "/common;/mixed;$<$<COMPILE_LANGUAGE:C>:/c-only>;$<INSTALL_INTERFACE:/install>".into(),
    );
    target.props.set(
        "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES".into(),
        "$<$<COMPILE_LANGUAGE:CXX>:/mixed>".into(),
    );
    write(&host, &build.join(API_DIR).join("query/codemodel-v2"), b"")?;
    write_replies(&model, &result, &host, build)?;
    let (_, index) = index(&host, &reply)?;
    let codemodel = follow(&host, &reply, get(get(&index, "reply"), "codemodel-v2"))?;
    let targets = get(at(items(get(&codemodel, "configurations")), 0)?, "targets");
    let lib = follow(&host, &reply, entry(targets, "name", "lib"))?;
    assert_eq!(
        get(&lib, "interfaceIncludes"),
        &parse(
            br#"[
        {"path":"/common"},
        {"path":"/mixed","languages":["C"]},
        {"path":"/c-only","languages":["C"]},
        {"path":"/mixed","isSystem":true,"languages":["CXX"]}
    ]"#
        )?
    );
    Ok(())
}

#[test]
fn links_object_replies_and_backtraces() -> TestResult {
    let host = PureHost::default();
    let build = Path::new("/build");
    let reply = build.join(API_DIR).join("reply");
    let (model, result) = fixture(Path::new("/src"), build)?;
    query_files(&host, build)?;
    write_replies(&model, &result, &host, build)?;
    let (_, index) = index(&host, &reply)?;
    let replies = get(&index, "reply");
    let codemodel = follow(&host, &reply, get(replies, "codemodel-v2"))?;
    assert_eq!(uint(get(get(&codemodel, "version"), "minor"))?, 12);
    let config = at(items(get(&codemodel, "configurations")), 0)?;
    let dir = at(items(get(config, "directories")), 0)?;
    assert_eq!(get(dir, "source"), &Value::string("."));
    assert_eq!(
        get(at(items(get(config, "projects")), 0)?, "name"),
        &Value::string("Probe")
    );
    assert_eq!(get(dir, "hasInstallRule"), &Bool(true));
    let references = get(config, "targets");
    let app = follow(&host, &reply, entry(references, "name", "app"))?;
    let library = entry(references, "name", "lib");
    assert_eq!(
        get(at(items(get(&app, "dependencies")), 0)?, "id"),
        get(library, "id")
    );
    let dir_object = follow(&host, &reply, dir)?;
    let install = at(items(get(&dir_object, "installers")), 0)?;
    assert_eq!(get(install, "targetId"), get(&app, "id"));
    assert_eq!(get(install, "destination"), &Value::string("bin"));
    assert_eq!(
        get(get(get(&app, "install"), "prefix"), "path"),
        &Value::string("/probe")
    );

    let graph = get(&app, "backtraceGraph");
    assert_eq!(
        get(graph, "files"),
        &array([Value::string("CMakeLists.txt")])
    );
    let nodes = items(get(graph, "nodes"));
    let add = at(nodes, uint(get(&app, "backtrace"))?)?;
    assert_eq!(uint(get(add, "line"))?, 6);
    assert_eq!(
        at(items(get(graph, "commands")), uint(get(add, "command"))?)?,
        &Value::string("add_executable")
    );
    assert_eq!(uint(get(add, "parent"))?, 0);
    let group = at(items(get(&app, "compileGroups")), 0)?;
    let source_index = uint(at(items(get(group, "sourceIndexes")), 0)?)?;
    let source = at(items(get(&app, "sources")), source_index)?;
    assert_eq!(get(source, "path"), &Value::string("main.c"));
    assert_eq!(uint(get(source, "compileGroupIndex"))?, 0);
    let flag = at(items(get(group, "compileCommandFragments")), 0)?;
    assert_eq!(get(flag, "fragment"), &Value::string("-Wall"));
    let flag_location = at(nodes, uint(get(flag, "backtrace"))?)?;
    assert_eq!(uint(get(flag_location, "line"))?, 9);
    assert_eq!(
        get(at(items(get(group, "defines")), 0)?, "define"),
        &Value::string("NAME=1")
    );
    assert_eq!(
        get(at(items(get(group, "includes")), 0)?, "path"),
        &Value::string("/src/inc")
    );
    let link = at(items(get(get(&app, "link"), "commandFragments")), 0)?;
    assert_eq!(get(link, "role"), &Value::string("libraries"));
    assert_eq!(get(link, "fragment"), &Value::string("liblib.a"));

    let cache = follow(&host, &reply, get(replies, "cache-v2"))?;
    let probe = entry(get(&cache, "entries"), "name", "PROBE");
    assert_eq!(get(probe, "type"), &Value::string("BOOL"));
    let properties = get(probe, "properties");
    for (name, value) in [
        ("ADVANCED", "1"),
        ("HELPSTRING", "Probe option"),
        ("STRINGS", "ON;OFF"),
    ] {
        assert_eq!(
            get(entry(properties, "name", name), "value"),
            &Value::string(value)
        );
    }
    let toolchains = follow(&host, &reply, get(replies, "toolchains-v1"))?;
    let c = entry(get(&toolchains, "toolchains"), "language", "C");
    assert_eq!(
        get(get(c, "compiler"), "path"),
        &Value::string("/usr/bin/cc")
    );
    assert_eq!(get(c, "sourceFileExtensions"), &array([Value::string("c")]));
    let cmake_files = follow(&host, &reply, get(replies, "cmakeFiles-v1"))?;
    let inputs = get(&cmake_files, "inputs");
    for (path, flag) in [
        ("/build/generated.cmake", "isGenerated"),
        ("/elsewhere/module.cmake", "isExternal"),
        ("/cmake/Modules/Check.cmake", "isCMake"),
    ] {
        assert_eq!(get(entry(inputs, "path", path), flag), &Bool(true));
    }
    let configure_log = follow(&host, &reply, get(replies, "configureLog-v1"))?;
    assert_eq!(
        get(&configure_log, "path"),
        &Value::string("/build/CMakeFiles/CMakeConfigureLog.yaml")
    );
    assert!(
        items(get(&configure_log, "eventKindNames")).contains(&Value::string("try_compile-v1"))
    );
    Ok(())
}

#[test]
fn indexes_subprojects_and_abstract_targets_per_configuration() -> TestResult {
    let host = PureHost::default();
    let build = Path::new("/build");
    let (mut model, mut result) = fixture(Path::new("/src"), build)?;
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    let child = model.add_directory(
        DirId::ROOT,
        "/src/sub".into(),
        "/build/sub".into(),
        Backtrace::default(),
    )?;
    cache_entry(
        &mut model,
        "Child_SOURCE_DIR",
        b"/src/sub",
        CacheType::Static,
        "",
    );
    cache_entry(
        &mut model,
        "Child_BINARY_DIR",
        b"/build/sub",
        CacheType::Static,
        "",
    );
    model.add_target(Target::new(
        "api".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        child,
        trace(Path::new("/src/sub"), 2, "add_library"),
        PolicyMap::default(),
    ))?;
    result.configs = ["Debug", "Release"]
        .map(|name| rcmake_genex::ConfigId(name.into()))
        .into();
    for target in &mut result.targets {
        target.config.0 = "Debug".into();
    }
    let app = result
        .targets
        .iter()
        .find(|target| target.metadata.name == "app")
        .ok_or("missing resolved app target")?;
    let mut release = app.clone();
    release.config.0 = "Release".into();
    release.artifacts.file = Some("Release/app".into());
    result.targets.push(release);
    let install = model.installs.first_mut().ok_or("missing install rule")?;
    install.options.configurations = vec!["Release".into()];
    install.options.destination = Some("bin/release".into());

    write(&host, &build.join(API_DIR).join("query/codemodel-v2"), b"")?;
    write_replies(&model, &result, &host, build)?;
    let reply = build.join(API_DIR).join("reply");
    let (_, index) = index(&host, &reply)?;
    let codemodel = follow(&host, &reply, get(get(&index, "reply"), "codemodel-v2"))?;
    let configs = get(&codemodel, "configurations");
    let debug = entry(configs, "name", "Debug");
    let release = entry(configs, "name", "Release");
    let debug_ref = entry(get(debug, "targets"), "name", "app");
    let release_ref = entry(get(release, "targets"), "name", "app");
    assert_eq!(get(debug_ref, "id"), get(release_ref, "id"));
    assert_ne!(get(debug_ref, "jsonFile"), get(release_ref, "jsonFile"));
    assert_eq!(items(get(release, "targets")).len(), 1);
    let debug_app = follow(&host, &reply, debug_ref)?;
    let release_app = follow(&host, &reply, release_ref)?;
    assert!(!fields(&debug_app).contains_key(b"install".as_slice()));
    let destination = at(items(get(get(&release_app, "install"), "destinations")), 0)?;
    assert_eq!(get(destination, "path"), &Value::string("bin/release"));

    for config in [debug, release] {
        let directories = items(get(config, "directories"));
        let projects = items(get(config, "projects"));
        let api = entry(get(config, "abstractTargets"), "name", "api");
        let definition = follow(&host, &reply, api)?;
        assert_eq!(get(&definition, "abstract"), &Bool(true));
        let directory = at(directories, uint(get(api, "directoryIndex"))?)?;
        let project = at(projects, uint(get(api, "projectIndex"))?)?;
        assert_eq!(get(directory, "source"), &Value::string("sub"));
        assert_eq!(uint(get(directory, "parentIndex"))?, 0);
        assert_eq!(get(project, "name"), &Value::string("Child"));
        assert_eq!(uint(get(project, "parentIndex"))?, 0);
    }
    Ok(())
}

#[test]
fn xcode_reports_configuration_artifacts_and_utility_targets() -> TestResult {
    check_ide_replies(GeneratorKind::Xcode, "", false)
}

#[test]
fn visual_studio_reports_configuration_artifacts_and_utility_targets() -> TestResult {
    for version in VsVersion::ALL {
        check_ide_replies(GeneratorKind::VisualStudio(version), ".exe", true)?;
    }
    Ok(())
}

fn check_ide_replies(kind: GeneratorKind, suffix: &str, platform: bool) -> TestResult {
    let host = PureHost::default();
    let build = Path::new("/build");
    let (mut model, mut result) = fixture(Path::new("/src"), build)?;
    if platform {
        cache_entry(
            &mut model,
            "CMAKE_GENERATOR",
            kind.name().as_bytes(),
            CacheType::Internal,
            "",
        );
        cache_entry(
            &mut model,
            "CMAKE_GENERATOR_PLATFORM",
            b"x64",
            CacheType::Internal,
            "",
        );
        model.generator = None;
    } else {
        model.generator = Some(kind);
    }
    let mut artifact_model = model.clone();
    artifact_model.generator = Some(kind);
    let app = artifact_model
        .find_target(DirId::ROOT, b"app")
        .ok_or("missing app target")?;
    artifact_model
        .target_mut(app)
        .ok_or("missing app target")?
        .props
        .set("SUFFIX".into(), suffix.into());
    let debug = rcmake_genex::ConfigId("Debug".into());
    let release = rcmake_genex::ConfigId("Release".into());
    result.configs = vec![debug.clone(), release.clone()];
    let mut release_targets = result.targets.clone();
    for (targets, config) in [
        (&mut result.targets, debug),
        (&mut release_targets, release),
    ] {
        for target in targets {
            let raw = artifact_model
                .target(target.id)
                .ok_or("missing artifact target")?;
            target.artifacts =
                rcmake_resolve::lower::compute_artifacts(&artifact_model, raw, &config);
            target.config = config.clone();
        }
    }
    result.targets.extend(release_targets);

    write(&host, &build.join(API_DIR).join("query/codemodel-v2"), b"")?;
    write_replies(&model, &result, &host, build)?;
    let reply = build.join(API_DIR).join("reply");
    let (_, index) = index(&host, &reply)?;
    let generator = get(get(&index, "cmake"), "generator");
    assert_eq!(get(generator, "name"), &Value::string(kind.name()));
    assert_eq!(get(generator, "multiConfig"), &Bool(true));
    if platform {
        assert_eq!(get(generator, "platform"), &Value::string("x64"));
    }
    let codemodel = follow(&host, &reply, get(get(&index, "reply"), "codemodel-v2"))?;
    let configs = get(&codemodel, "configurations");
    assert_eq!(items(configs).len(), 2);
    for name in ["Debug", "Release"] {
        let config = entry(configs, "name", name);
        let targets = get(config, "targets");
        let app = entry(targets, "name", "app");
        let target = follow(&host, &reply, app)?;
        let artifacts = items(get(&target, "artifacts"));
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            get(at(artifacts, 0)?, "path"),
            &Value::string(format!("{name}/app{suffix}"))
        );
        assert_eq!(
            get(&target, "nameOnDisk"),
            &Value::string(format!("app{suffix}"))
        );

        let all = entry(targets, "name", "ALL_BUILD");
        let all = follow(&host, &reply, all)?;
        assert_eq!(get(&all, "type"), &Value::string("UTILITY"));
        assert_eq!(get(&all, "isGeneratorProvided"), &Bool(true));
        let zero_ref = entry(targets, "name", "ZERO_CHECK");
        let zero = follow(&host, &reply, zero_ref)?;
        assert_eq!(get(&zero, "isGeneratorProvided"), &Bool(true));
        for dependency in [app, zero_ref] {
            assert!(
                items(get(&all, "dependencies"))
                    .iter()
                    .any(|target| get(target, "id") == get(dependency, "id"))
            );
        }
        let directory = at(items(get(config, "directories")), 0)?;
        assert_eq!(
            items(get(directory, "targetIndexes")).len(),
            items(targets).len()
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn cmake_451_oracle_matches_reply_schema() -> TestResult {
    use std::{path::PathBuf, time::Duration};

    use rcmake_host::{CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest};

    let host = NativeHost::new()?;
    let Some(oracle) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
        return Ok(());
    };
    struct Temp<'a>(&'a NativeHost, PathBuf);
    impl Drop for Temp<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(&self.1);
        }
    }

    let time = host.now();
    let temp = Temp(
        &host,
        Path::new("/tmp").join(format!(
            "rcmake-fileapi-{}-{}-{}",
            host.system().pid,
            time.secs,
            time.nanos
        )),
    );
    let source = temp.1.join("src");
    let oracle_build = temp.1.join("oracle");
    let build = temp.1.join("rcmake");
    write(
        &host,
        &source.join("CMakeLists.txt"),
        br#"cmake_minimum_required(VERSION 4.5)
project(Probe C)
option(PROBE "Probe option" ON)
set_property(CACHE PROBE PROPERTY STRINGS ON OFF)
add_library(lib STATIC main.c)
add_executable(app main.c)
target_include_directories(app PRIVATE inc)
target_compile_definitions(app PRIVATE NAME=1)
target_compile_options(app PRIVATE -Wall)
target_link_libraries(app PRIVATE lib)
install(TARGETS app)
mark_as_advanced(PROBE)
"#,
    )?;
    write(
        &host,
        &source.join("main.c"),
        b"int main(void) { return 0; }\n",
    )?;
    query_files(&host, &oracle_build)?;
    let result = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: oracle,
                args: vec![
                    "-S".into(),
                    from_native_cmake(&source),
                    "-B".into(),
                    from_native_cmake(&oracle_build),
                    "-G".into(),
                    "Ninja".into(),
                    "-DCMAKE_INSTALL_PREFIX=/probe".into(),
                ],
            }],
            cwd: temp.1.clone(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(45)),
            tee: false,
        },
        &mut |_, _| {},
    )?;
    assert_eq!(
        result.results,
        [ProcessResult::Exited(0)],
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );

    query_files(&host, &build)?;
    let (model, result) = fixture(&source, &build)?;
    write_replies(&model, &result, &host, &build)?;
    let expected_dir = oracle_build.join(API_DIR).join("reply");
    let actual_dir = build.join(API_DIR).join("reply");
    let (_, expected) = index(&host, &expected_dir)?;
    let (_, actual) = index(&host, &actual_dir)?;
    assert_fields(&actual, &expected, &["cmake", "objects", "reply"]);
    assert_eq!(
        get(get(&actual, "cmake"), "version"),
        get(get(&expected, "cmake"), "version")
    );
    let actual_replies = get(&actual, "reply");
    let expected_replies = get(&expected, "reply");
    for (query, payload) in [
        ("codemodel-v2", "configurations"),
        ("cache-v2", "entries"),
        ("toolchains-v1", "toolchains"),
        ("cmakeFiles-v1", "inputs"),
        ("configureLog-v1", "eventKindNames"),
    ] {
        let actual = follow(&host, &actual_dir, get(actual_replies, query))?;
        let expected = follow(&host, &expected_dir, get(expected_replies, query))?;
        assert_fields(&actual, &expected, &["kind", "version", payload]);
        assert_eq!(get(&actual, "kind"), get(&expected, "kind"));
        assert_eq!(get(&actual, "version"), get(&expected, "version"));
        match query {
            "codemodel-v2" => {
                compare_codemodel(&host, &actual_dir, &actual, &expected_dir, &expected)?;
            }
            "cache-v2" => {
                let actual = entry(get(&actual, payload), "name", "PROBE");
                let expected = entry(get(&expected, payload), "name", "PROBE");
                for key in ["name", "type", "value", "properties"] {
                    assert_eq!(get(actual, key), get(expected, key));
                }
            }
            "toolchains-v1" => {
                let actual = get(entry(get(&actual, payload), "language", "C"), "compiler");
                let expected = get(entry(get(&expected, payload), "language", "C"), "compiler");
                assert_fields(actual, expected, &["path", "id", "version", "implicit"]);
            }
            "cmakeFiles-v1" => assert_eq!(
                entry(get(&actual, payload), "path", "CMakeLists.txt"),
                entry(get(&expected, payload), "path", "CMakeLists.txt"),
            ),
            "configureLog-v1" => assert_eq!(get(&actual, payload), get(&expected, payload)),
            _ => unreachable!(),
        }
    }
    let state = |value| get(get(value, "client-test"), "query.json");
    for key in ["client", "requests"] {
        assert_eq!(
            get(state(actual_replies), key),
            get(state(expected_replies), key)
        );
    }
    assert_eq!(
        at(items(get(state(actual_replies), "responses")), 1)?,
        at(items(get(state(expected_replies), "responses")), 1)?
    );
    Ok(())
}

#[cfg(unix)]
fn assert_fields(actual: &Value, expected: &Value, keys: &[&str]) {
    for key in keys {
        assert_eq!(
            std::mem::discriminant(get(actual, key)),
            std::mem::discriminant(get(expected, key)),
            "{key}"
        );
    }
}

#[cfg(unix)]
fn compare_codemodel(
    host: &dyn Host,
    actual_dir: &Path,
    actual: &Value,
    expected_dir: &Path,
    expected: &Value,
) -> TestResult {
    let actual = at(items(get(actual, "configurations")), 0)?;
    let expected = at(items(get(expected, "configurations")), 0)?;
    assert_fields(
        actual,
        expected,
        &[
            "name",
            "directories",
            "projects",
            "targets",
            "abstractTargets",
        ],
    );
    let actual = follow(
        host,
        actual_dir,
        entry(get(actual, "targets"), "name", "app"),
    )?;
    let expected = follow(
        host,
        expected_dir,
        entry(get(expected, "targets"), "name", "app"),
    )?;
    assert_fields(
        &actual,
        &expected,
        &[
            "name",
            "id",
            "type",
            "paths",
            "artifacts",
            "backtrace",
            "backtraceGraph",
            "compileGroups",
            "link",
            "sources",
            "dependencies",
            "install",
        ],
    );
    for field in ["type", "paths", "artifacts", "nameOnDisk"] {
        assert_eq!(get(&actual, field), get(&expected, field));
    }
    assert_fields(
        get(&actual, "backtraceGraph"),
        get(&expected, "backtraceGraph"),
        &["files", "commands", "nodes"],
    );
    let actual_group = at(items(get(&actual, "compileGroups")), 0)?;
    let expected_group = at(items(get(&expected, "compileGroups")), 0)?;
    assert_fields(
        actual_group,
        expected_group,
        &[
            "language",
            "compileCommandFragments",
            "defines",
            "includes",
            "sourceIndexes",
        ],
    );
    let actual_flag = entry(
        get(actual_group, "compileCommandFragments"),
        "fragment",
        "-Wall",
    );
    let expected_flag = entry(
        get(expected_group, "compileCommandFragments"),
        "fragment",
        "-Wall",
    );
    assert_fields(actual_flag, expected_flag, &["fragment", "backtrace"]);
    let actual_link = get(&actual, "link");
    let expected_link = get(&expected, "link");
    assert_fields(
        actual_link,
        expected_link,
        &["language", "commandFragments"],
    );
    assert_eq!(
        get(
            entry(get(actual_link, "commandFragments"), "role", "libraries"),
            "fragment"
        ),
        get(
            entry(get(expected_link, "commandFragments"), "role", "libraries"),
            "fragment"
        )
    );
    Ok(())
}
