use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{GracefulTimeout, TimeoutSignal};

#[derive(Default)]
pub(crate) struct Settings {
    pub(crate) errors: Vec<String>,
    signal: Option<TimeoutSignal>,
    grace: Option<Duration>,
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
