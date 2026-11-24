use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::DiagCategory;
use rcmake_eval::Interp;
use rcmake_host::{FileTime, Host, WriteMode, path};
use rcmake_json::Value;

use crate::object;

pub(crate) struct Invocation<'a> {
    pub command: &'a [u8],
    pub start: FileTime,
    pub exit_code: i32,
    pub directories: Option<(&'a Path, &'a Path)>,
}

pub(crate) fn take_results(interp: &Interp) -> Vec<Value> {
    let mut document = interp.diag_engine().sarif_json();
    run(&mut document)
        .and_then(|run| run.remove(b"results".as_slice()))
        .and_then(|results| match results {
            Value::Array(values) => Some(values),
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) fn write(
    host: &dyn Host,
    output: &Path,
    interp: &Interp,
    mut results: Vec<Value>,
    invocation: Invocation<'_>,
) -> bool {
    let mut document = interp.diag_engine().sarif_json();
    if let Some(run) = run(&mut document) {
        if let Some(Value::Array(current)) = run.get_mut(b"results".as_slice()) {
            results.append(current);
            let rules = rule_table(&mut results);
            *current = results;
            if let Some(Value::Object(tool)) = run.get_mut(b"tool".as_slice())
                && let Some(Value::Object(driver)) = tool.get_mut(b"driver".as_slice())
                && !rules.is_empty()
            {
                driver.insert("rules".into(), Value::Array(rules));
            }
        }
        if let Some((source, binary)) = invocation.directories {
            let uri = |dir: &Path| {
                let mut uri = BString::from("file://");
                uri.extend_from_slice(&path::from_native(dir));
                uri.push(b'/');
                object([("uri", Value::String(uri))])
            };
            run.insert(
                "originalUriBaseIds".into(),
                object([
                    ("CMAKE_BINARY_DIR", uri(binary)),
                    ("CMAKE_SOURCE_DIR", uri(source)),
                ]),
            );
        }
        let location = object([("uri", Value::String(invocation.command.into()))]);
        run.insert(
            "invocations".into(),
            Value::Array(vec![object([
                ("endTimeUtc", Value::string(timestamp(host.now()))),
                ("executableLocation", location),
                (
                    "executionSuccessful",
                    Value::Bool(invocation.exit_code == 0),
                ),
                ("exitCode", Value::Int(i64::from(invocation.exit_code))),
                ("startTimeUtc", Value::string(timestamp(invocation.start))),
            ])]),
        );
    }
    let json = rcmake_json::write_styled(&document, "  ");
    if let Err(err) = host.write(output, json.as_slice(), WriteMode::Truncate) {
        crate::emit_cli_error(
            host,
            format!(
                "Failed to write SARIF output to \"{}\": {err}",
                output.display()
            ),
        );
        return false;
    }
    true
}

fn run(document: &mut Value) -> Option<&mut BTreeMap<BString, Value>> {
    let Value::Object(root) = document else {
        return None;
    };
    let Value::Array(runs) = root.get_mut(b"runs".as_slice())? else {
        return None;
    };
    let Value::Object(run) = runs.first_mut()? else {
        return None;
    };
    Some(run)
}

fn rule_table(results: &mut [Value]) -> Vec<Value> {
    let mut rules = Vec::new();
    let mut indexes = BTreeMap::new();
    for result in results {
        let Value::Object(result) = result else {
            continue;
        };
        let Some(Value::String(id)) = result.get(b"ruleId".as_slice()) else {
            continue;
        };
        let index = *indexes.entry(id.clone()).or_insert_with(|| {
            let index = rules.len() as u64;
            let name = DiagCategory::ALL
                .iter()
                .find(|category| id == category.sarif_rule_id())
                .map_or_else(
                    || match id.as_slice() {
                        b"CMake.Warning" => "CMake Warning",
                        b"CMake.FatalError" => "CMake Error",
                        _ => "CMake",
                    },
                    |category| category.as_str(),
                );
            rules.push(object([
                ("id", Value::String(id.clone())),
                ("name", Value::string(name)),
            ]));
            index
        });
        result.insert("ruleIndex".into(), Value::UInt(index));
    }
    rules
}

fn timestamp(time: FileTime) -> String {
    jiff::Timestamp::from_second(time.secs)
        .map(|time| time.to_string())
        .unwrap_or_default()
}
