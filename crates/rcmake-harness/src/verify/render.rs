//! Human-readable text and machine-readable JSON renderers for `rcmake-verify`.

use serde_json::{Value, json};

use super::compare::{SectionReport, VOLATILE_CACHE_EXCEPTIONS, VerifyReport};

/// Machine-readable report schema version.
pub(crate) const REPORT_SCHEMA_VERSION: u32 = 1;

/// Render a [`VerifyReport`] as pretty-printed JSON.
pub(crate) fn render_json(report: &VerifyReport) -> String {
    let oracle_ms = report.timings.oracle_ns as f64 / 1_000_000.0;
    let rcmake_ms = report.timings.rcmake_ns as f64 / 1_000_000.0;
    let volatile: Vec<Value> = VOLATILE_CACHE_EXCEPTIONS
        .iter()
        .map(|(pattern, reason)| json!({ "pattern": pattern, "reason": reason }))
        .collect();

    let mut payload = json!({
        "schema_version": REPORT_SCHEMA_VERSION,
        "verdict": report.verdict.as_str(),
        "exit_code": report.verdict.exit_code(),
        "engines": {
            "oracle": {
                "executable": report.oracle_exe,
                "version": report.oracle_version,
                "expected_version": "4.5.1"
            },
            "rcmake": {
                "executable": report.rcmake_exe,
                "version": report.rcmake_version,
                "persona": "compat"
            }
        },
        "scope": {
            "generator": report.generator,
            "compile_commands_injected": report.compile_commands_injected,
            "external_inputs": report.external_inputs,
            "volatile_cache_exceptions": volatile
        },
        "timings": {
            "observation": "single_paired_observation",
            "run_order": report.timings.run_order,
            "oracle_ms": round_ms(oracle_ms),
            "rcmake_ms": round_ms(rcmake_ms),
            "speedup": report.timings.speedup.map(round_ratio)
        },
        "sections": {
            "configure": section_json(&report.configure),
            "codemodel": section_json(&report.codemodel),
            "cache": section_json(&report.cache),
            "compile_commands": section_json(&report.compile_commands),
            "ninja_dry_run": section_json(&report.ninja_dry_run)
        }
    });
    if let Some(err) = &report.error
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("error".to_owned(), Value::String(err.clone()));
    }
    if let Some(warn) = &report.cleanup_warning
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("cleanup_warning".to_owned(), Value::String(warn.clone()));
    }
    let mut rendered = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned());
    rendered.push('\n');
    rendered
}

fn section_json(sec: &SectionReport) -> Value {
    json!({
        "status": sec.status.as_str(),
        "compared": sec.compared,
        "differences": sec.differences,
        "examples": sec.examples,
        "notes": sec.notes
    })
}

/// Render a [`VerifyReport`] as concise human-readable text.
pub(crate) fn render_text(report: &VerifyReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "rcmake verify: {} (exit {})\n",
        report.verdict.as_str(),
        report.verdict.exit_code()
    ));
    out.push_str(&format!(
        "  oracle: {} ({})\n",
        report.oracle_version, report.oracle_exe
    ));
    out.push_str(&format!(
        "  rcmake: {} ({}, compat persona)\n",
        report.rcmake_version, report.rcmake_exe
    ));
    if let Some(generator_name) = &report.generator {
        out.push_str(&format!("  generator: {generator_name}\n"));
    }
    if !report.external_inputs.is_empty() {
        out.push_str(&format!(
            "  external inputs (not staged): {}\n",
            report.external_inputs.join(", ")
        ));
    }
    if let Some(err) = &report.error {
        out.push_str(&format!("  error: {err}\n"));
        return out;
    }

    let oracle_ms = report.timings.oracle_ns as f64 / 1_000_000.0;
    let rcmake_ms = report.timings.rcmake_ns as f64 / 1_000_000.0;
    match report.timings.speedup {
        Some(ratio) => out.push_str(&format!(
            "  timing (single paired observation, order: oracle -> rcmake): oracle {oracle_ms:.1} ms, rcmake {rcmake_ms:.1} ms ({ratio:.2}x)\n"
        )),
        None => out.push_str(&format!(
            "  timing (single paired observation, order: oracle -> rcmake): oracle {oracle_ms:.1} ms, rcmake {rcmake_ms:.1} ms\n"
        )),
    }

    for (name, sec) in [
        ("configure", &report.configure),
        ("codemodel", &report.codemodel),
        ("cache", &report.cache),
        ("compile_commands", &report.compile_commands),
        ("ninja_dry_run", &report.ninja_dry_run),
    ] {
        out.push_str(&format!("  {name}: {}", sec.status.as_str()));
        if sec.compared > 0 || sec.differences > 0 {
            out.push_str(&format!(
                " ({} compared, {} diff)",
                sec.compared, sec.differences
            ));
        }
        out.push('\n');
        for note in &sec.notes {
            out.push_str(&format!("    note: {note}\n"));
        }
        for ex in &sec.examples {
            out.push_str(&format!("    diff: {ex}\n"));
        }
    }
    if let Some(warn) = &report.cleanup_warning {
        out.push_str(&format!("  warning: {warn}\n"));
    }
    out
}

fn round_ms(ms: f64) -> f64 {
    (ms * 1000.0).round() / 1000.0
}

fn round_ratio(ratio: f64) -> f64 {
    (ratio * 1000.0).round() / 1000.0
}
