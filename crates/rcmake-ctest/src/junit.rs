//! JUnit XML reports for `--output-junit` and `ctest_test(OUTPUT_JUNIT)`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use rcmake_base::list;
use rcmake_host::{Host, HostResult, WriteMode};

use crate::dart::Configuration;
use crate::execution::{SkipReason, Status};
use crate::reporting::{TestRecord, format_date, labels};
use crate::selection::{Plan, PlannedTest};

pub(crate) fn write(
    host: &dyn Host,
    file: &Path,
    configuration: &Configuration,
    plan: &Plan,
    statuses: &[Option<Status>],
    records: &BTreeMap<usize, TestRecord>,
    elapsed: Duration,
) -> HostResult<()> {
    let tests = statuses.iter().flatten().count();
    let count = |matches: fn(Status) -> bool| {
        statuses
            .iter()
            .flatten()
            .copied()
            .filter(|&status| matches(status))
            .count()
    };
    let failures = count(|status| matches!(status, Status::Failed | Status::Timeout));
    let disabled = count(|status| status == Status::Disabled);
    let skipped = count(|status| matches!(status, Status::Skipped | Status::NotRun));
    let name = if configuration.build_name.is_empty() {
        "(empty)".into()
    } else {
        escape(&configuration.build_name, true)
    };
    let site = escape(&configuration.site, true);
    let timestamp = format_date(host, host.now(), false, "%Y-%m-%dT%H:%M:%S");
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"{name}\"\n\ttests=\"{tests}\"\n\tfailures=\"{failures}\"\n\tdisabled=\"{disabled}\"\n\tskipped=\"{skipped}\"\n\thostname=\"{site}\"\n\ttime=\"{}\"\n\ttimestamp=\"{timestamp}\"\n\t>\n",
        elapsed.as_secs(),
    );
    let empty = TestRecord::default();
    for (test, status) in plan.tests.iter().zip(statuses) {
        if let Some(status) = status {
            testcase(
                &mut xml,
                test,
                *status,
                records.get(&test.number).unwrap_or(&empty),
            );
        }
    }
    xml.push_str("</testsuite>\n");
    write_file(host, file, xml.as_bytes())
}

fn testcase(xml: &mut String, test: &PlannedTest, status: Status, record: &TestRecord) {
    let name = escape(&test.name, true);
    let time = significant_seconds(record.elapsed);
    let (state, element) = match status {
        Status::Passed => ("run", None),
        Status::Disabled => ("disabled", None),
        Status::Skipped | Status::NotRun => ("notrun", Some("skipped")),
        Status::Failed | Status::Timeout => ("fail", Some("failure")),
    };
    let _ = writeln!(
        xml,
        "\t<testcase name=\"{name}\" classname=\"{name}\" time=\"{time}\" status=\"{state}\">"
    );
    if let Some(element) = element {
        let reason = completion(status, record);
        let reason = escape(reason.as_bytes(), true);
        let _ = writeln!(xml, "\t\t<{element} message=\"{reason}\"/>");
    }
    let labels = list::join(labels(test));
    if labels.is_empty() {
        xml.push_str("\t\t<properties/>\n");
    } else {
        let labels = escape(&labels, true);
        let _ = writeln!(
            xml,
            "\t\t<properties>\n\t\t\t<property name=\"cmake_labels\" value=\"{labels}\"/>\n\t\t</properties>"
        );
    }
    let output = output(test, status, record);
    let _ = writeln!(
        xml,
        "\t\t<system-out>{}</system-out>\n\t</testcase>",
        escape(&output, false)
    );
}

pub(crate) fn completion(status: Status, record: &TestRecord) -> String {
    match status {
        Status::Passed => "Completed".into(),
        Status::Failed => "Failed".into(),
        Status::Timeout => "Timeout".into(),
        Status::Disabled => "Disabled".into(),
        Status::NotRun
            if record
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("Failed test dependencies:")) =>
        {
            "Fixture dependency failed".into()
        }
        Status::NotRun
            if record.detail.as_deref()
                == Some("The test is unavailable in this configuration.") =>
        {
            "Missing Configuration".into()
        }
        Status::NotRun => "Unable to find executable".into(),
        Status::Skipped => match record.skip_reason {
            Some(SkipReason::ReturnCode(code)) => format!("SKIP_RETURN_CODE={code}"),
            Some(SkipReason::RegularExpression) => "SKIP_REGULAR_EXPRESSION_MATCHED".into(),
            None => "Skipped".into(),
        },
    }
}

pub(crate) fn output<'a>(
    test: &PlannedTest,
    status: Status,
    record: &'a TestRecord,
) -> Cow<'a, [u8]> {
    match status {
        Status::Disabled => b"Disabled".as_slice().into(),
        Status::NotRun if !record.output.is_empty() => record.output.as_slice().into(),
        Status::NotRun if test.command.program == "NOT_AVAILABLE" => {
            b"Test not available without configuration.  (Missing \"-C <config>\"?)"
                .as_slice()
                .into()
        }
        Status::NotRun => {
            if let Some(detail) = record
                .detail
                .as_deref()
                .filter(|detail| detail.starts_with("Failed test dependencies:"))
            {
                return detail.as_bytes().into();
            }
            format!("Unable to find executable: {}", test.command.program)
                .into_bytes()
                .into()
        }
        _ => record.output.as_slice().into(),
    }
}

fn significant_seconds(duration: Duration) -> String {
    let value = duration.as_secs_f64();
    if value == 0.0 {
        return "0".into();
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or_default();
    let trim_fraction = |mut value: String| {
        if value.contains('.') {
            value.truncate(value.trim_end_matches('0').trim_end_matches('.').len());
        }
        value
    };
    if !(-4..6).contains(&exponent) {
        return format!("{}e{exponent:+03}", trim_fraction(mantissa.into()));
    }
    trim_fraction(format!("{value:.*}", (5 - exponent) as usize))
}

pub(crate) fn escape(bytes: &[u8], attribute: bool) -> String {
    let mut result = String::new();
    for ch in String::from_utf8_lossy(bytes).chars() {
        let entity = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute => "&quot;",
            '\n' if attribute => "&#10;",
            '\t' if attribute => "&#9;",
            '\r' => "&#13;",
            '\x00'..='\x08' | '\x0B'..='\x0C' | '\x0E'..='\x1F' | '\u{fffe}' | '\u{ffff}' => {
                let _ = write!(result, "&lt;{}&gt;", ch as u32);
                continue;
            }
            _ => {
                result.push(ch);
                continue;
            }
        };
        result.push_str(entity);
    }
    result
}

pub(crate) fn write_file(host: &dyn Host, file: &Path, data: &[u8]) -> HostResult<()> {
    if let Some(parent) = file.parent() {
        host.create_dir_all(parent)?;
    }
    host.write(file, data, WriteMode::Truncate)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::{ProcessResult, PureHost, ReadRange, RunOutput};

    use super::*;

    #[test]
    fn writes_junit_for_each_status_and_overwrites_the_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/CTestTestfile.cmake"), br#"
            add_test([=[pass<&"]=] runner pass)
            add_test(fail runner fail)
            add_test(disabled runner disabled)
            add_test(missing runner missing)
            add_test(skipped runner skipped)
            set_tests_properties([=[pass<&"]=] PROPERTIES LABELS "z;a;a")
            set_tests_properties(disabled PROPERTIES DISABLED ON)
            set_tests_properties(skipped PROPERTIES SKIP_RETURN_CODE 4 SKIP_REGULAR_EXPRESSION "match")
        "#)?;
        host.set_command_handler(|host, request, _| {
            host.sleep(Duration::from_millis(125));
            let (result, text): (_, &[u8]) = match request.pipeline[0].args[0].as_slice() {
                b"pass" => (ProcessResult::Exited(0), b"<quoted> \"text\" & stuff\n"),
                b"fail" => (ProcessResult::Exited(9), b""),
                b"skipped" => (ProcessResult::Exited(4), b"match"),
                b"missing" => (
                    ProcessResult::SpawnFailed(rcmake_host::HostError::unsupported(
                        "Missing executable",
                    )),
                    b"",
                ),
                _ => return Err(rcmake_host::HostError::unsupported("Unexpected test")),
            };
            Ok(RunOutput {
                results: vec![result],
                stdout: text.to_vec(),
                stderr: Vec::new(),
            })
        })?;
        let file = Path::new("/work/results/reports/junit.xml");
        let run = |extra: &[&str]| {
            let argv = ["ctest", "-Q", "--output-junit", "results/reports/junit.xml"];
            crate::main(
                &argv
                    .into_iter()
                    .chain(extra.iter().copied())
                    .map(BString::from)
                    .collect::<Vec<_>>(),
                host.clone(),
            )
        };
        assert_eq!(run(&[]), 8);
        let xml = String::from_utf8(host.read(file, ReadRange::All { limit: 16_384 })?)?;
        for expected in [
            "<testsuite name=\"(empty)\"\n\ttests=\"5\"\n\tfailures=\"1\"\n\tdisabled=\"1\"\n\tskipped=\"2\"",
            "name=\"pass&lt;&amp;&quot;\" classname=\"pass&lt;&amp;&quot;\" time=\"0.125\" status=\"run\"",
            "<property name=\"cmake_labels\" value=\"a;z\"/>",
            "<system-out>&lt;quoted&gt; \"text\" &amp; stuff\n</system-out>",
            "<failure message=\"Failed\"/>",
            "status=\"disabled\"",
            "<skipped message=\"Unable to find executable\"/>",
            "<skipped message=\"SKIP_RETURN_CODE=4\"/>",
        ] {
            assert!(xml.contains(expected), "Missing {expected} in {xml}");
        }
        assert_eq!(run(&["-R", "^pass"]), 0);
        let xml = String::from_utf8(host.read(file, ReadRange::All { limit: 16_384 })?)?;
        assert_eq!(xml.matches("<testcase ").count(), 1);
        Ok(())
    }
}
