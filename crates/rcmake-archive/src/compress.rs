//! The Unix compress stream format uses variable-width LZW codes.

use std::collections::HashMap;
use std::io::{self, Read, Write};

const CLEAR: u16 = 256;
const START_WIDTH: usize = 9;
const MAX_WIDTH: usize = 16;
const CAPACITY: usize = 1 << MAX_WIDTH;

pub(super) struct Encoder {
    bytes: Vec<u8>,
    dictionary: HashMap<(u16, u8), u16>,
    prefix: Option<u16>,
    width: usize,
    group: u32,
    bits: usize,
}

impl Encoder {
    pub(super) fn new() -> Self {
        Self {
            bytes: vec![0x1f, 0x9d, 0x90],
            dictionary: HashMap::new(),
            prefix: None,
            width: START_WIDTH,
            group: 0,
            bits: 0,
        }
    }

    fn emit(&mut self, code: u16) {
        // Flush whole bytes as soon as possible. Only partial bytes stay in group.
        self.group |= u32::from(code) << (self.bits % 8);
        let bytes = (self.bits % 8 + self.width) / 8;
        for _ in 0..bytes {
            self.bytes.push(self.group as u8);
            self.group >>= 8;
        }
        self.bits = (self.bits + self.width) % (8 * self.width);
    }

    fn align(&mut self) {
        // Width changes and clear codes pad the current group to eight codes.
        if self.bits != 0 {
            let remaining = self.width - self.bits / 8;
            self.bytes.push(self.group as u8);
            self.bytes.resize(self.bytes.len() + remaining - 1, 0);
            self.group = 0;
            self.bits = 0;
        }
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        if let Some(prefix) = self.prefix {
            self.emit(prefix);
        }
        if !self.bits.is_multiple_of(8) {
            self.bytes.push(self.group as u8);
        }
        self.bytes
    }
}

impl Write for Encoder {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes {
            let Some(prefix) = self.prefix else {
                self.prefix = Some(u16::from(byte));
                continue;
            };
            if let Some(&code) = self.dictionary.get(&(prefix, byte)) {
                self.prefix = Some(code);
                continue;
            }

            self.emit(prefix);
            let next = self.dictionary.len() + usize::from(CLEAR) + 1;
            if next == CAPACITY {
                self.emit(CLEAR);
                self.align();
                self.dictionary.clear();
                self.width = START_WIDTH;
            } else {
                if next == 1 << self.width {
                    self.align();
                    self.width += 1;
                }
                self.dictionary.insert((prefix, byte), next as u16);
            }
            self.prefix = Some(u16::from(byte));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Decoder<R> {
    groups: R,
    group: [u8; MAX_WIDTH],
    len: usize,
    bits: usize,
    width: usize,
    max_width: usize,
    block_mode: bool,
    dictionary: Vec<(u16, u8)>,
    previous: Option<u16>,
    phrase: Vec<u8>,
}

impl<R: Read> Decoder<R> {
    pub(super) fn new(mut groups: R) -> io::Result<Self> {
        let mut header = [0; 3];
        groups.read_exact(&mut header)?;
        let [0x1f, 0x9d, flags] = header else {
            return Err(invalid_stream());
        };
        let max_width = usize::from(flags & 0x1f);
        if !(START_WIDTH..=MAX_WIDTH).contains(&max_width) || flags & 0x60 != 0 {
            return Err(invalid_stream());
        }
        Ok(Self {
            groups,
            group: [0; MAX_WIDTH],
            len: 0,
            bits: 0,
            width: START_WIDTH,
            max_width,
            block_mode: flags & 0x80 != 0,
            dictionary: Vec::new(),
            previous: None,
            phrase: Vec::new(),
        })
    }

    fn next_code(&mut self) -> io::Result<Option<u16>> {
        if self.bits + self.width > self.len * 8 {
            self.len = 0;
            while self.len < self.width {
                match self.groups.read(
                    self.group
                        .get_mut(self.len..self.width)
                        .ok_or_else(invalid_stream)?,
                ) {
                    Ok(0) => break,
                    Ok(n) => self.len += n,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            if self.len == 0 {
                return Ok(None);
            }
            if self.len * 8 < self.width {
                return Err(invalid_stream());
            }
            self.bits = 0;
        }
        let mut packed = [0; 4];
        for (dest, &src) in packed.iter_mut().zip(
            self.group
                .get(..self.len)
                .ok_or_else(invalid_stream)?
                .iter()
                .skip(self.bits / 8),
        ) {
            *dest = src;
        }
        let code = (u32::from_le_bytes(packed) >> (self.bits % 8)) & ((1 << self.width) - 1);
        self.bits += self.width;
        Ok(Some(code as u16))
    }

    fn decode_phrase(&mut self) -> io::Result<bool> {
        let first_free = usize::from(CLEAR) + usize::from(self.block_mode);
        loop {
            let next = first_free + self.dictionary.len();
            if self.width < self.max_width && next == 1 << self.width {
                self.len = 0;
                self.width += 1;
            }
            let Some(code) = self.next_code()? else {
                return Ok(false);
            };
            if self.block_mode && code == CLEAR {
                if self.previous.is_none() || self.group.len() != self.width {
                    return Err(invalid_stream());
                }
                self.len = 0;
                self.width = START_WIDTH;
                self.dictionary.clear();
                self.previous = None;
                continue;
            }

            let mut entry = code;
            let repeated = usize::from(code) == next;
            if repeated {
                entry = self.previous.ok_or_else(invalid_stream)?;
            }
            while entry >= CLEAR {
                let index = usize::from(entry)
                    .checked_sub(first_free)
                    .ok_or_else(invalid_stream)?;
                let &(prefix, byte) = self.dictionary.get(index).ok_or_else(invalid_stream)?;
                self.phrase.push(byte);
                entry = prefix;
            }
            let first = entry as u8;
            self.phrase.push(first);
            if repeated {
                self.phrase.insert(0, first);
            }
            if let Some(previous) = self.previous
                && next < 1 << self.max_width
            {
                self.dictionary.push((previous, first));
            }
            self.previous = Some(code);
            return Ok(true);
        }
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Return bytes already decoded before examining the next phrase.
        if buf.is_empty() || (self.phrase.is_empty() && !self.decode_phrase()?) {
            return Ok(0);
        }
        let count = buf.len().min(self.phrase.len());
        for byte in buf.iter_mut().take(count) {
            *byte = self.phrase.pop().ok_or_else(invalid_stream)?;
        }
        Ok(count)
    }
}

fn invalid_stream() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid Unix compress stream")
}
