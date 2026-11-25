use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_host::Stream;
use rcmake_host::{GracefulTimeout, TimeoutSignal};
use rcmake_regex::{Regex, RegexError};

#[derive(Default)]
pub(crate) struct Settings {
    pub(crate) errors: Vec<String>,
    signal: Option<TimeoutSignal>,
    grace: Option<Duration>,
    after_match: Option<(Duration, Regex)>,
}

impl Settings {
    pub(crate) fn apply(&mut self, key: &[u8], value: &BString) {
        let result = match key {
            b"TIMEOUT_SIGNAL_NAME" => signal(value).map(|signal| self.signal = Some(signal)),
            b"TIMEOUT_SIGNAL_GRACE_PERIOD" => grace(value).map(|grace| self.grace = Some(grace)),
            _ => return,
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    pub(crate) fn set_after_match(
        &mut self,
        delay: &[u8],
        pattern: &[u8],
    ) -> Result<(), RegexError> {
        self.after_match = None;
        let seconds = crate::cli::number_prefix(delay).unwrap_or_default();
        let delay = Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX);
        self.after_match = Some((delay, Regex::new(pattern)?));
        Ok(())
    }

    pub(crate) fn matcher(&self) -> Option<MatchTimeout> {
        let (delay, pattern) = self.after_match.clone()?;
        Some(MatchTimeout {
            delay,
            pattern,
            stdout: OutputLine::default(),
            stderr: OutputLine::default(),
            budget: Limits::default().regex_steps,
        })
    }

    pub(crate) fn graceful(&self) -> Option<GracefulTimeout> {
        Some(GracefulTimeout {
            signal: self.signal?,
            grace: self.grace.unwrap_or(Duration::from_secs(1)),
        })
    }
}

fn signal(value: &BString) -> Result<TimeoutSignal, String> {
    if cfg!(windows) {
        return Err("TIMEOUT_SIGNAL_NAME is not supported on Windows.".into());
    }
    match value.as_slice() {
        b"SIGINT" => Ok(TimeoutSignal::Interrupt),
        b"SIGQUIT" => Ok(TimeoutSignal::Quit),
        b"SIGTERM" => Ok(TimeoutSignal::Terminate),
        b"SIGUSR1" => Ok(TimeoutSignal::User1),
        b"SIGUSR2" => Ok(TimeoutSignal::User2),
        _ => Err(format!(
            "TIMEOUT_SIGNAL_NAME \"{value}\" not supported on this platform."
        )),
    }
}

fn grace(value: &BString) -> Result<Duration, String> {
    if cfg!(windows) {
        return Err("TIMEOUT_SIGNAL_GRACE_PERIOD is not supported on Windows.".into());
    }
    let seconds = crate::cli::number_prefix(value).unwrap_or_default();
    if !seconds.is_finite() || seconds <= 0.0 {
        Err(format!(
            "TIMEOUT_SIGNAL_GRACE_PERIOD \"{value}\" is not greater than \"0\" seconds."
        ))
    } else if seconds > 60.0 {
        Err(format!(
            "TIMEOUT_SIGNAL_GRACE_PERIOD \"{value}\" is not less than the maximum of \"60\" seconds."
        ))
    } else {
        Ok(Duration::from_secs_f64(seconds))
    }
}

pub(crate) fn signal_name(signal: TimeoutSignal) -> &'static str {
    match signal {
        TimeoutSignal::Interrupt => "SIGINT",
        TimeoutSignal::Quit => "SIGQUIT",
        TimeoutSignal::Terminate => "SIGTERM",
        TimeoutSignal::User1 => "SIGUSR1",
        TimeoutSignal::User2 => "SIGUSR2",
    }
}

#[derive(Default)]
struct OutputLine {
    bytes: Vec<u8>,
    overflow: bool,
}

pub(crate) struct MatchTimeout {
    delay: Duration,
    pattern: Regex,
    stdout: OutputLine,
    stderr: OutputLine,
    budget: u64,
}

impl MatchTimeout {
    pub(crate) fn update(&mut self, stream: Stream, bytes: &[u8]) -> Option<Duration> {
        const MAX_LINE: usize = 64 * 1024;
        let line = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            let end = chunk.strip_suffix(b"\n");
            let text = end.unwrap_or(chunk);
            if !line.overflow && text.len() <= MAX_LINE - line.bytes.len() {
                line.bytes.extend_from_slice(text);
            } else {
                line.overflow = true;
            }
            if end.is_some() {
                let matched = !line.overflow
                    && self
                        .pattern
                        .find_at(
                            line.bytes.strip_suffix(b"\r").unwrap_or(&line.bytes),
                            0,
                            Default::default(),
                            &mut self.budget,
                        )
                        .ok()
                        .flatten()
                        .is_some();
                line.bytes.clear();
                line.overflow = false;
                if matched {
                    return Some(self.delay);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_matches_complete_lines_across_chunks() -> Result<(), Box<dyn std::error::Error>> {
        let mut settings = Settings::default();
        settings.set_after_match(b"0.5", b"^ready$")?;
        let mut timeout = settings.matcher().ok_or("Missing match timeout.")?;
        assert_eq!(timeout.update(Stream::Stdout, b"re"), None);
        assert_eq!(timeout.update(Stream::Stderr, b"ady\n"), None);
        assert_eq!(timeout.update(Stream::Stdout, b"ady"), None);
        assert_eq!(
            timeout.update(Stream::Stdout, b"\r\n"),
            Some(Duration::from_millis(500))
        );
        Ok(())
    }
}
