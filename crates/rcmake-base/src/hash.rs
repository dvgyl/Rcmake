//! Cryptographic hash algorithms supported by CMake 4.5.1 (`string(<HASH>)`, `file(<HASH>)`,
//! and `cmake -E md5sum/sha*sum`).

use md5::Digest as _;
use rcmake_base_internal::hex_encode;

/// All cryptographic hash algorithms supported by CMake 4.5.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HashAlgo {
    /// MD5 (`MD5`).
    Md5,
    /// SHA-1 (`SHA1`).
    Sha1,
    /// SHA-224 (`SHA224`).
    Sha224,
    /// SHA-256 (`SHA256`).
    Sha256,
    /// SHA-384 (`SHA384`).
    Sha384,
    /// SHA-512 (`SHA512`).
    Sha512,
    /// SHA3-224 (`SHA3_224`).
    Sha3_224,
    /// SHA3-256 (`SHA3_256`).
    Sha3_256,
    /// SHA3-384 (`SHA3_384`).
    Sha3_384,
    /// SHA3-512 (`SHA3_512`).
    Sha3_512,
}

impl HashAlgo {
    /// Parse a CMake hash command token (exact uppercase as used by `string()`/`file()`).
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        match name {
            b"MD5" => Some(Self::Md5),
            b"SHA1" => Some(Self::Sha1),
            b"SHA224" => Some(Self::Sha224),
            b"SHA256" => Some(Self::Sha256),
            b"SHA384" => Some(Self::Sha384),
            b"SHA512" => Some(Self::Sha512),
            b"SHA3_224" => Some(Self::Sha3_224),
            b"SHA3_256" => Some(Self::Sha3_256),
            b"SHA3_384" => Some(Self::Sha3_384),
            b"SHA3_512" => Some(Self::Sha3_512),
            _ => None,
        }
    }

    /// Compute the lowercase hex digest of `data`.
    #[must_use]
    pub fn hex_digest(self, data: &[u8]) -> String {
        match self {
            Self::Md5 => hex_encode(&md5::Md5::digest(data)),
            Self::Sha1 => hex_encode(&sha1::Sha1::digest(data)),
            Self::Sha224 => hex_encode(&sha2::Sha224::digest(data)),
            Self::Sha256 => hex_encode(&sha2::Sha256::digest(data)),
            Self::Sha384 => hex_encode(&sha2::Sha384::digest(data)),
            Self::Sha512 => hex_encode(&sha2::Sha512::digest(data)),
            Self::Sha3_224 => hex_encode(&sha3::Sha3_224::digest(data)),
            Self::Sha3_256 => hex_encode(&sha3::Sha3_256::digest(data)),
            Self::Sha3_384 => hex_encode(&sha3::Sha3_384::digest(data)),
            Self::Sha3_512 => hex_encode(&sha3::Sha3_512::digest(data)),
        }
    }
}

impl HashAlgo {
    /// Start a streaming hash computation for this algorithm.
    ///
    /// Use the streaming form for large inputs, for example a package file or
    /// `cmake -E sha256sum` on a large file. The result is the same as
    /// [`HashAlgo::hex_digest`] over the concatenation of all chunks.
    #[must_use]
    pub fn hasher(self) -> IncrementalHasher {
        let state = match self {
            Self::Md5 => HasherState::Md5(md5::Md5::new()),
            Self::Sha1 => HasherState::Sha1(sha1::Sha1::new()),
            Self::Sha224 => HasherState::Sha224(sha2::Sha224::new()),
            Self::Sha256 => HasherState::Sha256(sha2::Sha256::new()),
            Self::Sha384 => HasherState::Sha384(sha2::Sha384::new()),
            Self::Sha512 => HasherState::Sha512(sha2::Sha512::new()),
            Self::Sha3_224 => HasherState::Sha3_224(sha3::Sha3_224::new()),
            Self::Sha3_256 => HasherState::Sha3_256(sha3::Sha3_256::new()),
            Self::Sha3_384 => HasherState::Sha3_384(sha3::Sha3_384::new()),
            Self::Sha3_512 => HasherState::Sha3_512(sha3::Sha3_512::new()),
        };
        IncrementalHasher { state }
    }
}

/// Streaming hash state created by [`HashAlgo::hasher`].
///
/// Call [`IncrementalHasher::update`] once for each chunk of input. Then call
/// [`IncrementalHasher::finish_hex`] to get the lowercase hexadecimal digest.
#[derive(Clone, Debug)]
pub struct IncrementalHasher {
    state: HasherState,
}

#[derive(Clone, Debug)]
enum HasherState {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha224(sha2::Sha224),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
    Sha3_224(sha3::Sha3_224),
    Sha3_256(sha3::Sha3_256),
    Sha3_384(sha3::Sha3_384),
    Sha3_512(sha3::Sha3_512),
}

impl IncrementalHasher {
    /// Add the bytes in `data` to the hash input.
    pub fn update(&mut self, data: &[u8]) {
        match &mut self.state {
            HasherState::Md5(h) => h.update(data),
            HasherState::Sha1(h) => h.update(data),
            HasherState::Sha224(h) => h.update(data),
            HasherState::Sha256(h) => h.update(data),
            HasherState::Sha384(h) => h.update(data),
            HasherState::Sha512(h) => h.update(data),
            HasherState::Sha3_224(h) => h.update(data),
            HasherState::Sha3_256(h) => h.update(data),
            HasherState::Sha3_384(h) => h.update(data),
            HasherState::Sha3_512(h) => h.update(data),
        }
    }

    /// Finish the computation and return the lowercase hexadecimal digest.
    #[must_use]
    pub fn finish_hex(self) -> String {
        match self.state {
            HasherState::Md5(h) => hex_encode(&h.finalize()),
            HasherState::Sha1(h) => hex_encode(&h.finalize()),
            HasherState::Sha224(h) => hex_encode(&h.finalize()),
            HasherState::Sha256(h) => hex_encode(&h.finalize()),
            HasherState::Sha384(h) => hex_encode(&h.finalize()),
            HasherState::Sha512(h) => hex_encode(&h.finalize()),
            HasherState::Sha3_224(h) => hex_encode(&h.finalize()),
            HasherState::Sha3_256(h) => hex_encode(&h.finalize()),
            HasherState::Sha3_384(h) => hex_encode(&h.finalize()),
            HasherState::Sha3_512(h) => hex_encode(&h.finalize()),
        }
    }
}

mod rcmake_base_internal {
    pub(super) fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            if let Some(&hi) = HEX.get(usize::from(b >> 4)) {
                out.push(char::from(hi));
            }
            if let Some(&lo) = HEX.get(usize::from(b & 0x0f)) {
                out.push(char::from(lo));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digests_match_rfc() {
        assert_eq!(
            HashAlgo::Md5.hex_digest(b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            HashAlgo::Sha256.hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            HashAlgo::Sha3_256.hex_digest(b"abc"),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
    }

    #[test]
    fn md5_matches_rfc_1321_vectors() {
        assert_eq!(
            HashAlgo::Md5.hex_digest(b""),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            HashAlgo::Md5.hex_digest(b"a"),
            "0cc175b9c0f1b6a831c399e269772661"
        );
        assert_eq!(
            HashAlgo::Md5.hex_digest(b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            HashAlgo::Md5.hex_digest(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn streaming_digest_equals_one_shot_digest_for_every_algorithm() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        for algo in [
            HashAlgo::Md5,
            HashAlgo::Sha1,
            HashAlgo::Sha224,
            HashAlgo::Sha256,
            HashAlgo::Sha384,
            HashAlgo::Sha512,
            HashAlgo::Sha3_224,
            HashAlgo::Sha3_256,
            HashAlgo::Sha3_384,
            HashAlgo::Sha3_512,
        ] {
            let mut hasher = algo.hasher();
            for chunk in data.chunks(997) {
                hasher.update(chunk);
            }
            assert_eq!(hasher.finish_hex(), algo.hex_digest(&data), "{algo:?}");
        }
    }

    #[test]
    fn streaming_digest_of_no_input_is_empty_digest() {
        assert_eq!(
            HashAlgo::Sha256.hasher().finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
