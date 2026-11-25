use std::ops::Range;

use rcmake_json::Value;

use crate::{
    HoverDoc, Position, SemanticContext, SemanticDefinition, SemanticDiagnostic, SemanticDocument,
    SemanticLocation, SemanticProvider, SemanticResult, SemanticUpdate,
};

use super::EvalSemanticProvider;

impl SemanticProvider for EvalSemanticProvider {
    fn update(&self, update: SemanticUpdate<'_>) {
        self.snapshots.update(update);
    }

    fn hover(&self, context: &SemanticContext<'_>) -> Option<String> {
        self.hover_with_version(context).map(|(text, _)| text)
    }

    fn hover_with_version(&self, context: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
        self.render_hover(context, false)
    }

    fn definition(&self, context: &SemanticContext<'_>) -> Option<SemanticLocation> {
        self.definitions(context).into_iter().next()
    }

    fn hover_doc(&self, context: &SemanticContext<'_>) -> Option<SemanticResult<HoverDoc>> {
        self.query_hover(context, true, |name, version, markdown| {
            let (code, body_md) = hover_parts(&markdown);
            Some(SemanticResult {
                uri: context.uri.to_owned(),
                version: version?,
                value: HoverDoc {
                    title: String::from_utf8_lossy(name).chars().take(160).collect(),
                    body_md,
                    code,
                },
            })
        })?
        .current(&self.snapshots)??
    }

    fn definition_result(
        &self,
        context: &SemanticContext<'_>,
    ) -> Option<SemanticResult<SemanticDefinition>> {
        self.versioned_definition(context)
    }

    fn completions(&self, context: &SemanticContext<'_>) -> Option<SemanticResult<Vec<Value>>> {
        let value = self.completions(context);
        (!value.is_empty()).then(|| SemanticResult {
            uri: context.uri.to_owned(),
            version: context.version,
            value,
        })
    }

    fn diagnostics(
        &self,
        document: &SemanticDocument<'_>,
    ) -> Option<SemanticResult<Vec<SemanticDiagnostic>>> {
        let (value, _) = self.typed_diagnostics(document, None)?;
        Some(SemanticResult {
            uri: document.uri.to_owned(),
            version: document.version,
            value,
        })
    }

    fn code_actions(
        &self,
        document: &SemanticDocument<'_>,
        range: Range<Position>,
    ) -> Option<SemanticResult<Vec<Value>>> {
        let (_, value) = self.typed_diagnostics(document, Some(range))?;
        (!value.is_empty()).then(|| SemanticResult {
            uri: document.uri.to_owned(),
            version: document.version,
            value,
        })
    }
}

fn hover_parts(markdown: &str) -> (Option<String>, String) {
    let provisional = "*(provisional snapshot)*\n\n";
    let (prefix, body) = markdown
        .strip_prefix(provisional)
        .map_or(("", markdown), |body| (provisional, body));
    let Some((fence, after_open)) = body.split_once("cmake\n") else {
        return (None, markdown.to_owned());
    };
    if !body.starts_with('`') || !fence.bytes().all(|byte| byte == b'`') {
        return (None, markdown.to_owned());
    }
    // Keep multi-location hovers in their original order.
    if body.contains("\n\n---\n\n") {
        return (None, markdown.to_owned());
    }
    let close = format!("\n{fence}");
    let Some((code, rest)) = after_open.split_once(&close) else {
        return (None, markdown.to_owned());
    };
    let body_md = format!("{prefix}{}", rest.trim_ascii_start());
    (Some(code.trim_end().to_owned()), body_md)
}
