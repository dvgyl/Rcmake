//! GNU make jobserver token accounting.

use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::Host;

fn auth(flags: &[u8]) -> Option<BString> {
    let mut input = flags.iter().copied().peekable();
    let mut result = None;
    while input.next_if(|byte| byte.is_ascii_whitespace()).is_some() || input.peek().is_some() {
        let mut word = Vec::new();
        let mut quote = None;
        while let Some(byte) = input.next_if(|byte| quote.is_some() || !byte.is_ascii_whitespace())
        {
            match byte {
                b'\\' => word.extend(input.next()),
                b'\'' | b'"' if quote == Some(byte) => quote = None,
                b'\'' | b'"' if quote.is_none() => quote = Some(byte),
                _ => word.push(byte),
            }
        }
        for prefix in [b"--jobserver-auth=".as_slice(), b"--jobserver-fds=", b"-J"] {
            if let Some(value) = word.strip_prefix(prefix) {
                result = Some(BString::from(value));
                break;
            }
        }
    }
    result
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

pub(crate) enum Event<T> {
    Done(T),
    Token(Token),
    Lost,
}

pub(crate) struct Token {
    byte: u8,
    writer: Writer,
}

impl Drop for Token {
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(&[self.byte]);
        }
    }
}

pub(crate) struct Tokens {
    requests: Option<mpsc::Sender<()>>,
    pending: bool,
    implicit_in_use: bool,
    assigned: Vec<Token>,
    spare: Option<Token>,
}

impl Tokens {
    pub(crate) fn connect<T: Send + 'static>(
        host: &dyn Host,
        events: mpsc::Sender<Event<T>>,
    ) -> Option<Self> {
        let auth = auth(&host.env_get(b"MAKEFLAGS")?)?;
        let pipe = host.open_jobserver(&auth)?;
        let (requests, read) = mpsc::channel();
        let mut reader = pipe.reader;
        let writer = Arc::new(Mutex::new(pipe.writer));
        thread::Builder::new()
            .name("make-jobserver".into())
            .spawn(move || {
                while read.recv().is_ok() {
                    let mut byte = [0];
                    loop {
                        match reader.read(&mut byte) {
                            Ok(1) => break,
                            Err(error) if error.kind() == ErrorKind::Interrupted => {}
                            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                                if matches!(
                                    read.recv_timeout(Duration::from_millis(2)),
                                    Err(mpsc::RecvTimeoutError::Disconnected)
                                ) {
                                    return;
                                }
                            }
                            _ => {
                                let _ = events.send(Event::Lost);
                                return;
                            }
                        }
                    }
                    let [byte] = byte;
                    if events
                        .send(Event::Token(Token {
                            byte,
                            writer: Arc::clone(&writer),
                        }))
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests: Some(requests),
            pending: false,
            implicit_in_use: false,
            assigned: Vec::new(),
            spare: None,
        })
    }

    pub(crate) fn try_acquire(&mut self) -> bool {
        if self.implicit_in_use {
            let Some(token) = self.spare.take() else {
                if !self.pending {
                    self.pending = self.requests.as_ref().is_some_and(|tx| tx.send(()).is_ok());
                }
                return false;
            };
            self.assigned.push(token);
        }
        self.implicit_in_use = true;
        true
    }

    pub(crate) fn received(&mut self, token: Token) {
        self.pending = false;
        self.spare = Some(token);
    }

    pub(crate) fn lost(&mut self) {
        self.requests = None;
        self.pending = false;
    }

    pub(crate) fn return_spare(&mut self) {
        self.spare = None;
    }

    pub(crate) fn release(&mut self) {
        if self.assigned.pop().is_none() {
            self.implicit_in_use = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PipeWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for PipeWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn returns_pipe_tokens_and_keeps_the_implicit_slot_local() {
        let returned = Arc::new(Mutex::new(Vec::new()));
        let writer: Writer = Arc::new(Mutex::new(Box::new(PipeWriter(Arc::clone(&returned)))));
        let (requests, reader) = mpsc::channel();
        let mut tokens = Tokens {
            requests: Some(requests),
            pending: false,
            implicit_in_use: false,
            assigned: Vec::new(),
            spare: None,
        };
        assert!(tokens.try_acquire());
        assert!(reader.try_recv().is_err());
        assert!(!tokens.try_acquire());
        assert!(!tokens.try_acquire());
        assert_eq!(reader.try_recv(), Ok(()));
        assert!(reader.try_recv().is_err());
        tokens.received(Token {
            byte: b'+',
            writer: Arc::clone(&writer),
        });
        assert!(tokens.try_acquire());
        tokens.release();
        tokens.release();
        assert_eq!(*returned.lock().unwrap(), b"+");
        tokens.received(Token { byte: b'=', writer });
        drop(tokens);
        assert_eq!(*returned.lock().unwrap(), b"+=");
    }

    #[test]
    fn last_jobserver_flag_wins() {
        for (flags, expected) in [
            (" -j3 --jobserver-auth=3,4", Some("3,4")),
            (
                "--jobserver-fds=5,6 --jobserver-auth=fifo:/tmp/a\\ b",
                Some("fifo:/tmp/a b"),
            ),
            ("-J7,8", Some("7,8")),
            ("-j2", None),
            ("", None),
        ] {
            assert_eq!(auth(flags.as_bytes()), expected.map(BString::from));
        }
    }
}
