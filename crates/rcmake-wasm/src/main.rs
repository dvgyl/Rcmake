//! JSON protocol for the Rcmake playground.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use rcmake_api::{ExplainQuery, ExplainReport, render_explain_json};
use serde_json::Value;

mod project;
mod response;

fn main() -> io::Result<()> {
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    io::stdout().lock().write_all(&dispatch(&input))
}

fn dispatch(input: &[u8]) -> Vec<u8> {
    let request: Value = match serde_json::from_slice(input) {
        Ok(request) => request,
        Err(error) => {
            return response::failure("INPUT", &error.to_string())
                .to_string()
                .into_bytes();
        }
    };
    let field = |name| request.get(name).and_then(Value::as_str);
    let operation = field("op");
    if !matches!(operation, Some("configure" | "explain")) {
        return response::failure("INPUT", "Specify the configure or explain operation.")
            .to_string()
            .into_bytes();
    }
    let project = project::Project::from_json(&request).and_then(|project| project.evaluate());
    if operation == Some("explain") {
        let name = field("symbol").unwrap_or_default();
        let config = field("config").map(str::as_bytes);
        let report = project.map_or_else(
            |_| ExplainReport::NotFound {
                name: name.into(),
                suggestions: Vec::new(),
            },
            |project| project.result.explain(&ExplainQuery::Auto(name.into())),
        );
        return render_explain_json(&report, config).into();
    }
    let output = match project {
        Ok(project) => response::configure(&project),
        Err(error) => error,
    };
    output.to_string().into_bytes()
}

fn strings(
    value: &Value,
    field: &str,
) -> Result<std::collections::BTreeMap<String, String>, Value> {
    let invalid = || {
        response::failure(
            "INPUT",
            &format!("{field} must be an object with string values."),
        )
    };
    value
        .as_object()
        .ok_or_else(invalid)?
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(invalid)
        })
        .collect()
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn display_source(value: &[u8]) -> String {
    text(
        value
            .strip_prefix(project::SOURCE.as_bytes())
            .and_then(|file| file.strip_prefix(b"/"))
            .unwrap_or(value),
    )
}

#[cfg(test)]
mod tests;
