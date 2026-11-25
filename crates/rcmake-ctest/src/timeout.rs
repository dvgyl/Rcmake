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
            output: Vec::new(),
            line_start: 0,
            finished: false,
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

pub(crate) struct MatchTimeout {
    delay: Duration,
    pattern: Regex,
    output: Vec<u8>,
    line_start: usize,
    finished: bool,
}

impl MatchTimeout {
    pub(crate) fn update(&mut self, _stream: Stream, bytes: &[u8]) -> Option<Duration> {
        const MAX_OUTPUT: usize = 16 * 1024 * 1024;
        if self.finished {
            return None;
        }
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            if chunk.len() > MAX_OUTPUT - self.output.len() {
                self.finished = true;
                return None;
            }
            self.output.extend_from_slice(chunk);
            if !chunk.ends_with(b"\n") {
                continue;
            }
            self.output.pop();
            if self.output.last() == Some(&b'\r') {
                self.output.pop();
            }
            let line = self.output.get(self.line_start..).unwrap_or_default();
            if ctest_tag(line, b"CTestDetails") || ctest_tag(line, b"CTestLabel") {
                self.output.truncate(self.line_start);
                continue;
            }
            self.output.push(b'\n');
            self.line_start = self.output.len();
            if self
                .pattern
                .find_at(
                    &self.output,
                    0,
                    Default::default(),
                    &mut Limits::default().regex_steps,
                )
                .ok()
                .flatten()
                .is_some()
            {
                self.finished = true;
                return Some(self.delay);
            }
        }
        None
    }
}

fn ctest_tag(line: &[u8], name: &[u8]) -> bool {
    use rcmake_base::ByteSlice as _;
    let open = [b"<".as_slice(), name, b">"].concat();
    let close = [b"</".as_slice(), name, b">"].concat();
    line.split_once_str(&open)
        .is_some_and(|(_, tail)| tail.find(&close).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_matches_complete_lines_across_chunks() -> Result<(), Box<dyn std::error::Error>> {
        let mut settings = Settings::default();
        settings.set_after_match(b"0.5", b"^ready\nnext")?;
        let mut timeout = settings.matcher().ok_or("Missing match timeout.")?;
        assert_eq!(timeout.update(Stream::Stdout, b"re"), None);
        assert_eq!(timeout.update(Stream::Stderr, b"ady\n"), None);
        assert_eq!(
            timeout.update(Stream::Stdout, b"next\r\n"),
            Some(Duration::from_millis(500))
        );
        settings.set_after_match(b"0.5", b"next$")?;
        let mut timeout = settings.matcher().ok_or("Missing match timeout.")?;
        assert_eq!(timeout.update(Stream::Stdout, b"next\n"), None);
        Ok(())
    }
}
