use crate::{SemanticContext, SemanticProvider};

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use rcmake_api::{
    AliasSite, CallableDefinition, ExecutedSite, ExecutedSymbol, FileEdge, FileEdgeKind,
    SourceLocation, SourceRange, VariableDefinition,
};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, Location, MessageKind, PolicyMap, SourceMap, source::SourceKind,
};
use rcmake_eval::UserCommandKind;
use rcmake_host::{
    CommandSpec, Follow, Input, Output, PureHost, ReadRange, RunRequest, path::from_native_cmake,
};
use rcmake_model::{
    BT, CacheEntry, CacheType, DirId, LinkItem, Target, TargetKind, TargetOrigin, UsageField,
};

use crate::{
    PositionEncoding,
    json::{get, int, string},
    position::Lines,
};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Project {
    host: Arc<PureHost>,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let root = PathBuf::from(if cfg!(windows) {
            "C:/project"
        } else {
            "/project"
        });
        let host = Arc::new(PureHost::new(root.clone(), root.join("bin/rcmake")));
        Self { host, root }
    }

    fn file(&self, name: &str, text: &str) -> HostResult<()> {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            self.host.create_dir_all(parent)?;
        }
        self.host
            .write(&path, text.as_bytes(), HostWriteMode::Truncate)
    }

    fn uri(&self, name: &str) -> String {
        uri::file(&self.root.join(name))
    }

    fn provider(
        &self,
        evaluate: impl Fn(&PreparedConfigure) -> Result<ConfigureResult, ApiError>
        + Send
        + Sync
        + 'static,
    ) -> EvalSemanticProvider {
        EvalSemanticProvider::new(
            ConfigureRequest::new(
                &self.root,
                self.root.join("build"),
                self.root.join("cmake"),
                self.host.clone(),
            ),
            evaluate,
        )
    }
}

fn open(provider: &EvalSemanticProvider, uri: &str, text: &str, version: i64) {
    provider.update(SemanticUpdate::Changed { uri, text, version });
}

fn cursor<'a>(uri: &'a str, text: &'a str, version: i64, word: &str) -> SemanticContext<'a> {
    let offset = text.find(word).unwrap_or(text.len());
    let encoding = PositionEncoding::Utf16;
    SemanticContext {
        uri,
        text,
        version,
        offset,
        encoding,
        position: Lines::new(text).position(text, offset, encoding),
    }
}

fn model_at(session: &PreparedConfigure) -> ConfigureResult {
    let mut result = ConfigureResult::default();
    result.model.set_root_dirs(
        from_native_cmake(&session.source_dir),
        from_native_cmake(&session.build_dir),
    );
    result
        .model
        .dir_mut(DirId::ROOT)
        .listfiles
        .push(from_native_cmake(
            &session.source_dir.join("CMakeLists.txt"),
        ));
    result
}

fn location(path: &Path, line: u32) -> SourceLocation {
    SourceLocation {
        path: from_native_cmake(path),
        line,
        byte_offset: None,
        source_id: None,
        range: None,
    }
}

fn invalid_fixture() -> ApiError {
    ApiError::InvalidConfig {
        field: "fixture",
        message: "The test input is incomplete.".into(),
    }
}

fn read(host: &dyn Host, path: &Path) -> HostResult<Vec<u8>> {
    host.read(path, ReadRange::All { limit: 4096 })
}

#[test]
fn evaluates_generator_hover_and_preserves_target_definition() -> TestResult {
    let project = Project::new();
    let lists = r#"include(defs.cmake)
add_library(app INTERFACE)
target_compile_definitions(app INTERFACE "$<$<CONFIG:Debug>:DEBUG_ONLY>")
set(output "$<TARGET_FILE:tool>")
set_target_properties(app PROPERTIES CUSTOM "$<TARGET_PROPERTY:NAME>")
set_property(TARGET app PROPERTY OTHER "$<TARGET_PROPERTY:tool,NAME>")
"#;
    let defs = "add_executable(tool IMPORTED)\n";
    project.file("defs.cmake", defs)?;
    let provider = project
        .provider(|session| {
            let mut result = model_at(session);
            result.model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
            result
                .model
                .dir_mut(DirId::ROOT)
                .vars
                .insert("CMAKE_CONFIGURATION_TYPES".into(), "Debug;Release".into());
            let defs = session.source_dir.join("defs.cmake");
            read(session.host().as_ref(), &defs)?;
            let mut tool = Target::new(
                "tool".into(),
                TargetKind::Executable,
                TargetOrigin::Imported(rcmake_model::ImportedVisibility::Global),
                DirId::ROOT,
                Backtrace::default(),
                PolicyMap::default(),
            );
            tool.props.set(
                "IMPORTED_LOCATION".into(),
                from_native_cmake(&session.build_dir.join("bin/tool")),
            );
            let id = result
                .model
                .add_target(tool)
                .map_err(|_| invalid_fixture())?;
            result.facts.target_sites.insert(id, location(&defs, 1));
            result
                .model
                .add_target(Target::new(
                    "app".into(),
                    TargetKind::InterfaceLibrary,
                    TargetOrigin::Normal,
                    DirId::ROOT,
                    Backtrace::default(),
                    PolicyMap::default(),
                ))
                .map_err(|_| invalid_fixture())?;
            Ok(result)
        })
        .with_explicit_refresh();
    let root_uri = project.uri("CMakeLists.txt");
    open(&provider, &root_uri, lists, 1);
    provider.snapshots().refresh()?;

    for (word, expected) in [
        (
            "DEBUG_ONLY",
            [
                r"$<$<CONFIG:Debug>:DEBUG_ONLY>",
                r"Target: app",
                r"Debug: DEBUG_ONLY",
                "Release: (empty)",
            ],
        ),
        (
            "$<TARGET_FILE",
            [
                "$<TARGET_FILE:tool>",
                "Target: None",
                "bin/tool",
                r"CMakeLists.txt:4",
            ],
        ),
        (
            "$<TARGET_PROPERTY:NAME>",
            [
                "$<TARGET_PROPERTY:NAME>",
                "Target: app",
                "Debug: app",
                "Release: app",
            ],
        ),
        (
            "$<TARGET_PROPERTY:tool",
            [
                "$<TARGET_PROPERTY:tool,NAME>",
                "Target: app",
                "Debug: tool",
                "Release: tool",
            ],
        ),
    ] {
        let (hover, version) = provider
            .hover_markdown(&cursor(&root_uri, lists, 1, word))
            .ok_or("Missing generator expression hover")?;
        assert_eq!(version, Some(1));
        for value in expected {
            assert!(hover.contains(value), "{value:?} missing from {hover:?}");
        }
    }
    for word in ["tool>", "tool,NAME>"] {
        let definitions = provider.definitions(&cursor(&root_uri, lists, 1, word));
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].uri, project.uri("defs.cmake"));
        assert_eq!(definitions[0].start.line, 0);
    }
    Ok(())
}

#[test]
fn defines_unopened_functions_targets_and_aliases() -> TestResult {
    let project = Project::new();
    let root = "include(defs.cmake)\nMY_HELPER(arg)\ntarget_link_libraries(app PRIVATE tools tools_alias)\n";
    let defs = "function(My_Helper input)\nendfunction()\nadd_library(tools INTERFACE)\nadd_library(tools_alias ALIAS tools)\n";
    project.file("defs.cmake", defs)?;
    let provider = project
        .provider(|session| {
            let mut result = model_at(session);
            let defs = session.source_dir.join("defs.cmake");
            let text = read(session.host().as_ref(), &defs)?;
            let mut sources = SourceMap::default();
            let source_id = sources.add(from_native_cmake(&defs), SourceKind::File, text.into());
            result.facts.source_map = Some(Arc::new(sources));
            result.facts.callables.push(CallableDefinition {
                name: "My_Helper".into(),
                kind: UserCommandKind::Function,
                location: SourceLocation {
                    source_id,
                    range: Some(SourceRange { start: 9, end: 18 }),
                    ..location(&defs, 1)
                },
                parameters: vec!["input".into()],
                documentation: None,
            });
            let mut tools = Target::new(
                "tools".into(),
                TargetKind::InterfaceLibrary,
                TargetOrigin::Normal,
                DirId::ROOT,
                Backtrace::default(),
                PolicyMap::default(),
            );
            tools.usage.fields.insert(
                UsageField::InterfaceCompileDefinitions,
                vec![BT::new("TOOLS".into(), Backtrace::default())],
            );
            tools.usage.fields.insert(
                UsageField::InterfaceIncludeDirectories,
                vec![BT::new("headers".into(), Backtrace::default())],
            );
            tools.usage.interface_link_libraries = Some(vec![LinkItem {
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: None,
                text: BT::new("m".into(), Backtrace::default()),
                lookup_dir: DirId::ROOT,
            }]);
            let id = result
                .model
                .add_target(tools)
                .map_err(|_| invalid_fixture())?;
            result.facts.target_sites.insert(id, location(&defs, 3));
            result
                .model
                .add_alias(DirId::ROOT, "tools_alias".into(), id)
                .map_err(|_| invalid_fixture())?;
            let decoy = result
                .model
                .add_target(Target::new(
                    "PRIVATE".into(),
                    TargetKind::InterfaceLibrary,
                    TargetOrigin::Normal,
                    DirId::ROOT,
                    Backtrace::default(),
                    PolicyMap::default(),
                ))
                .map_err(|_| invalid_fixture())?;
            result.facts.target_sites.insert(decoy, location(&defs, 3));
            result.facts.aliases.push(AliasSite {
                name: "tools_alias".into(),
                dir: DirId::ROOT,
                global: true,
                location: location(&defs, 4),
            });
            Ok(result)
        })
        .with_explicit_refresh();
    let root_uri = project.uri("CMakeLists.txt");
    let defs_uri = project.uri("defs.cmake");
    open(&provider, &root_uri, root, 1);
    provider.snapshots().refresh()?;

    for (word, line, column) in [
        ("MY_HELPER", 0, 9),
        ("tools tools_alias", 2, 0),
        ("tools_alias", 3, 0),
    ] {
        let context = cursor(&root_uri, root, 1, word);
        let answer = provider.definition(&context).ok_or("Missing definition")?;
        assert_eq!(answer.uri, defs_uri);
        assert_eq!(answer.start.line, line);
        assert_eq!(answer.start.character, column);
    }
    let context = cursor(&root_uri, root, 1, "MY_HELPER");
    let (hover, _) = provider
        .hover_markdown(&context)
        .ok_or("Missing callable hover")?;
    assert!(hover.contains("function(My_Helper input)"));
    let context = cursor(&root_uri, root, 1, "tools_alias");
    let (hover, _) = provider
        .hover_markdown(&context)
        .ok_or("Missing target hover")?;
    assert!(hover.contains("INTERFACE\\_LIBRARY alias for tools"));
    assert!(hover.contains("Compile definitions"));
    assert!(hover.contains("Include directories"));
    assert!(hover.contains("Link libraries"));
    assert!(
        provider
            .definition(&cursor(&root_uri, root, 1, "PRIVATE"))
            .is_none()
    );
    let reply = provider
        .definitions_reply(&context)
        .ok_or("Missing reply")?;
    open(&provider, &root_uri, root, 2);
    let published = AtomicBool::new(false);
    assert!(!reply.publish(provider.snapshots(), |_| {
        published.store(true, Ordering::Relaxed)
    }));
    assert!(!published.load(Ordering::Relaxed));

    provider.snapshots().refresh()?;
    project.file("defs.cmake", &format!("# Insert a line.\n{defs}"))?;
    for word in ["MY_HELPER", "tools tools_alias", "tools_alias"] {
        assert!(
            provider
                .definitions(&cursor(&root_uri, root, 2, word))
                .is_empty()
        );
    }
    Ok(())
}

#[test]
fn reads_unsaved_variables_and_preserves_source_offsets() -> TestResult {
    let project = Project::new();
    let disk = "set(VALUE disk)\r\nmessage(\"😀 ${VALUE} $ENV{VALUE}\")\r\n";
    let unsaved = disk.replace("disk", "live");
    project.file("CMakeLists.txt", disk)?;
    let provider = project.provider(|session| {
        let mut result = model_at(session);
        let path = session.source_dir.join("CMakeLists.txt");
        let text = read(session.host().as_ref(), &path)?;
        let text = std::str::from_utf8(&text).map_err(|_| invalid_fixture())?;
        let value = text
            .strip_prefix("set(VALUE ")
            .and_then(|tail| tail.split(')').next())
            .ok_or_else(invalid_fixture)?;
        let reference = text
            .find("${VALUE}")
            .and_then(|offset| u32::try_from(offset).ok())
            .ok_or_else(invalid_fixture)?;
        let mut sources = SourceMap::default();
        let source_id = sources.add(
            from_native_cmake(&path),
            SourceKind::File,
            Arc::from(text.as_bytes()),
        );
        let definition = SourceLocation {
            source_id,
            range: Some(SourceRange { start: 4, end: 9 }),
            ..location(&path, 1)
        };
        result.facts.source_map = Some(Arc::new(sources));
        result
            .model
            .dir_mut(DirId::ROOT)
            .vars
            .insert("VALUE".into(), "final".into());
        assert!(
            Arc::make_mut(&mut result.facts.executed_sites).record(ExecutedSite {
                dir: DirId::ROOT,
                site: SourceLocation {
                    source_id,
                    range: Some(SourceRange {
                        start: reference,
                        end: reference + 8
                    }),
                    ..location(&path, 2)
                },
                symbol: ExecutedSymbol::Variable {
                    name: "VALUE".into(),
                    definition: Some(VariableDefinition {
                        value: value.into(),
                        location: Some(definition),
                        cached: false,
                        provisional: false,
                    }),
                },
            })
        );
        Ok(result)
    });
    let uri = project.uri("CMakeLists.txt");
    for (text, version, value) in [(disk, 1, "disk"), (unsaved.as_str(), 2, "live")] {
        open(&provider, &uri, text, version);
        let mut context = cursor(&uri, text, version, "${VALUE}");
        context.offset += 3;
        context.position = Lines::new(text).position(text, context.offset, context.encoding);
        let (hover, used) = provider
            .hover_markdown(&context)
            .ok_or("Missing variable hover")?;
        assert!(hover.contains(&format!("VALUE = {value}")));
        assert!(hover.contains("CMakeLists\\.txt:1"));
        assert_eq!(used, Some(version));
        let answer = provider
            .definition(&context)
            .ok_or("Missing variable definition")?;
        assert_eq!(answer.start.character, 4);
        assert_eq!(answer.end.character, 9);
        assert!(
            provider
                .hover(&cursor(&uri, text, version, "$ENV{VALUE}"))
                .is_none()
        );
        assert!(
            provider
                .completions(&context)
                .iter()
                .any(|item| get(item, "label").and_then(string) == Some("VALUE"))
        );
    }
    assert_eq!(
        read(project.host.as_ref(), &project.root.join("CMakeLists.txt"))?,
        disk.as_bytes()
    );
    assert!(
        project
            .host
            .metadata(&project.root.join("build"), Follow::Yes)
            .is_err()
    );
    Ok(())
}

#[test]
fn follows_all_evaluated_include_and_subdirectory_destinations() -> TestResult {
    let project = Project::new();
    let setup = "add_subdirectory(src)\ninclude(Extras.cmake)\n";
    let root = "include(cmake/setup.cmake)\n";
    project.file("cmake/setup.cmake", setup)?;
    for file in [
        "src/CMakeLists.txt",
        "cmake/src/CMakeLists.txt",
        "left/Extras.cmake",
        "right/Extras.cmake",
    ] {
        project.file(file, "")?;
    }
    let provider = project.provider(|session| {
        let mut result = model_at(session);
        let path = session.source_dir.join("cmake/setup.cmake");
        read(session.host().as_ref(), &path)?;
        result
            .model
            .dir_mut(DirId::ROOT)
            .listfiles
            .push(from_native_cmake(&path));
        let edge = |dir, kind, line, start, end, destination| FileEdge {
            dir,
            kind,
            site: location(&path, line),
            argument_range: Some(SourceRange { start, end }),
            destination: from_native_cmake(&session.source_dir.join(destination)),
        };
        result.facts.file_edges.push(edge(
            DirId::ROOT,
            FileEdgeKind::AddSubdirectory,
            1,
            17,
            20,
            "src/CMakeLists.txt",
        ));
        for (directory, extras) in [
            ("left", "left/Extras.cmake"),
            ("right", "right/Extras.cmake"),
        ] {
            let dir = result
                .model
                .add_directory(
                    DirId::ROOT,
                    from_native_cmake(&session.source_dir.join(directory)),
                    from_native_cmake(&session.build_dir.join(directory)),
                    Backtrace::default(),
                )
                .map_err(|_| invalid_fixture())?;
            result
                .model
                .dir_mut(dir)
                .listfiles
                .push(from_native_cmake(&path));
            result
                .facts
                .file_edges
                .push(edge(dir, FileEdgeKind::Include, 2, 30, 42, extras));
        }
        Ok(result)
    });
    open(&provider, &project.uri("CMakeLists.txt"), root, 1);
    let uri = project.uri("cmake/setup.cmake");
    open(&provider, &uri, setup, 1);
    let destinations = |word| {
        provider
            .definitions(&cursor(&uri, setup, 1, word))
            .into_iter()
            .map(|location| location.uri)
            .collect::<Vec<_>>()
    };
    assert_eq!(destinations("src"), [project.uri("src/CMakeLists.txt")]);
    assert_eq!(
        destinations("Extras"),
        [
            project.uri("left/Extras.cmake"),
            project.uri("right/Extras.cmake")
        ]
    );
    Ok(())
}

#[test]
fn cancels_a_run_when_a_later_edit_arrives() -> TestResult {
    const FIRST: &str = "message(\"${VALUE}\")\n";
    const SECOND: &str = "# Shift the cursor.\nmessage(\"${VALUE}\")\n";
    const THIRD: &str = "# Change it again.\n\nmessage(\"${VALUE}\")\n";
    let project = Project::new();
    let uri = project.uri("CMakeLists.txt");
    let handle = Arc::new(OnceLock::<Weak<EvalSemanticProvider>>::new());
    let weak = Arc::clone(&handle);
    let interrupt = Arc::new(AtomicBool::new(false));
    let interrupt_run = Arc::clone(&interrupt);
    let edit_uri = uri.clone();
    let provider = Arc::new(
        project
            .provider(move |session| {
                assert_eq!(
                    session.services.step_budget,
                    Some(super::DEFAULT_STEP_BUDGET)
                );
                let token = session
                    .services
                    .cancel
                    .as_ref()
                    .ok_or_else(invalid_fixture)?;
                if interrupt_run.swap(false, Ordering::AcqRel) {
                    let provider = weak
                        .get()
                        .and_then(Weak::upgrade)
                        .ok_or_else(invalid_fixture)?;
                    open(&provider, &edit_uri, THIRD, 3);
                    assert!(token.is_cancelled());
                    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::Busy);
                    let context = cursor(&edit_uri, THIRD, 3, "${VALUE}");
                    let (hover, version) = provider
                        .hover_markdown(&context)
                        .ok_or_else(invalid_fixture)?;
                    assert!(hover.contains("*(provisional snapshot)*"));
                    assert_eq!(version, Some(1));
                    assert!(provider.definitions(&context).is_empty());
                }
                let mut result = model_at(session);
                result
                    .model
                    .dir_mut(DirId::ROOT)
                    .vars
                    .insert("VALUE".into(), "visible".into());
                Ok(result)
            })
            .with_explicit_refresh(),
    );
    handle
        .set(Arc::downgrade(&provider))
        .map_err(|_| "Provider already set")?;
    open(&provider, &uri, FIRST, 1);
    provider.snapshots().refresh()?;
    interrupt.store(true, Ordering::Release);
    open(&provider, &uri, SECOND, 2);
    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::Completed);
    assert_eq!(
        provider
            .hover_markdown(&cursor(&uri, THIRD, 3, "${VALUE}"))
            .map(|(_, version)| version),
        Some(Some(3)),
    );
    Ok(())
}

#[test]
fn publishes_a_denial_hint_with_configure_diagnostics() -> TestResult {
    let project = Project::new();
    let provider = project.provider(|session| {
        let host = session.host();
        host.write_out(Stream::Stdout, b"-- Configure message\n");
        let run = RunRequest {
            pipeline: vec![CommandSpec {
                program: "cc".into(),
                args: Vec::new(),
            }],
            cwd: session.source_dir.clone(),
            env: Vec::new(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
            default_error_mode: false,
        };
        assert!(host.run_bounded(&run, 4096).is_err());
        host.write(
            &session.source_dir.join("configure-output"),
            b"memory",
            HostWriteMode::Truncate,
        )?;
        Ok(ConfigureResult {
            diagnostics: vec![Diagnostic {
                kind: MessageKind::Warning,
                effect: Effect::None,
                category: None,
                policy: None,
                body: "A configure warning.".into(),
                location: Location::None,
                annotations: None,
            }],
            ..model_at(session)
        })
    });
    let results = provider.diagnostics(&project.uri("CMakeLists.txt"));
    assert!(project.host.stream_bytes(Stream::Stdout).is_empty());
    let messages: Vec<_> = results
        .iter()
        .map(|item| {
            (
                get(item, "severity").and_then(int),
                get(item, "message").and_then(string),
            )
        })
        .collect();
    assert_eq!(
        messages,
        [
            (
                Some(3),
                Some(
                    "The editor cannot run this command. Run a local configure to get the result."
                )
            ),
            (Some(2), Some("A configure warning.")),
        ]
    );
    assert!(
        project
            .host
            .metadata(&project.root.join("configure-output"), Follow::Yes)
            .is_err()
    );
    Ok(())
}

#[test]
fn uses_the_same_buffer_for_symlink_and_encoded_uris() -> TestResult {
    let project = Project::new();
    project.file("source file.cmake", "on disk")?;
    let source = project.root.join("source file.cmake");
    let alias = project.root.join("alias.cmake");
    project.host.symlink(&source, &alias)?;
    let overlay = BufferOverlayHost::new(project.host.clone());
    overlay.set_buffer(&project.uri("alias.cmake"), "in memory")?;
    assert_eq!(read(overlay.host().as_ref(), &source)?, b"in memory");
    overlay.set_buffer(&project.uri("source file.cmake"), "new text")?;
    assert_eq!(read(overlay.host().as_ref(), &alias)?, b"new text");
    assert_eq!(read(project.host.as_ref(), &source)?, b"on disk");

    let inputs = Arc::new(Mutex::new(Vec::new()));
    let reads = Arc::clone(&inputs);
    let provider = project
        .provider(move |session| {
            let text = read(session.host().as_ref(), &source)?;
            reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text);
            Ok(model_at(session))
        })
        .with_explicit_refresh();
    let source = project.uri("source file.cmake");
    let alias = project.uri("alias.cmake");
    open(&provider, &source, "unsaved first", 20);
    open(&provider, &alias, "unsaved second", 1);
    provider.snapshots().refresh()?;
    provider.update(SemanticUpdate::Closed { uri: &alias });
    provider.snapshots().refresh()?;
    assert_eq!(
        *inputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [b"unsaved second".to_vec(), b"unsaved first".to_vec()],
    );
    Ok(())
}

#[test]
fn undefined_cache_reference_does_not_use_the_final_cache_value() -> TestResult {
    let project = Project::new();
    let text =
        "message($CACHE{VALUE})\nset(VALUE later CACHE STRING Help)\nmessage($CACHE{VALUE})\n";
    let provider = project.provider(|session| {
        let mut result = model_at(session);
        let path = session.source_dir.join("CMakeLists.txt");
        read(session.host().as_ref(), &path)?;
        result.model.cache.insert(
            "VALUE".into(),
            CacheEntry {
                value: "later".into(),
                ty: CacheType::String,
                help: "".into(),
                advanced: false,
                props: Default::default(),
            },
        );
        assert!(
            Arc::make_mut(&mut result.facts.executed_sites).record(ExecutedSite {
                dir: DirId::ROOT,
                site: SourceLocation {
                    range: Some(SourceRange { start: 8, end: 21 }),
                    ..location(&path, 1)
                },
                symbol: ExecutedSymbol::Variable {
                    name: "VALUE".into(),
                    definition: None
                },
            })
        );
        Ok(result)
    });
    let uri = project.uri("CMakeLists.txt");
    open(&provider, &uri, text, 1);
    let mut earlier = cursor(&uri, text, 1, "$CACHE{VALUE}");
    earlier.offset += 8;
    assert!(provider.hover(&earlier).is_none());
    let later_offset = text
        .rfind("$CACHE{VALUE}")
        .ok_or("Missing later reference")?
        + 8;
    let later = SemanticContext {
        offset: later_offset,
        ..earlier
    };
    assert!(
        provider
            .hover(&later)
            .is_some_and(|hover| hover.contains("VALUE = later"))
    );
    Ok(())
}

#[test]
fn early_fatal_denial_counts_each_request_once() -> TestResult {
    let project = Project::new();
    project.file(
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 4.5)\nproject(p NONE)\n",
    )?;
    project.file(
        "PreLoad.cmake",
        concat!(
            "execute_process(COMMAND same RESULT_VARIABLE result)\n",
            "execute_process(COMMAND same COMMAND_ERROR_IS_FATAL ANY)\n",
        ),
    )?;
    let provider = project.provider(PreparedConfigure::evaluate);
    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::Completed);
    let state = provider.snapshots().lock();
    let snapshot = state.snapshot.as_ref().ok_or("Missing partial snapshot.")?;
    assert!(matches!(
        &snapshot.result.completion,
        CompletionState::Partial { denied_processes, omitted_denials: 0, .. }
            if denied_processes.len() == 2
    ));
    assert!(
        snapshot
            .result
            .index
            .as_ref()
            .is_some_and(|index| index.is_provisional())
    );
    Ok(())
}

#[test]
fn disk_slice_refreshes_when_the_file_changes_at_the_same_size() -> TestResult {
    let project = Project::new();
    let top = concat!(
        "cmake_minimum_required(VERSION 4.5)\nproject(p NONE)\n",
        "file(READ \"${CMAKE_CURRENT_SOURCE_DIR}/settings.txt\" VALUE OFFSET 2 LIMIT 3)\n",
        "file(WRITE \"${CMAKE_CURRENT_SOURCE_DIR}/settings.txt\" overlay)\n",
    );
    project.file("CMakeLists.txt", top)?;
    project.file("settings.txt", "xxold")?;
    let provider = project.provider(PreparedConfigure::evaluate);
    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::Completed);
    let path = project.root.join("settings.txt");
    assert!(provider.snapshots().disk_paths().contains(&path));
    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::UpToDate);
    project.file("settings.txt", "xxnew")?;
    assert_eq!(provider.snapshots().refresh()?, RefreshStatus::Completed);
    let state = provider.snapshots().lock();
    let snapshot = state.snapshot.as_ref().ok_or("Missing snapshot.")?;
    assert_eq!(
        snapshot
            .result
            .model
            .definition(DirId::ROOT, b"VALUE")
            .map(|value| value.as_slice()),
        Some(b"new".as_slice()),
    );
    Ok(())
}

#[test]
fn missing_disk_input_stays_tracked_until_recreated() -> TestResult {
    let project = Project::new();
    project.file(
        "CMakeLists.txt",
        concat!(
            "cmake_minimum_required(VERSION 4.5)\nproject(p NONE)\n",
            "file(READ \"${CMAKE_CURRENT_SOURCE_DIR}/settings.txt\" VALUE)\n",
        ),
    )?;
    project.file("settings.txt", "before")?;
    let provider = project.provider(PreparedConfigure::evaluate);
    let snapshots = provider.snapshots();
    assert_eq!(snapshots.refresh()?, RefreshStatus::Completed);
    let input = project.root.join("settings.txt");
    project.host.remove_file(&input)?;
    assert!(matches!(
        snapshots.refresh(),
        Err(ApiError::ConfigureFailed { .. })
    ));
    assert!(snapshots.disk_paths().contains(&input));
    assert_eq!(snapshots.refresh()?, RefreshStatus::UpToDate);
    project.file("settings.txt", "after")?;
    assert_eq!(snapshots.refresh()?, RefreshStatus::Completed);
    Ok(())
}
