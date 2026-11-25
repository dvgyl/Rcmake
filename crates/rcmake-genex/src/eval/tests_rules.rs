//! Tests for `$<RULE_PROPERTY>`, CMP0085, and diagnostic texts.

use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_diag::{Backtrace, Frame, FrameKind};

use crate::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, EvalFlags, GenexError, GenexHost,
    InterfaceView, RuleProperty, TargetHandle, evaluate, parse,
};

const HEAD: TargetHandle = TargetHandle(1);
const UTILITY: TargetHandle = TargetHandle(2);

#[derive(Default)]
struct Host {
    rules: HashMap<BString, HashMap<BString, BString>>,
    policies: HashMap<BString, bool>,
}

impl Host {
    fn rule(&mut self, name: &str, props: &[(&str, &str)]) {
        let mut map: HashMap<BString, BString> = props
            .iter()
            .map(|(key, value)| (BString::from(*key), BString::from(*value)))
            .collect();
        map.entry("NAME".into()).or_insert_with(|| name.into());
        self.rules.insert(name.into(), map);
    }
}

impl GenexHost for Host {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        match name {
            b"head" => Some(HEAD),
            b"utility" => Some(UTILITY),
            _ => None,
        }
    }

    fn target_name(&self, target: TargetHandle) -> BString {
        if target == HEAD { "head" } else { "utility" }.into()
    }

    fn rule_property(&self, rule: &[u8], property: &[u8]) -> Option<RuleProperty> {
        let props = self.rules.get(rule)?;
        Some(RuleProperty {
            rule_name: props.get(b"NAME".as_slice()).cloned().unwrap_or_default(),
            value: props.get(property).cloned(),
        })
    }

    fn directory_policy(&self, policy: &[u8], _ctx: &EvalContext) -> Option<bool> {
        self.policies.get(policy).copied()
    }

    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString> {
        (name == b"TYPE").then(|| {
            if target == HEAD {
                "EXECUTABLE"
            } else {
                "UTILITY"
            }
            .into()
        })
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
}

fn run(
    input: &str,
    ctx: &EvalContext,
    host: &mut Host,
) -> Result<(BString, EvalFlags), GenexError> {
    evaluate(&parse(input.as_bytes())?, ctx, host)
}

fn value(input: &str, host: &mut Host) -> BString {
    let result = run(input, &EvalContext::default(), host);
    assert!(result.is_ok(), "{input}: {result:?}");
    result.map(|(value, _)| value).unwrap_or_default()
}

fn error(input: &str, ctx: &EvalContext, host: &mut Host) -> String {
    let result = run(input, ctx, host);
    assert!(
        result.is_err(),
        "{input}: expected an error, got {result:?}"
    );
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

#[test]
fn rule_property_reads_and_evaluates_rule_properties() {
    let mut host = Host::default();
    host.rule(
        "gen_1",
        &[
            ("NAME", "gen"),
            ("VERBATIM", "1"),
            ("COMMENT", "$<1:evaluated>"),
        ],
    );
    assert_eq!(value("$<RULE_PROPERTY:gen_1,NAME>", &mut host), "gen");
    assert_eq!(value("$<RULE_PROPERTY:gen_1,VERBATIM>", &mut host), "1");
    assert_eq!(
        value("$<RULE_PROPERTY:gen_1,COMMENT>", &mut host),
        "evaluated"
    );
    assert_eq!(value("$<RULE_PROPERTY:gen_1,VOID>", &mut host), "");
}

#[test]
fn rule_property_reports_argument_and_reference_errors() {
    let mut host = Host::default();
    host.rule(
        "foo_1",
        &[
            ("NAME", "foo"),
            ("COMPILE_OPTIONS", "$<RULE_PROPERTY:foo_1,COMPILE_OPTIONS>"),
        ],
    );
    let ctx = EvalContext::default();
    assert!(
        error("$<RULE_PROPERTY:>", &ctx, &mut host)
            .contains("requires 2 comma separated parameters, but got 1 instead.")
    );
    assert!(
        error("$<RULE_PROPERTY:,FOO>", &ctx, &mut host)
            .contains("$<RULE_PROPERTY:rule,prop> expression requires a non-empty rule name.")
    );
    assert!(
        error("$<RULE_PROPERTY:foo_1,>", &ctx, &mut host)
            .contains("$<RULE_PROPERTY:src,prop> expression requires a non-empty property name.")
    );
    let text = error("$<RULE_PROPERTY:foo_1,COMPILE_OPTIONS>", &ctx, &mut host);
    assert!(text.starts_with("CMake Error:\n"), "{text}");
    assert!(text.contains("Self reference on rule \"foo\"."), "{text}");
    assert!(error("$<RULE_PROPERTY:missing,NAME>", &ctx, &mut host).contains("missing"));
}

#[test]
fn in_list_follows_cmp0085() {
    let input = "$<IN_LIST:,>$<IN_LIST:,a>$<IN_LIST:,;a>$<IN_LIST:a,>$<IN_LIST:a,a>$<IN_LIST:a,;a>";
    let mut host = Host::default();
    host.policies.insert("CMP0085".into(), false);
    assert_eq!(value(input, &mut host), "000011");
    host.policies.insert("CMP0085".into(), true);
    assert_eq!(value(input, &mut host), "101011");
}

#[test]
fn custom_command_contexts_reject_compile_features_and_target_policy() {
    let mut host = Host::default();
    let custom_target = EvalContext {
        head: Some(UTILITY),
        current: Some(UTILITY),
        ..EvalContext::default()
    };
    let message = "may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target.";
    assert!(error("$<COMPILE_FEATURES:cxx_final>", &custom_target, &mut host).contains(message));
    assert!(
        error(
            "$<COMPILE_FEATURES:cxx_final>",
            &EvalContext::default(),
            &mut host
        )
        .contains(message)
    );
    assert!(
        error(
            "$<TARGET_POLICY:CMP0004>",
            &EvalContext::default(),
            &mut host
        )
        .contains(message)
    );
}

#[test]
fn filter_reports_its_operator_name() {
    let mut host = Host::default();
    let text = error("$<FILTER:,RM,>", &EvalContext::default(), &mut host);
    assert!(
        text.contains("$<FILTER:...> second parameter must be either INCLUDE or EXCLUDE"),
        "{text}"
    );
}

#[test]
fn located_errors_omit_parent_directory_frames() {
    let file = |name: &str| -> std::sync::Arc<[u8]> { name.as_bytes().into() };
    let frame = |path: &str, line: u32, name: &str| Frame {
        file: file(path),
        line,
        name: Some(file(name)),
        kind: FrameKind::Command,
    };
    let backtrace = Backtrace::default()
        .push(frame("CMakeLists.txt", 3, "include"))
        .push(frame("Test.cmake", 2, "add_subdirectory"))
        .push(frame("sub/CMakeLists.txt", 2, "include_directories"));
    let error = GenexError::Message {
        expression: "$<BAD>".into(),
        message: "Bad.".into(),
    }
    .at(&backtrace, b"");
    let text = error.to_string();
    assert!(
        text.starts_with("CMake Error at sub/CMakeLists.txt:2 (include_directories):"),
        "{text}"
    );
    assert!(!text.contains("Call Stack"), "{text}");
    assert!(!text.contains("add_subdirectory"), "{text}");
}
