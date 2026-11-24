use rcmake_base::{BString, Limits};
use rcmake_genex::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, GenexError, GenexHost, InterfaceView,
    TargetHandle, evaluate, genex_strip, parse,
};

use super::{Check, mutate, pick, preview, random_bytes, require};
use crate::Rng;

pub(super) fn check(rng: &mut Rng, limits: &Limits) -> Check {
    let (input, valid) = match rng.below(16) {
        0 => (random_bytes(rng), false),
        1 => {
            let depth = *pick(rng, &[127, 128, 129, 999, 1000, 1001]);
            (
                format!("{}x{}", "$<BOOL:".repeat(depth), ">".repeat(depth)).into_bytes(),
                false,
            )
        }
        2 => {
            let depth = rng.below(5);
            let mut input = tree(rng, depth).into_bytes();
            mutate(rng, &mut input, b"$<\xff,");
            (input, false)
        }
        3 => (b"keep $<IF:1,incomplete,$<BOOL:OFF>".to_vec(), false),
        4 if rng.below(16) == 0 => (b"$$<CONFIG><BOOL:ON>".to_vec(), false),
        _ => {
            let depth = rng.below(6);
            (tree(rng, depth).into_bytes(), true)
        }
    };
    evaluate_input(&input, limits, valid)
}

fn tree(rng: &mut Rng, depth: u64) -> String {
    if depth == 0 {
        return (*pick(
            rng,
            &[
                "ON",
                "OFF",
                "a;b",
                "résumé",
                "$<CONFIG>",
                "$<TARGET_PROPERTY:app,NAME>",
                "$<SEMICOLON>",
            ],
        ))
        .into();
    }
    let child = tree(rng, depth - 1);
    match rng.below(4) {
        0 => format!("$<BOOL:{child}>"),
        1 => format!("$<LOWER_CASE:{child}>"),
        2 => format!(
            "$<IF:$<BOOL:{child}>,{},{}>",
            tree(rng, depth - 1),
            tree(rng, depth - 1)
        ),
        _ => format!("$<STREQUAL:{child},{}>", tree(rng, depth - 1)),
    }
}

fn evaluate_input(input: &[u8], limits: &Limits, valid: bool) -> Check {
    let stripped = genex_strip(input);
    let again = genex_strip(&stripped);
    require(again == stripped, || {
        format!(
            "GENEX_STRIP changed on the second pass. Input {}. First {}. Second {}.",
            preview(input),
            preview(&stripped),
            preview(&again),
        )
    })?;

    let context = EvalContext {
        config: ConfigId("Debug".into()),
        ..EvalContext::default()
    };
    let result = parse(input).and_then(|ast| evaluate(&ast, &context, &mut Stub(limits)));
    match result {
        Ok((output, _)) => {
            require(output.len() <= limits.max_value_bytes, || {
                format!(
                    "Generator expression returned {} bytes. Input {}.",
                    output.len(),
                    preview(input)
                )
            })?;
            Ok(true)
        }
        Err(error) if valid => Err(format!(
            "Generated expression failed. {error} Input {}.",
            preview(input)
        )),
        Err(_) => Ok(false),
    }
}

struct Stub<'a>(&'a Limits);

impl GenexHost for Stub<'_> {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        (name == b"app").then_some(TargetHandle(0))
    }

    fn target_name(&self, _: TargetHandle) -> BString {
        "app".into()
    }

    fn raw_property(&self, _: TargetHandle, name: &[u8]) -> Option<BString> {
        (name == b"NAME").then(|| "app".into())
    }

    fn transitive_property(
        &mut self,
        _: TargetHandle,
        _: &[u8],
        _: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        Ok(InterfaceView::default())
    }

    fn artifact(
        &self,
        _: TargetHandle,
        _: ArtifactKind,
        _: ArtifactPart,
        _: &ConfigId,
    ) -> Option<BString> {
        None
    }

    fn link_closure(
        &mut self,
        _: TargetHandle,
        _: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        Ok(Vec::new())
    }

    fn definition(&self, _: &[u8]) -> Option<BString> {
        None
    }

    fn limits(&self) -> Limits {
        self.0.clone()
    }
}
