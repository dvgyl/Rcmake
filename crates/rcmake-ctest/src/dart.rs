//! Dashboard files for `-T Test` and dashboard scripts.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::{FileTime, Host, HostError, HostResult, ReadRange, path};

use crate::execution::Status;
use crate::junit::{completion, escape, output, write_file};
use crate::reporting::{TestRecord, format_date, labels, processors};
use crate::selection::Plan;

#[derive(Default)]
pub(crate) struct Configuration {
    pub(crate) site: BString,
    pub(crate) build_name: BString,
    pub(crate) nightly_start: BString,
}

impl Configuration {
    pub(crate) fn load(host: &dyn Host, dir: &Path, overrides: &[BString]) -> HostResult<Self> {
        let mut config = Self::default();
        for name in ["CTestConfiguration.ini", "DartConfiguration.tcl"] {
            match host.read(&dir.join(name), ReadRange::All { limit: 1024 * 1024 }) {
                Ok(bytes) => {
                    for line in bytes.split(|&byte| byte == b'\n') {
                        if let Some((key, value)) = split(line, b':') {
                            config.set(key.trim_ascii(), value.trim_ascii());
                        }
                    }
                    break;
                }
                Err(error) if error.kind == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        for entry in overrides {
            if let Some((key, value)) = split(entry, b'=') {
                config.set(key, value);
            }
        }
        Ok(config)
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        match key {
            b"Site" => self.site = value.into(),
            b"BuildName" => self.build_name = value.into(),
            b"NightlyStartTime" => self.nightly_start = value.into(),
            _ => {}
        }
    }
}

pub(crate) fn split(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let pos = bytes.iter().position(|&byte| byte == delimiter)?;
    let (before, after) = bytes.split_at(pos);
    Some((before, after.get(1..)?))
}

#[derive(Clone)]
pub(crate) struct Tag {
    stamp: String,
    pub(crate) track: String,
    pub(crate) model: Model,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Model {
    Experimental,
    Continuous,
    Nightly,
}

impl Model {
    pub(crate) fn parse(value: &[u8]) -> Option<Self> {
        [Self::Experimental, Self::Continuous, Self::Nightly]
            .into_iter()
            .find(|model| value.eq_ignore_ascii_case(model.name().as_bytes()))
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Experimental => "Experimental",
            Self::Continuous => "Continuous",
            Self::Nightly => "Nightly",
        }
    }
}

impl Tag {
    pub(crate) fn stamp(&self) -> &str {
        &self.stamp
    }

    pub(crate) fn read(host: &dyn Host, dir: &Path) -> HostResult<Self> {
        Self::read_with_model(host, dir, None)
    }

    pub(crate) fn read_with_model(
        host: &dyn Host,
        dir: &Path,
        fallback: Option<Model>,
    ) -> HostResult<Self> {
        let bytes = host.read(&dir.join("Testing/TAG"), ReadRange::All { limit: 4096 })?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.lines();
        let stamp = lines.next().unwrap_or_default();
        let track = lines.next().unwrap_or_default();
        let model = lines
            .next()
            .and_then(|model| Model::parse(model.as_bytes()))
            .or(fallback);
        if stamp.len() != 13
            || !stamp.bytes().enumerate().all(|(index, byte)| {
                if index == 8 {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit()
                }
            })
            || track.is_empty()
        {
            return Err(HostError::unsupported("Invalid Testing/TAG file"));
        }
        let model =
            model.ok_or_else(|| HostError::unsupported("Testing/TAG has no dashboard model"))?;
        Ok(Self {
            stamp: stamp.to_owned(),
            track: track.to_owned(),
            model,
        })
    }

    pub(crate) fn create(
        host: &dyn Host,
        dir: &Path,
        mut time: FileTime,
        model: Model,
        track: Option<&str>,
        nightly_start: &[u8],
    ) -> HostResult<Self> {
        if model == Model::Nightly {
            let seconds = nightly_seconds(nightly_start).ok_or_else(|| {
                HostError::unsupported("NightlyStartTime is required for the Nightly model")
            })?;
            let today = time.secs.saturating_sub(time.secs.rem_euclid(86_400));
            let next = today.saturating_add(seconds);
            time.secs = if next > time.secs {
                next.saturating_sub(86_400)
            } else {
                next
            };
        }
        let stamp = format_date(host, time, true, "%Y%m%d-%H%M");
        if stamp.is_empty() {
            return Err(HostError::unsupported("Invalid dashboard start time"));
        }
        let track = track
            .filter(|track| !track.is_empty())
            .unwrap_or(model.name());
        if track.contains(['\r', '\n']) {
            return Err(HostError::unsupported(
                "Dashboard track contains a line break",
            ));
        }
        let data = format!("{stamp}\n{track}\n{}\n", model.name());
        write_file(host, &dir.join("Testing/TAG"), data.as_bytes())?;
        Ok(Self {
            stamp,
            track: track.to_owned(),
            model,
        })
    }
}

fn nightly_seconds(value: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(value).ok()?;
    let mut parts = text.split_ascii_whitespace();
    let mut hms = parts.next()?.split(':').map(str::parse::<i64>);
    let hours = hms.next()?.ok()?;
    let minutes = hms.next()?.ok()?;
    let seconds = hms.next().transpose().ok()?.unwrap_or(0);
    if !(0..24).contains(&hours)
        || !(0..60).contains(&minutes)
        || !(0..60).contains(&seconds)
        || hms.next().is_some()
    {
        return None;
    }
    let zone = parts.next().unwrap_or("UTC");
    let offset = match zone {
        "UTC" | "GMT" | "Z" => 0,
        "EST" | "CDT" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" | "MDT" => -6 * 3600,
        "MST" | "PDT" => -7 * 3600,
        "PST" => -8 * 3600,
        offset if offset.starts_with(['+', '-']) => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let digits = offset.get(1..)?.replace(':', "");
            if digits.len() != 4 {
                return None;
            }
            let hours = digits.get(..2)?.parse::<i64>().ok()?;
            let minutes = digits.get(2..)?.parse::<i64>().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    Some(hours * 3600 + minutes * 60 + seconds - offset)
}

pub(crate) struct Dashboard {
    pub(crate) directory: PathBuf,
    pub(crate) tag: Tag,
    pub(crate) configuration: Configuration,
    pub(crate) show_project: bool,
}

pub(crate) struct TestRun<'a> {
    pub(crate) plan: &'a Plan,
    pub(crate) statuses: &'a [Option<Status>],
    pub(crate) records: &'a BTreeMap<usize, TestRecord>,
    pub(crate) started: FileTime,
    pub(crate) elapsed: Duration,
}

pub(crate) fn write_test_xml(
    host: &dyn Host,
    dashboard: &Dashboard,
    run: &TestRun<'_>,
) -> HostResult<()> {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site");
    let config = &dashboard.configuration;
    let stamp = format!("{}-{}", dashboard.tag.stamp, dashboard.tag.track);
    let generator = format!("ctest-{}", rcmake_base::CMAKE_VERSION_STR);
    let system = host.system();
    let os_name = if system.sysname == "Darwin" {
        b"macOS".as_slice()
    } else {
        &system.sysname
    };
    let cpu_count = system.cpu_count.to_string();
    for (key, value) in [
        ("BuildName", site_value(&config.build_name)),
        ("BuildStamp", stamp.as_bytes()),
        ("Name", site_value(&config.site)),
        ("Generator", generator.as_bytes()),
        ("CompilerName", b""),
        ("CompilerVersion", b""),
        ("OSName", os_name),
        ("Hostname", system.hostname.as_slice()),
        ("OSRelease", system.release.as_slice()),
        ("OSVersion", system.version.as_slice()),
        ("OSPlatform", system.machine.as_slice()),
        (
            "Is64Bits",
            if cfg!(target_pointer_width = "64") {
                b"1"
            } else {
                b"0"
            },
        ),
        ("NumberOfLogicalCPU", cpu_count.as_bytes()),
    ] {
        let separator = if key == "BuildName" { " " } else { "\n\t" };
        let _ = write!(xml, "{separator}{key}=\"{}\"", escape(value, true));
    }
    xml.push_str("\n\t>\n\t<Testing>\n");
    date_elements(host, &mut xml, "Start", run.started);
    xml.push_str("\t\t<TestList>\n");
    let tests = || {
        run.plan
            .tests
            .iter()
            .zip(run.statuses)
            .filter_map(|(test, status)| {
                let status = (*status)?;
                let dir = path::from_native(&test.directory);
                let base = path::from_native(&dashboard.directory);
                let relative = Lexical::new(&dir)
                    .relative(Lexical::new(&base))
                    .unwrap_or(dir);
                let full_name = format!("{relative}/{}", test.name);
                Some((test, status, relative, full_name))
            })
    };
    for (_, _, _, full_name) in tests() {
        element(&mut xml, 3, "Test", full_name.as_bytes());
    }
    xml.push_str("\t\t</TestList>\n");
    let empty = TestRecord::default();
    for (test, status, relative, full_name) in tests() {
        let record = run.records.get(&test.number).unwrap_or(&empty);
        let xml_status = match status {
            Status::Passed => "passed",
            Status::Failed | Status::Timeout => "failed",
            _ => "notrun",
        };
        let _ = writeln!(xml, "\t\t<Test Status=\"{xml_status}\">");
        element(&mut xml, 3, "Name", &test.name);
        element(&mut xml, 3, "Path", &relative);
        element(&mut xml, 3, "FullName", full_name.as_bytes());
        let command = if status == Status::NotRun {
            ""
        } else {
            &record.command
        };
        element(&mut xml, 3, "FullCommandLine", command.as_bytes());
        if let Some(started) = record.started.filter(|_| !command.is_empty()) {
            let time = format!("{}.{:06}", started.secs, started.nanos / 1000);
            element(&mut xml, 3, "StartTestTime", time.as_bytes());
        }
        xml.push_str("\t\t\t<Results>\n");
        if xml_status == "failed" {
            measurement(
                &mut xml,
                "text/string",
                "Exit Code",
                if status == Status::Timeout {
                    "Timeout"
                } else {
                    "Failed"
                },
            );
            measurement(
                &mut xml,
                "text/string",
                "Exit Value",
                &record.exit_code.unwrap_or_default().to_string(),
            );
        }
        if xml_status != "notrun" {
            measurement(
                &mut xml,
                "numeric/double",
                "Execution Time",
                &format!("{:.6}", record.elapsed.as_secs_f64()),
            );
        }
        measurement(
            &mut xml,
            "numeric/double",
            "Processors",
            &processors(test).to_string(),
        );
        let reason = if matches!(status, Status::Failed | Status::Timeout) {
            "Completed".into()
        } else {
            completion(status, record)
        };
        measurement(&mut xml, "text/string", "Completion Status", &reason);
        measurement(&mut xml, "text/string", "Command Line", command);
        let mut environment = Vec::new();
        if !command.is_empty() {
            environment.push("#CTEST_RESOURCE_GROUP_COUNT=".into());
            environment.extend(test.list(b"ENVIRONMENT"));
        }
        let environment = environment
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        measurement(&mut xml, "text/string", "Environment", &environment);
        xml.push_str("\t\t\t\t<Measurement>\n");
        element(&mut xml, 5, "Value", &output(test, status, record));
        xml.push_str("\t\t\t\t</Measurement>\n\t\t\t</Results>\n");
        let labels = labels(test);
        if !labels.is_empty() {
            xml.push_str("\t\t\t<Labels>\n");
            for label in labels {
                element(&mut xml, 4, "Label", &label);
            }
            xml.push_str("\t\t\t</Labels>\n");
        }
        xml.push_str("\t\t</Test>\n");
    }
    date_elements(host, &mut xml, "End", host.now());
    element(
        &mut xml,
        2,
        "ElapsedMinutes",
        (run.elapsed.as_secs() / 60).to_string().as_bytes(),
    );
    xml.push_str("\t</Testing>\n</Site>\n");
    write_file(
        host,
        &dashboard
            .directory
            .join("Testing")
            .join(&dashboard.tag.stamp)
            .join("Test.xml"),
        xml.as_bytes(),
    )
}

fn site_value(value: &[u8]) -> &[u8] {
    if value.is_empty() { b"(empty)" } else { value }
}

fn date_elements(host: &dyn Host, xml: &mut String, prefix: &str, time: FileTime) {
    let date = format_date(host, time, true, "%b %e %H:%M GMT");
    element(xml, 2, &format!("{prefix}DateTime"), date.as_bytes());
    element(
        xml,
        2,
        &format!("{prefix}TestTime"),
        time.secs.to_string().as_bytes(),
    );
}

fn measurement(xml: &mut String, kind: &str, name: &str, value: &str) {
    let _ = writeln!(
        xml,
        "\t\t\t\t<NamedMeasurement type=\"{kind}\" name=\"{name}\">"
    );
    element(xml, 5, "Value", value.as_bytes());
    xml.push_str("\t\t\t\t</NamedMeasurement>\n");
}

fn element(xml: &mut String, depth: usize, name: &str, value: &[u8]) {
    let _ = writeln!(
        xml,
        "{}<{name}>{}</{name}>",
        "\t".repeat(depth),
        escape(value, false)
    );
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn writes_utc_tag_in_track_then_model_order() -> HostResult<()> {
        let host = PureHost::default();
        let dir = Path::new("/work/build");
        let tag = Tag::create(
            &host,
            dir,
            FileTime {
                secs: 1_700_000_000,
                nanos: 0,
            },
            Model::Nightly,
            Some("My group"),
            b"01:00:00 UTC",
        )?;
        assert_eq!(tag.stamp(), "20231114-0100");
        assert_eq!(
            host.read(&dir.join("Testing/TAG"), ReadRange::All { limit: 4096 })?,
            b"20231114-0100\nMy group\nNightly\n"
        );
        let tag = Tag::read(&host, dir)?;
        assert_eq!(tag.track, "My group");
        assert_eq!(tag.model.name(), "Nightly");
        Ok(())
    }

    #[test]
    fn rejects_a_tag_that_escapes_the_testing_directory() -> HostResult<()> {
        let host = PureHost::default();
        let dir = Path::new("/work/build");
        host.seed_file(&dir.join("Testing/TAG"), b"../../otherxx\nExperimental\n")?;
        assert!(Tag::read(&host, dir).is_err());
        Ok(())
    }
}
