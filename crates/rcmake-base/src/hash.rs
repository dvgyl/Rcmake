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
}
