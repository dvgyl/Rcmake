//! Bounded byte-stream key decoder supporting VT100/xterm escape sequences.

/// Decoded logical keyboard event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Printable character.
    Char(char),
    /// Control key `Ctrl-<letter>` (lowercase ASCII letter `b'a'..=b'z'`).
    Ctrl(u8),
    /// Return / Enter (`\r` or `\n`).
    Enter,
    /// Backspace (`0x7f` or `0x08`).
    Backspace,
    /// Forward delete (`ESC [ 3 ~`).
    Delete,
    /// Horizontal tab (`0x09`).
    Tab,
    /// Lone Escape key (after timeout tick or double `ESC`).
    Esc,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home key.
    Home,
    /// End key.
    End,
    /// Page Up key.
    PageUp,
    /// Page Down key.
    PageDown,
    /// Unrecognized or malformed input sequence.
    Unknown,
}

/// Maximum number of bytes buffered inside a single CSI escape sequence.
const MAX_ESC_BUF: usize = 8;
/// Maximum number of discarded overflow bytes before leaving an unrecognized CSI sequence.
const MAX_CSI_DISCARD: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    CsiDiscard { skipped: usize },
    Ss3,
    Utf8 { expected: u8 },
}

/// Bounded streaming decoder from raw terminal bytes to [`Key`].
#[derive(Clone, Debug)]
pub struct KeyDecoder {
    state: State,
    buf: [u8; MAX_ESC_BUF],
    len: usize,
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyDecoder {
    /// Construct a fresh decoder in the ground state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            buf: [0; MAX_ESC_BUF],
            len: 0,
        }
    }

    /// Feed one raw input byte into the decoder.
    pub fn push(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            State::Ground => self.push_ground(byte),
            State::Esc => self.push_esc(byte),
            State::Csi => self.push_csi(byte),
            State::CsiDiscard { skipped } => self.push_csi_discard(byte, skipped),
            State::Ss3 => self.push_ss3(byte),
            State::Utf8 { expected } => self.push_utf8(byte, expected),
        }
    }

    /// Notify the decoder that a poll timeout elapsed without further bytes.
    ///
    /// A lone pending `ESC` resolves to `Some(Key::Esc)`; any incomplete multi-byte
    /// escape or UTF-8 sequence resolves to `Some(Key::Unknown)`.
    pub fn flush_timeout(&mut self) -> Option<Key> {
        let prev = self.state;
        self.reset();
        match prev {
            State::Ground => None,
            State::Esc => Some(Key::Esc),
            State::Csi | State::CsiDiscard { .. } | State::Ss3 | State::Utf8 { .. } => {
                Some(Key::Unknown)
            }
        }
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.len = 0;
    }

    fn push_ground(&mut self, byte: u8) -> Option<Key> {
        match byte {
            0x1b => {
                self.state = State::Esc;
                self.len = 0;
                None
            }
            b'\r' | b'\n' => Some(Key::Enter),
            0x08 | 0x7f => Some(Key::Backspace),
            0x09 => Some(Key::Tab),
            0x01..=0x1a => Some(Key::Ctrl(byte + b'a' - 1)),
            0x20..=0x7e => Some(Key::Char(char::from(byte))),
            0x00 | 0x1c..=0x1f => Some(Key::Unknown),
            _ => {
                let expected = utf8_sequence_len(byte);
                if expected <= 1 {
                    Some(Key::Unknown)
                } else {
                    self.buf[0] = byte;
                    self.len = 1;
                    self.state = State::Utf8 { expected };
                    None
                }
            }
        }
    }

    fn push_esc(&mut self, byte: u8) -> Option<Key> {
        match byte {
            b'[' => {
                self.state = State::Csi;
                self.len = 0;
                None
            }
            b'O' => {
                self.state = State::Ss3;
                self.len = 0;
                None
            }
            0x1b => Some(Key::Esc),
            _ => {
                self.reset();
                Some(Key::Unknown)
            }
        }
    }

    fn push_ss3(&mut self, byte: u8) -> Option<Key> {
        self.reset();
        let key = match byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            _ => Key::Unknown,
        };
        Some(key)
    }

    fn push_csi(&mut self, byte: u8) -> Option<Key> {
        if byte == 0x1b {
            self.state = State::Esc;
            self.len = 0;
            return Some(Key::Unknown);
        }
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        } else {
            self.reset();
            return Some(Key::Unknown);
        }

        if (0x40..=0x7e).contains(&byte) {
            let key = decode_csi(self.buf.get(..self.len).unwrap_or_default());
            self.reset();
            return Some(key);
        }

        if !(0x20..=0x3f).contains(&byte) {
            self.reset();
            return Some(Key::Unknown);
        }

        if self.len >= MAX_ESC_BUF {
            self.state = State::CsiDiscard { skipped: 0 };
            self.len = 0;
        }

        None
    }

    fn push_csi_discard(&mut self, byte: u8, skipped: usize) -> Option<Key> {
        if byte == 0x1b {
            self.state = State::Esc;
            self.len = 0;
            return Some(Key::Unknown);
        }
        if (0x40..=0x7e).contains(&byte)
            || !(0x20..=0x3f).contains(&byte)
            || skipped >= MAX_CSI_DISCARD
        {
            self.reset();
            return Some(Key::Unknown);
        }
        self.state = State::CsiDiscard {
            skipped: skipped.saturating_add(1),
        };
        None
    }

    fn push_utf8(&mut self, byte: u8, expected: u8) -> Option<Key> {
        if (byte & 0b1100_0000) != 0b1000_0000 {
            self.reset();
            return Some(Key::Unknown);
        }
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        } else {
            self.reset();
            return Some(Key::Unknown);
        }
        if self.len < usize::from(expected) {
            return None;
        }
        let key = self
            .buf
            .get(..self.len)
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .and_then(|s| s.chars().next())
            .filter(|ch| !ch.is_control())
            .map_or(Key::Unknown, Key::Char);
        self.reset();
        Some(key)
    }
}

fn utf8_sequence_len(first: u8) -> u8 {
    if (first & 0b1110_0000) == 0b1100_0000 {
        2
    } else if (first & 0b1111_0000) == 0b1110_0000 {
        3
    } else if (first & 0b1111_1000) == 0b1111_0000 {
        4
    } else {
        0
    }
}

fn decode_csi(seq: &[u8]) -> Key {
    match seq {
        b"A" => Key::Up,
        b"B" => Key::Down,
        b"C" => Key::Right,
        b"D" => Key::Left,
        b"H" | b"1~" | b"7~" => Key::Home,
        b"F" | b"4~" | b"8~" => Key::End,
        b"3~" => Key::Delete,
        b"5~" => Key::PageUp,
        b"6~" => Key::PageDown,
        _ => Key::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> Vec<Key> {
        let mut decoder = KeyDecoder::new();
        let mut keys = Vec::new();
        for &b in bytes {
            if let Some(key) = decoder.push(b) {
                keys.push(key);
            }
        }
        if let Some(key) = decoder.flush_timeout() {
            keys.push(key);
        }
        keys
    }

    #[test]
    fn decodes_all_standard_sequences() {
        let cases: &[(&[u8], Key)] = &[
            (b"\x1b[A", Key::Up),
            (b"\x1b[B", Key::Down),
            (b"\x1b[C", Key::Right),
            (b"\x1b[D", Key::Left),
            (b"\x1b[H", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1bOA", Key::Up),
            (b"\x1bOB", Key::Down),
            (b"\x1bOC", Key::Right),
            (b"\x1bOD", Key::Left),
            (b"\x1bOH", Key::Home),
            (b"\x1bOF", Key::End),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[3~", Key::Delete),
            (b"\x1b[4~", Key::End),
            (b"\x1b[5~", Key::PageUp),
            (b"\x1b[6~", Key::PageDown),
            (b"\x7f", Key::Backspace),
            (b"\x08", Key::Backspace),
            (b"\r", Key::Enter),
            (b"\n", Key::Enter),
            (b"\t", Key::Tab),
            (b"\x03", Key::Ctrl(b'c')),
            (b"\x04", Key::Ctrl(b'd')),
            (b"\x0e", Key::Ctrl(b'n')),
            (b"\x10", Key::Ctrl(b'p')),
            (b"\x15", Key::Ctrl(b'u')),
            (b"\x1b", Key::Esc),
            (b"\x1b[99;99~", Key::Unknown),
            (b"\x1b[1234567890A", Key::Unknown),
        ];
        for (input, expected) in cases {
            assert_eq!(decode_all(input), vec![*expected], "input: {input:?}");
        }
    }

    #[test]
    fn handles_split_escape_across_reads() {
        let mut decoder = KeyDecoder::new();
        assert_eq!(decoder.push(0x1b), None);
        assert_eq!(decoder.push(b'['), None);
        assert_eq!(decoder.push(b'5'), None);
        assert_eq!(decoder.push(b'~'), Some(Key::PageUp));
        assert_eq!(decoder.flush_timeout(), None);

        // Lone ESC flushed on timeout tick
        assert_eq!(decoder.push(0x1b), None);
        assert_eq!(decoder.flush_timeout(), Some(Key::Esc));
    }

    #[test]
    fn one_mib_random_bytes_never_panics() {
        let mut decoder = KeyDecoder::new();
        let mut state: u64 = 0x517c_c1b7_2722_0a95;
        let mut count = 0usize;
        for i in 0..(1024 * 1024) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let byte = (state & 0xff) as u8;
            if decoder.push(byte).is_some() {
                count += 1;
            }
            if i % 97 == 0 && decoder.flush_timeout().is_some() {
                count += 1;
            }
        }
        assert!(count > 0);
    }
}
