//! JSON protocol for the Rcmake playground.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use rcmake_api::{ExplainQuery, ExplainReport, render_explain_json};
use rcmake_json::{ReadOptions, Value, parse, write_compact};

mod project;
mod response;

const REQUEST_OPTIONS: ReadOptions = ReadOptions {
    allow_comments: false,
    dup_keys_last_wins: true,
};

fn main() -> io::Result<()> {
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    io::stdout().lock().write_all(&dispatch(&input))
}

fn dispatch(input: &[u8]) -> Vec<u8> {
    // String fields that are not UTF-8 would otherwise read as absent.
    if let Err(error) = std::str::from_utf8(input) {
        let message = format!(
            "The request is not valid UTF-8 at byte {}.",
            error.valid_up_to()
        );
        return write_compact(&response::failure("INPUT", &message)).into();
    }
    let request = match parse(input, &REQUEST_OPTIONS) {
        Ok(request) => request,
        Err(error) => {
            return write_compact(&response::failure("INPUT", &error.to_string())).into();
        }
    };
    let field = |name| request.get(name).and_then(Value::as_str);
    let operation = field("op");
    if !matches!(operation, Some("configure" | "explain")) {
        return write_compact(&response::failure(
            "INPUT",
            "Specify the configure or explain operation.",
        ))
        .into();
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
    write_compact(&output).into()
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
            let key = std::str::from_utf8(key.as_slice()).map_err(|_| invalid())?;
            value
                .as_str()
                .map(|value| (key.to_owned(), value.to_owned()))
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
