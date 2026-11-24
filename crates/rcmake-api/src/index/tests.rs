use std::sync::Arc;

use rcmake_diag::{Frame, FrameKind, PolicyMap};
use rcmake_eval::LedgerLevel;
use rcmake_model::{CacheEntry, CacheType, ImportedVisibility, Target, TargetOrigin};

use super::*;
use crate::AliasSite;

fn location(path: &str, line: u32) -> SourceLocation {
    SourceLocation {
        path: path.into(),
        line,
        byte_offset: None,
    }
}

fn trace(path: &str, line: u32, command: &str) -> Backtrace {
    Backtrace::default().push(Frame {
        file: Arc::from(path.as_bytes()),
        line,
        name: Some(Arc::from(command.as_bytes())),
        kind: FrameKind::Command,
    })
}

#[test]
fn scopes_local_aliases_by_their_declaration() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    let root = DirId::ROOT;
    let imported = model.add_target(Target::new(
        "imported".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Imported(ImportedVisibility::Local),
        root,
        trace("/project/CMakeLists.txt", 2, "add_library"),
        PolicyMap::default(),
    ))?;
    let global = model.add_target(Target::new(
        "util".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        root,
        trace("/project/CMakeLists.txt", 3, "add_custom_target"),
        PolicyMap::default(),
    ))?;
    model.add_alias(root, "global_alias".into(), global)?;
    let left = model.add_directory(
        root,
        "/project/shared".into(),
        "/build/left".into(),
        Backtrace::default(),
    )?;
    let right = model.add_directory(
        root,
        "/project/shared".into(),
        "/build/right".into(),
        Backtrace::default(),
    )?;
    model.add_alias(left, "local_alias".into(), imported)?;
    model.add_alias(right, "local_alias".into(), imported)?;
    let nested = model.add_directory(
        left,
        "/project/nested".into(),
        "/build/nested".into(),
        Backtrace::default(),
    )?;
    let mut facts = EvaluationFacts::default();
    for (name, dir, global, line) in [
        ("global_alias", root, true, 4),
        ("local_alias", left, false, 10),
        ("local_alias", right, false, 20),
    ] {
        facts.aliases.push(AliasSite {
            name: name.into(),
            dir,
            global,
            location: location("/project/CMakeLists.txt", line),
        });
    }
    let index = WorkspaceIndex::build(&model, None, None, &facts, false);
    assert!(index.lookup_target(root, b"local_alias").is_none());
    assert_eq!(
        index
            .lookup_target(right, b"global_alias")
            .and_then(|target| target.alias_of.as_ref()),
        Some(&"util".into())
    );
    for (dir, line) in [(left, 10), (right, 20), (nested, 10)] {
        let target = index
            .lookup_target(dir, b"local_alias")
            .ok_or("missing alias")?;
        assert_eq!(target.id, imported);
        assert_eq!(target.location.as_ref().map(|site| site.line), Some(line));
    }
    assert_eq!(index.completions_for_scope(root, b"local").len(), 0);
    assert_eq!(index.completions_for_scope(left, b"local").len(), 1);
    Ok(())
}

#[test]
fn indexes_final_values_and_case_insensitive_command_names() {
    let mut model = ProjectModel::default();
    let root = DirId::ROOT;
    model.cache_mut().insert(
        "VALUE".into(),
        CacheEntry {
            value: "cache".into(),
            ty: CacheType::String,
            help: "Sample value.".into(),
            advanced: false,
            props: Default::default(),
        },
    );
    model.cache_mut().insert(
        "CACHE_ONLY".into(),
        CacheEntry {
            value: "present".into(),
            ty: CacheType::String,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    model
        .dir_mut(root)
        .vars
        .insert("VALUE".into(), "normal".into());
    let site = location("/project/CMakeLists.txt", 5);
    let mut facts = EvaluationFacts::default();
    facts
        .variable_sites
        .entry(root)
        .or_default()
        .insert("VALUE".into(), site.clone());
    let callable = CallableDefinition {
        name: "DoWork".into(),
        kind: UserCommandKind::Function,
        location: site.clone(),
        parameters: vec!["path".into()],
        documentation: Some("Process a file.".into()),
    };
    facts.callables.push(callable.clone());
    facts.callables.push(CallableDefinition {
        kind: UserCommandKind::Macro,
        ..callable
    });
    let mut ledger = VarLedgerSnapshot::new(LedgerLevel::Full { per_name_cap: 8 });
    ledger.record(
        b"CACHE_ONLY",
        1,
        VarOp::CacheSet,
        1,
        Some(b"present"),
        &trace("/project/CMakeLists.txt", 7, "set"),
    );
    let index = WorkspaceIndex::build(&model, None, Some(&ledger), &facts, true);
    assert_eq!(
        index.lookup_variable(root, b"VALUE"),
        Some(&VariableDefinition {
            value: "normal".into(),
            location: Some(site),
            cached: false,
            provisional: true
        }),
    );
    assert_eq!(
        index.lookup_variable(root, b"CACHE_ONLY"),
        Some(&VariableDefinition {
            value: "present".into(),
            location: Some(location("/project/CMakeLists.txt", 7)),
            cached: true,
            provisional: true
        }),
    );
    assert_eq!(
        index.lookup_callable(b"DOWORK").map(|entry| entry.kind),
        Some(UserCommandKind::Macro)
    );
    assert_eq!(
        index.completions_for_scope(root, b"DoW"),
        [Completion {
            name: &"DoWork".into(),
            kind: CompletionKind::Callable
        },]
    );
    assert!(index.lookup_variable(root, b"value").is_none());
}

#[test]
fn resolves_recorded_edges_by_scope_and_command_offset() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ProjectModel::default();
    let root = DirId::ROOT;
    let shared = "/project/read.cmake";
    let child = model.add_directory(
        root,
        "/project/sub".into(),
        "/build/sub".into(),
        trace("/project/CMakeLists.txt", 4, "add_subdirectory"),
    )?;
    for dir in [root, child] {
        model.dir_mut(dir).listfiles.push(shared.into());
    }
    let site = SourceLocation {
        byte_offset: Some(20),
        ..location(shared, 3)
    };
    let edge = |dir, offset, path: &str| FileEdge {
        kind: FileEdgeKind::Include,
        dir,
        site: SourceLocation {
            byte_offset: Some(offset),
            ..site.clone()
        },
        destination: path.into(),
    };
    let facts = EvaluationFacts {
        file_edges: vec![
            edge(root, 20, "/modules/First.cmake"),
            edge(root, 40, "/modules/Second.cmake"),
            edge(root, 20, "/modules/Third.cmake"),
            edge(child, 20, "/vendor/First.cmake"),
        ],
        ..EvaluationFacts::default()
    };
    let index = WorkspaceIndex::build(&model, None, None, &facts, false);
    assert_eq!(index.scopes_for_file(shared.as_bytes()), [root, child]);
    assert_eq!(
        index
            .resolve_include(root, &site)
            .map(|edge| edge.destination.as_slice())
            .collect::<Vec<_>>(),
        [
            b"/modules/First.cmake".as_slice(),
            b"/modules/Third.cmake".as_slice()
        ],
    );
    assert_eq!(
        index
            .resolve_include(child, &site)
            .next()
            .map(|edge| edge.destination.as_slice()),
        Some(b"/vendor/First.cmake".as_slice()),
    );
    assert_eq!(
        index
            .resolve_include(root, &location("/project/CMakeLists.txt", 4))
            .next()
            .map(|edge| edge.destination.as_slice()),
        Some(b"/project/sub/CMakeLists.txt".as_slice()),
    );
    Ok(())
}
