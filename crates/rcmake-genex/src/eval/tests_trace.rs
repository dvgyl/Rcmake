use rcmake_base::BString;

use crate::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, GenexTrace,
    InterfaceView, TargetHandle, evaluate, evaluate_traced, parse,
};

#[derive(Default)]
struct Host;

impl GenexHost for Host {
    fn find_target(&self, _name: &[u8]) -> Option<TargetHandle> {
        None
    }

    fn target_name(&self, _target: TargetHandle) -> BString {
        BString::default()
    }

    fn raw_property(&self, _target: TargetHandle, _name: &[u8]) -> Option<BString> {
        None
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

fn debug() -> EvalContext {
    EvalContext {
        config: ConfigId("Debug".into()),
        ..EvalContext::default()
    }
}

fn traced(input: &str, ctx: &EvalContext, limit: usize) -> (Result<BString, String>, GenexTrace) {
    let mut trace = GenexTrace::with_limit(limit);
    let result = parse(input.as_bytes())
        .and_then(|ast| evaluate_traced(&ast, ctx, &mut Host, &mut trace))
        .map(|(value, _)| value)
        .map_err(|error| error.to_string());
    (result, trace)
}

#[test]
fn nested_steps_are_pre_order_with_parent_links() {
    let (value, trace) = traced("$<$<CONFIG:Debug>:x>", &debug(), 16);
    assert_eq!(value, Ok(BString::from("x")));
    let shape: Vec<_> = trace
        .steps
        .iter()
        .map(|step| (step.id, step.parent, step.depth, step.operator.to_string()))
        .collect();
    assert_eq!(
        shape,
        [
            (0, None, 1, "1".to_owned()),
            (1, Some(0), 2, "CONFIG".to_owned())
        ]
    );
    assert_eq!(trace.steps[0].source, "$<$<CONFIG:Debug>:x>");
    assert_eq!(trace.steps[1].result, Ok(BString::from("1")));
    assert!(!trace.truncated());
}

#[test]
fn short_circuit_records_literal_and_expression_operands() {
    let (value, trace) = traced("$<AND:0,$<TARGET_PROPERTY:missing,X>,lit>", &debug(), 16);
    assert_eq!(value, Ok(BString::from("0")));
    assert_eq!(trace.steps.len(), 1, "skipped operands have no step");
    let skipped: Vec<_> = trace
        .skipped
        .iter()
        .map(|operand| (operand.parent, operand.index, operand.literal))
        .collect();
    assert_eq!(skipped, [(Some(0), 1, false), (Some(0), 2, true)]);
    assert_eq!(trace.skipped[0].source, "$<TARGET_PROPERTY:missing,X>");

    let (value, trace) = traced("$<IF:1,yes,$<CONFIG:Release>>", &debug(), 16);
    assert_eq!(value, Ok(BString::from("yes")));
    assert_eq!(trace.skipped.len(), 1);
    assert_eq!(trace.skipped[0].index, 2);
    assert!(!trace.skipped[0].literal);
}

#[test]
fn operator_error_is_recorded_on_failing_step_and_parents() {
    let (value, trace) = traced("$<1:$<NOT:x>>", &debug(), 16);
    assert!(value.is_err());
    assert_eq!(trace.steps.len(), 2);
    assert!(trace.steps.iter().all(|step| step.result.is_err()));
}

#[test]
fn step_limit_truncates_but_keeps_the_value() {
    let input = format!("{}x{}", "$<1:".repeat(100), ">".repeat(100));
    let ast = parse(input.as_bytes()).expect("parse");
    let plain = evaluate(&ast, &debug(), &mut Host).expect("plain");
    let (value, trace) = traced(&input, &debug(), 40);
    assert_eq!(value, Ok(plain.0));
    assert_eq!(trace.steps.len(), 40);
    assert_eq!(trace.omitted, 60);
    assert!(trace.truncated());
}

#[test]
fn host_operators_are_marked() {
    let (_, trace) = traced("$<TARGET_EXISTS:nope>", &debug(), 16);
    assert!(trace.steps[0].host);
    let (_, trace) = traced("$<CONFIG:Debug>", &debug(), 16);
    assert!(!trace.steps[0].host);
}

#[test]
fn cross_config_operators_are_flagged_past_the_step_limit() {
    let (_, trace) = traced("$<1:$<1:$<COMMAND_CONFIG:x>>>", &debug(), 1);
    assert_eq!(trace.steps.len(), 1);
    assert!(trace.command_config_used);
    assert!(!trace.output_config_used);
    let (_, trace) = traced("$<OUTPUT_CONFIG:x>", &debug(), 0);
    assert!(trace.steps.is_empty());
    assert!(trace.output_config_used);
    assert!(!trace.command_config_used);
}
