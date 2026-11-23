use super::lexical::Lexical;

pub(super) fn hash(normalized: &[u8]) -> String {
    let path = Lexical::new(normalized);
    let root = [path.root_name(), path.root_directory()];
    let trailing =
        (!path.relative_part().is_empty() && normalized.ends_with(b"/")).then_some(b"".as_slice());
    let parts = root
        .into_iter()
        .filter(|part| !part.is_empty())
        .chain(path.components())
        .chain(trailing);
    let hash = hash_components(parts);
    format!("{hash:x}")
}

#[cfg(target_os = "macos")]
fn hash_components<'a>(parts: impl Iterator<Item = &'a [u8]>) -> u64 {
    macos::path_hash(parts)
}

#[cfg(not(target_os = "macos"))]
fn hash_components<'a>(parts: impl Iterator<Item = &'a [u8]>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        return 0;
    }
    for part in parts {
        part.hash(&mut hash);
    }
    hash.finish()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::num::Wrapping as W;

    type Word = W<u64>;
    const K0: Word = W(0xc3a5c85c97cb3127);
    const K1: Word = W(0xb492b66fbe98f273);
    const K2: Word = W(0x9ae16a3b2f90404f);
    const K3: Word = W(0xc949d7c7509e6557);

    pub(super) fn path_hash<'a>(parts: impl Iterator<Item = &'a [u8]>) -> u64 {
        parts
            .fold(W(0), |state, part| {
                let component = city_hash(part);
                hash_pair(state, rotate(component + W(16), 16)) ^ component
            })
            .0
    }

    fn load(bytes: &[u8], offset: usize) -> Word {
        let data = bytes
            .get(offset..offset + 8)
            .and_then(|p| <[u8; 8]>::try_from(p).ok());
        W(data.map_or(0, u64::from_le_bytes))
    }

    fn short_word(bytes: &[u8], offset: usize) -> u32 {
        bytes
            .get(offset..offset + 4)
            .and_then(|p| <[u8; 4]>::try_from(p).ok())
            .map_or(0, u32::from_le_bytes)
    }

    fn byte(bytes: &[u8], offset: usize) -> u64 {
        bytes.get(offset).copied().map_or(0, u64::from)
    }

    fn rotate(word: Word, distance: u32) -> Word {
        W(word.0.rotate_right(distance))
    }

    fn mix(word: Word) -> Word {
        word ^ (word >> 47)
    }

    fn hash_pair(first: Word, second: Word) -> Word {
        const MULTIPLIER: Word = W(0x9ddfea08eb382d69);
        let first = mix((first ^ second) * MULTIPLIER);
        mix((second ^ first) * MULTIPLIER) * MULTIPLIER
    }

    fn city_hash(bytes: &[u8]) -> Word {
        let len = bytes.len();
        let length = W(len as u64);
        match len {
            0 => K2,
            1..=3 => {
                let start = W(byte(bytes, 0) + (byte(bytes, len / 2) << 8));
                let end = length + W(byte(bytes, len - 1) << 2);
                mix((start * K2) ^ (end * K3)) * K2
            }
            4..=8 => {
                let first = W(u64::from(short_word(bytes, 0) << 3));
                let last = W(u64::from(short_word(bytes, len - 4)));
                hash_pair(length + first, last)
            }
            9..=16 => {
                let last = load(bytes, len - 8);
                hash_pair(load(bytes, 0), rotate(last + length, len as u32)) ^ last
            }
            17..=32 => {
                let first = load(bytes, 0) * K1;
                let second = load(bytes, 8);
                let penultimate = load(bytes, len - 16) * K0;
                let last = load(bytes, len - 8) * K2;
                hash_pair(
                    rotate(first - second, 43) + rotate(last, 30) + penultimate,
                    first + rotate(second ^ K3, 20) - last + length,
                )
            }
            33..=64 => {
                let first = load(bytes, 0) + (length + load(bytes, len - 16)) * K0;
                let boundary = load(bytes, 24);
                let front = medium_block(first, load(bytes, 8), load(bytes, 16), boundary);
                let back = medium_block(
                    load(bytes, 16) + load(bytes, len - 32),
                    load(bytes, len - 24),
                    load(bytes, len - 16),
                    boundary + load(bytes, len - 8),
                );
                let combined = mix((front.0 + back.1) * K2 + (back.0 + front.1) * K0);
                mix(combined * K0 + front.1) * K2
            }
            _ => long_hash(bytes),
        }
    }

    fn medium_block(a: Word, b: Word, c: Word, z: Word) -> (Word, Word) {
        let sum = a + b + c;
        (
            sum + z,
            rotate(a + z, 52) + rotate(sum, 31) + rotate(a, 37) + rotate(a + b, 7),
        )
    }

    fn seeded_block(data: &[u8], offset: usize, a: Word, b: Word) -> (Word, Word) {
        let first = a + load(data, offset);
        let middle = first + load(data, offset + 8) + load(data, offset + 16);
        let last = load(data, offset + 24);
        (
            middle + last,
            rotate(b + first + last, 21) + rotate(middle, 44) + first,
        )
    }

    fn long_hash(data: &[u8]) -> Word {
        let len = data.len();
        let length = W(len as u64);
        let mut x = load(data, len - 40);
        let mut y = load(data, len - 16) + load(data, len - 56);
        let mut z = hash_pair(load(data, len - 48) + length, load(data, len - 24));
        let mut v = seeded_block(data, len - 64, length, z);
        let mut w = seeded_block(data, len - 32, y + K1, x);
        x = x * K1 + load(data, 0);
        let end = (len - 1) & !63;
        for offset in (0..end).step_by(64) {
            x = (rotate(x + y + v.0 + load(data, offset + 8), 37) * K1) ^ w.1;
            y = rotate(y + v.1 + load(data, offset + 48), 42) * K1 + v.0 + load(data, offset + 40);
            z = rotate(z + w.0, 33) * K1;
            v = seeded_block(data, offset, v.1 * K1, x + w.0);
            w = seeded_block(data, offset + 32, z + w.1, y + load(data, offset + 16));
            std::mem::swap(&mut x, &mut z);
        }
        hash_pair(
            hash_pair(v.0, w.0) + mix(y) * K1 + z,
            hash_pair(v.1, w.1) + x,
        )
    }
}
