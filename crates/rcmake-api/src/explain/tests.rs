use std::sync::Arc;

use rcmake_diag::{Frame, FrameKind, PolicyMap};
use rcmake_eval::{LedgerLevel, VarWrite};
use rcmake_genex::ConfigId;
use rcmake_model::{AppendMode, CacheEntry, PropLedger, PropOp, TargetOrigin, UsageField};
use rcmake_resolve::{ChainKey, GenerationResult, Hop, ResolveChains};

use super::*;
use crate::{
    ExplainRenderOptions, ResolvedEntry, VarLedgerSnapshot, VariableWriteDetails,
    render_explain_json, render_explain_terminal,
};

fn trace(path: &str, line: u32, command: &str) -> Backtrace {
    Backtrace::default().push(Frame {
        file: Arc::from(path.as_bytes()),
        line,
        name: Some(Arc::from(command.as_bytes())),
        kind: FrameKind::Command,
    })
}

#[test]
fn joins_resolved_entries_to_their_exact_config_and_ordinal()
-> Result<(), Box<dyn std::error::Error>> {
    let mut result = ConfigureResult::default();
    result
        .model
        .set_root_dirs("/project".into(), "/build".into());
    let id = result.model.add_target(Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        trace("/project/CMakeLists.txt", 4, "add_executable"),
        PolicyMap::default(),
    ))?;
    let dependency = result.model.add_target(Target::new(
        "foo".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        trace("/project/foo/CMakeLists.txt", 1, "add_library"),
        PolicyMap::default(),
    ))?;
    let leaf = result.model.add_target(Target::new(
        "bar".into(),
        TargetKind::InterfaceLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        trace("/project/libs/bar/CMakeLists.txt", 1, "add_library"),
        PolicyMap::default(),
    ))?;
    let origin = trace(
        "/project/libs/bar/CMakeLists.txt",
        14,
        "target_compile_definitions",
    );
    let key = ChainKey {
        target: id,
        config: ConfigId("Debug".into()),
        kind: ChainKind::Usage(UsageField::CompileDefinitions),
        ordinal: 1,
    };
    let mut chains = ResolveChains::new();
    chains.insert(
        ChainKey {
            ordinal: 0,
            ..key.clone()
        },
        vec![],
    );
    chains.insert(
        key.clone(),
        vec![
            Hop {
                from: dependency,
                via: UsageScope::Private,
                link_bt: trace("/project/CMakeLists.txt", 8, "target_link_libraries"),
            },
            Hop {
                from: leaf,
                via: UsageScope::Interface,
                link_bt: trace(
                    "/project/libs/foo/CMakeLists.txt",
                    9,
                    "target_link_libraries",
                ),
            },
        ],
    );
    result.facts.resolved_entries = vec![
        ResolvedEntry {
            key: ChainKey {
                ordinal: 0,
                ..key.clone()
            },
            value: BT::new("LOCAL=1".into(), Backtrace::default()),
            link_targets: Vec::new(),
        },
        ResolvedEntry {
            key: key.clone(),
            value: BT::new("FOO=1".into(), origin.clone()),
            link_targets: Vec::new(),
        },
        ResolvedEntry {
            key: ChainKey {
                config: ConfigId("Release".into()),
                ..key
            },
            value: BT::new("FOO=2".into(), origin.clone()),
            link_targets: Vec::new(),
        },
    ];
    let mut properties = PropLedger::default();
    properties.record(
        &PropScope::Target(leaf),
        b"INTERFACE_COMPILE_DEFINITIONS",
        PropEvent::new(
            1,
            PropOp::Append(AppendMode::List),
            None,
            Some(b"FOO=1"),
            &origin,
        ),
    );
    result.model.set_prop_ledger(Some(Box::new(properties)));
    result.generation = Some(GenerationResult {
        chains: Some(Box::new(chains)),
        ..GenerationResult::default()
    });
    let report = result.explain(&ExplainQuery::Auto("app".into()));
    let rendered = render_explain_terminal(
        &report,
        ExplainRenderOptions {
            config: Some(b"debug"),
            ..ExplainRenderOptions::default()
        },
    );
    assert!(rendered.contains("app -> PRIVATE foo -> INTERFACE bar"));
    assert!(rendered.contains("libs/bar/CMakeLists.txt:14"));
    assert!(rendered.contains("CMakeLists.txt:8"));
    assert!(rendered.contains("libs/foo/CMakeLists.txt:9"));
    assert!(rendered.contains("-DLOCAL=1  [direct]"));
    assert!(rendered.contains("Requested configuration debug matched."));
    assert!(rendered.lines().all(|line| line.len() <= 96));
    assert!(rendered.contains("INTERFACE_COMPILE_DEFINITIONS"));
    assert!(!rendered.contains("FOO=2"));
    let json = render_explain_json(&report, Some(b"Release"));
    assert!(json.to_string().contains("\"chain\":null"));
    assert!(!json.to_string().contains("FOO=1"));
    assert!(!json.to_string().contains("INTERFACE_COMPILE_DEFINITIONS"));
    let json = render_explain_json(&report, Some(b"Unknown")).to_string();
    assert!(json.contains(r#""status":"unavailable""#));
    assert!(json.contains(r#""available":["Debug","Release"]"#));
    let filtered =
        result.explain_filtered(&ExplainQuery::Target("app".into()), Some("-DFOO=1".into()));
    let output = render_explain_terminal(
        &filtered,
        ExplainRenderOptions {
            config: Some(b"Debug"),
            ..ExplainRenderOptions::default()
        },
    );
    assert!(output.contains("-DFOO=1"));
    assert!(!output.contains("-DLOCAL=1"));
    assert!(!output.contains("FOO=2"));
    Ok(())
}

#[test]
fn handles_ambiguity_and_truncated_variable_history() -> Result<(), Box<dyn std::error::Error>> {
    let mut result = ConfigureResult::default();
    result.model.add_target(Target::new(
        "item".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    result.model.cache_mut().insert(
        "item".into(),
        CacheEntry {
            value: "new".into(),
            ty: CacheType::String,
            help: "The cache entry.".into(),
            advanced: false,
            props: Default::default(),
        },
    );
    let mut ledger = VarLedgerSnapshot::new(LedgerLevel::Full { per_name_cap: 1 });
    let origin = trace("/project/CMakeLists.txt", 6, "set");
    ledger.record(
        b"item",
        VarWrite {
            seq: 1,
            op: VarOp::CacheSet,
            scope_depth: 1,
            scope_id: 1,
            old_value: None,
            new_value: Some(b"old"),
            backtrace: &origin,
        },
    );
    ledger.record(
        b"item",
        VarWrite {
            seq: 2,
            op: VarOp::CacheSet,
            scope_depth: 1,
            scope_id: 1,
            old_value: Some(b"old"),
            new_value: Some(b"new"),
            backtrace: &origin,
        },
    );
    result.var_ledger = Some(ledger);
    result.facts.cache_force.insert("item".into(), true);
    result.facts.variable_writes.insert(
        2,
        VariableWriteDetails {
            command: Some("set(CACHE)".into()),
            dir: Some(DirId::ROOT),
            ..VariableWriteDetails::default()
        },
    );
    assert!(matches!(
        result.explain(&ExplainQuery::Auto("item".into())),
        ExplainReport::Ambiguous { .. }
    ));
    let ExplainReport::NotFound { suggestions, .. } =
        result.explain(&ExplainQuery::Auto("itme".into()))
    else {
        return Err("unexpected report".into());
    };
    assert_eq!(
        suggestions,
        [
            ExplainSuggestion {
                name: "item".into(),
                kind: ExplainItemKind::Target
            },
            ExplainSuggestion {
                name: "item".into(),
                kind: ExplainItemKind::Variable
            },
        ]
    );
    let ExplainReport::NotFound { suggestions, .. } =
        result.explain(&ExplainQuery::Auto("ite".into()))
    else {
        return Err("unexpected report".into());
    };
    assert!(suggestions.is_empty());
    let report = result.explain(&ExplainQuery::Variable("item".into()));
    let terminal = render_explain_terminal(&report, ExplainRenderOptions::default());
    assert!(terminal.contains("\"old\" -> \"new\""));
    assert!(terminal.contains("/project/CMakeLists.txt:6"));
    assert!(terminal.contains("FORCE"));
    assert!(terminal.contains("history reached its limit"));
    let json = render_explain_json(&report, None);
    assert!(json.to_string().contains(r#""history":"truncated""#));
    Ok(())
}

#[test]
fn keeps_other_scopes_out_of_binding_and_tracks_exact_history_coverage()
-> Result<(), Box<dyn std::error::Error>> {
    let mut result = ConfigureResult::default();
    let root = DirId::ROOT;
    let child = result.model.add_directory(
        root,
        "/project/child".into(),
        "/build/child".into(),
        Backtrace::default(),
    )?;
    result.model.add_target(Target::new(
        "item".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        root,
        Backtrace::default(),
        PolicyMap::default(),
    ))?;
    result.model.add_alias(
        root,
        "extra".into(),
        result
            .model
            .find_target(root, b"item")
            .ok_or("missing target")?,
    )?;
    let origin = trace("/project/child/CMakeLists.txt", 3, "set");
    let mut ledger = VarLedgerSnapshot::with_event_limit(LedgerLevel::Full { per_name_cap: 2 }, 1);
    for (name, seq) in [(b"item".as_slice(), 1), (b"another".as_slice(), 2)] {
        ledger.record(
            name,
            VarWrite {
                seq,
                op: VarOp::Set,
                scope_depth: 1,
                scope_id: 2,
                old_value: None,
                new_value: Some(b"child"),
                backtrace: &origin,
            },
        );
    }
    result.var_ledger = Some(ledger);
    result.facts.variable_writes.insert(
        1,
        VariableWriteDetails {
            dir: Some(child),
            binding_dir: Some(child),
            ..VariableWriteDetails::default()
        },
    );
    assert!(matches!(
        result.explain(&ExplainQuery::Auto("item".into())),
        ExplainReport::Target(_)
    ));
    assert!(matches!(
        result.explain(&ExplainQuery::Variable("item".into())),
        ExplainReport::NotFound { .. }
    ));
    assert!(matches!(
        result.explain(&ExplainQuery::Target("extar".into())),
        ExplainReport::NotFound { suggestions, .. } if suggestions.iter().any(|item| item.name == "extra")
    ));
    let empty = ConfigureResult::default();
    assert!(matches!(
        empty.explain_in(child, &ExplainQuery::Auto("item".into())),
        ExplainReport::InvalidDirectory { .. }
    ));
    let ledger = result.var_ledger.as_ref().ok_or("missing ledger")?;
    assert_eq!(ledger.coverage(b"item"), VariableHistory::Complete);
    assert_eq!(ledger.coverage(b"another"), VariableHistory::Truncated);
    assert_eq!(ledger.coverage(b"never_set"), VariableHistory::Complete);
    assert_eq!(ledger.session_truncated(), Some(true));
    let level = LedgerLevel::Names(BTreeSet::from(["item".into()]));
    let ledger = VarLedgerSnapshot::new(level);
    assert_eq!(ledger.coverage(b"item"), VariableHistory::Complete);
    assert_eq!(ledger.coverage(b"another"), VariableHistory::Excluded);
    Ok(())
}

#[test]
fn preserves_full_values_and_both_binding_origins_in_json() -> Result<(), Box<dyn std::error::Error>>
{
    let mut result = ConfigureResult::default();
    let dir = DirId::ROOT;
    let full = "x".repeat(512);
    result
        .model
        .set_root_dirs("/project".into(), "/build".into());
    result
        .model
        .dir_mut(dir)
        .vars
        .insert("FLAGS".into(), full.clone().into());
    result.model.cache_mut().insert(
        "FLAGS".into(),
        CacheEntry {
            value: "old".into(),
            ty: CacheType::String,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    let source = trace("/project/defs.cmake", 8, "set");
    let cache = trace("/project/cache.cmake", 2, "set");
    result.facts.variable_sites.entry(dir).or_default().insert(
        "FLAGS".into(),
        SourceLocation::from_backtrace(&source).ok_or("missing source")?,
    );
    result.facts.cache_sites.insert(
        "FLAGS".into(),
        SourceLocation::from_backtrace(&cache).ok_or("missing cache source")?,
    );
    let mut ledger = VarLedgerSnapshot::new(LedgerLevel::Full { per_name_cap: 2 });
    ledger.record(
        b"FLAGS",
        VarWrite {
            seq: u64::MAX,
            op: VarOp::Set,
            scope_depth: 1,
            scope_id: 1,
            old_value: None,
            new_value: Some(full.as_bytes()),
            backtrace: &source,
        },
    );
    result.var_ledger = Some(ledger);
    let report = result.explain(&ExplainQuery::Variable("FLAGS".into()));
    let terminal = render_explain_terminal(&report, ExplainRenderOptions::default());
    assert!(terminal.contains("normal variable shadows cache"));
    assert!(terminal.contains("truncated, 512 bytes total"));
    assert!(terminal.contains("defs.cmake:8"));
    assert!(terminal.contains("cache.cmake:2"));
    assert!(terminal.lines().all(|line| line.len() <= 96));
    let json = render_explain_json(&report, None).to_string();
    assert!(json.contains(&full));
    assert!(json.contains(r#""sequence":"18446744073709551615""#));
    assert!(json.contains(r#""truncated":true"#));
    Ok(())
}

#[test]
fn renders_byte_paths_and_uses_color_only_when_requested() -> Result<(), Box<dyn std::error::Error>>
{
    use rcmake_host::{Host, PureHost, Stream};
    use std::path::PathBuf;

    let mut result = ConfigureResult::default();
    let mut path = b"/project/space ".to_vec();
    path.push(0xff);
    let origin = Backtrace::default().push(Frame {
        file: path.into(),
        line: 3,
        name: None,
        kind: FrameKind::Command,
    });
    result.model.add_target(Target::new(
        "app".into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        DirId::ROOT,
        origin,
        PolicyMap::default(),
    ))?;
    let report = result.explain(&ExplainQuery::Target("app".into()));
    let host = PureHost::new(PathBuf::from("/"), PathBuf::from("/rcmake"));
    let redirected = ExplainRenderOptions::for_host(&host, Stream::Stdout);
    assert!(!redirected.color && !redirected.hyperlinks);
    host.env_set(b"NO_COLOR", Some(b"1"));
    host.env_set(b"CLICOLOR_FORCE", Some(b"1"));
    let forced = ExplainRenderOptions::for_host(&host, Stream::Stdout);
    assert!(forced.color && !forced.hyperlinks);
    let plain = render_explain_terminal(&report, redirected);
    assert!(plain.contains(r"space \xff:3"));
    let linked = render_explain_terminal(
        &report,
        ExplainRenderOptions {
            hyperlinks: true,
            ..forced
        },
    );
    assert!(linked.contains("file:///project/space%20%FF#L3"));
    let json = render_explain_json(&report, None).to_string();
    assert!(json.contains(r#""bytes_hex":"2f70726f6a6563742f737061636520ff""#));
    assert!(!json.contains(r"\u001b"));
    Ok(())
}
