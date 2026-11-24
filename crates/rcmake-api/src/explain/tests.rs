use std::sync::Arc;

use rcmake_diag::{Frame, FrameKind, PolicyMap};
use rcmake_eval::LedgerLevel;
use rcmake_genex::ConfigId;
use rcmake_model::{AppendMode, CacheEntry, PropLedger, PropOp, TargetOrigin};
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
    let origin = trace(
        "/project/foo/CMakeLists.txt",
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
        key.clone(),
        vec![Hop {
            from: dependency,
            via: UsageScope::Private,
            link_bt: trace("/project/CMakeLists.txt", 8, "target_link_libraries"),
        }],
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
        &PropScope::Target(dependency),
        b"INTERFACE_COMPILE_DEFINITIONS",
        PropEvent {
            seq: 1,
            op: PropOp::Append(AppendMode::List),
            origin,
        },
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
    assert!(rendered.contains("via PRIVATE   foo"));
    assert!(rendered.contains("/project/foo/CMakeLists.txt:14"));
    assert!(rendered.contains("INTERFACE_COMPILE_DEFINITIONS"));
    assert!(!rendered.contains("FOO=2"));
    let json = render_explain_json(&report, Some(b"Release"));
    assert!(json.to_string().contains("\"chain\":null"));
    assert!(!json.to_string().contains("FOO=1"));
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
    ledger.record(b"item", 1, VarOp::CacheSet, 1, Some(b"old"), &origin);
    let previous = ledger
        .history(b"item")
        .next()
        .and_then(|event| event.value_preview.clone())
        .ok_or("missing write")?;
    ledger.record(b"item", 2, VarOp::CacheSet, 1, Some(b"new"), &origin);
    result.var_ledger = Some(ledger);
    result.facts.cache_force.insert("item".into(), true);
    result.facts.variable_writes.insert(
        2,
        VariableWriteDetails {
            previous: PreviousValue::Value(previous),
            command: Some("set(CACHE)".into()),
            dir: Some(DirId::ROOT),
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
    assert!(suggestions.is_empty());
    let ExplainReport::NotFound { suggestions, .. } =
        result.explain(&ExplainQuery::Auto("ite".into()))
    else {
        return Err("unexpected report".into());
    };
    assert_eq!(suggestions, [BString::from("item")]);
    let report = result.explain(&ExplainQuery::Variable("item".into()));
    let terminal = render_explain_terminal(&report, ExplainRenderOptions::default());
    assert!(terminal.contains("\"old\" -> \"new\""));
    assert!(terminal.contains("/project/CMakeLists.txt:6"));
    assert!(terminal.contains("FORCE"));
    assert!(terminal.contains("history reached its limit"));
    let json = render_explain_json(&report, None);
    assert!(json.to_string().contains("\"history_incomplete\":true"));
    Ok(())
}
