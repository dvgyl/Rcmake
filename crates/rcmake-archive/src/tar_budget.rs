use std::io::{self, Read};
use std::ops::Range;

use tar::{Header, PaxExtensions};

use crate::MAX_ENTRIES;

const BLOCK_SIZE: usize = 512;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

// Check physical headers before tar can read extension data into memory.
pub(super) struct Reader<R> {
    inner: R,
    header: Header,
    pending: Range<usize>,
    body: u64,
    sparse: bool,
    trailer: bool,
    pax_size: Option<u64>,
    pax: Option<(usize, Vec<u8>)>,
    records_left: usize,
    bytes_left: u64,
}

impl<R: Read> Reader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            header: Header::new_old(),
            pending: 0..0,
            body: 0,
            sparse: false,
            trailer: false,
            pax_size: None,
            pax: None,
            records_left: MAX_ENTRIES,
            bytes_left: MAX_METADATA_BYTES,
        }
    }

    pub(super) fn into_inner(self) -> R {
        self.inner
    }

    fn charge(&mut self, size: u64) -> io::Result<()> {
        let error = || io::Error::other("Tar metadata exceeds the record or size limit");
        if size > MAX_RECORD_BYTES {
            return Err(error());
        }
        self.records_left = self.records_left.checked_sub(1).ok_or_else(error)?;
        let record_size = size.next_multiple_of(BLOCK_SIZE as u64) + BLOCK_SIZE as u64;
        self.bytes_left = self.bytes_left.checked_sub(record_size).ok_or_else(error)?;
        Ok(())
    }

    fn next_header(&mut self) -> io::Result<()> {
        let (first, rest) = self
            .header
            .as_mut_bytes()
            .split_at_mut_checked(1)
            .ok_or_else(|| io::Error::other("Invalid tar header buffer"))?;
        if self.inner.read(first)? == 0 {
            self.trailer = true;
            return Ok(());
        }
        self.inner.read_exact(rest)?;
        self.pending = 0..BLOCK_SIZE;

        if self.sparse {
            self.sparse = self.header.as_bytes().get(504) == Some(&1);
            return self.charge(0);
        }

        if self.header.as_bytes().iter().all(|&byte| byte == 0) {
            self.trailer = true;
            return Ok(());
        }
        let kind = self.header.entry_type();
        let gnu = kind.is_gnu_longname() || kind.is_gnu_longlink();
        let local_pax = kind.is_pax_local_extensions();
        let global_pax = kind.is_pax_global_extensions();
        let recognized = self.header.as_gnu().is_some() || self.header.as_ustar().is_some();
        let mut size = self.header.entry_size()?;
        if gnu || local_pax || global_pax {
            self.charge(size)?;
            if local_pax && recognized && size != 0 {
                self.pax = Some((size as usize, Vec::new()));
            } else if local_pax || global_pax || !recognized {
                self.pax_size = None;
            }
        } else {
            size = self.pax_size.take().unwrap_or(size);
        }
        self.body = size
            .checked_next_multiple_of(BLOCK_SIZE as u64)
            .ok_or_else(|| io::Error::other("Invalid tar entry size"))?;
        self.sparse =
            kind.is_gnu_sparse() && self.header.as_gnu().is_some_and(|gnu| gnu.is_extended());
        Ok(())
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() && !self.trailer {
            if self.sparse || self.body == 0 {
                self.next_header()?;
            } else {
                let read = (&mut self.inner).take(self.body).read(out)?;
                self.body -= read as u64;
                if let Some((len, data)) = &mut self.pax {
                    let count = read.min(*len - data.len());
                    data.extend(out.iter().take(count).copied());
                    if data.len() == *len {
                        self.pax_size = pax_size(data);
                        self.pax = None;
                    }
                }
                return Ok(read);
            }
        }
        if self.pending.is_empty() {
            return self.inner.read(out);
        }
        let mut pending = self
            .header
            .as_bytes()
            .get(self.pending.clone())
            .ok_or_else(|| io::Error::other("Invalid tar header buffer"))?;
        let read = pending.read(out)?;
        self.pending.start += read;
        Ok(read)
    }
}

fn pax_size(bytes: &[u8]) -> Option<u64> {
    for field in PaxExtensions::new(bytes) {
        let field = field.ok()?;
        if field.key_bytes() == b"size" {
            return field.value().ok()?.parse().ok();
        }
    }
    None
}
