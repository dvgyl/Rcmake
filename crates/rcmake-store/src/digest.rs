use std::fmt;

use rcmake_json::{FieldError, FromJson, ToJson, Value};
use sha2::{Digest as _, Sha256};

/// A SHA-256 digest. Its serialized form is a lowercase hex string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Construct a digest from its 32 raw bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compute the SHA-256 digest of bytes.
    pub fn hash(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Decode 64 ASCII hex digits.
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (out, &[hi, lo]) in bytes.iter_mut().zip(pairs) {
            *out = ((char::from(hi).to_digit(16)? << 4) | char::from(lo).to_digit(16)?) as u8;
        }
        Some(Self(bytes))
    }

    /// Encode as 64 lowercase hex digits.
    pub fn to_hex(self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for Digest {
    type Error = &'static str;

    fn try_from(hex: String) -> Result<Self, Self::Error> {
        Self::from_hex(&hex).ok_or("Expected 64 hex digits.")
    }
}

impl From<Digest> for String {
    fn from(digest: Digest) -> Self {
        digest.to_hex()
    }
}

impl ToJson for Digest {
    fn to_json(&self) -> Value {
        Value::string(self.to_hex())
    }
}

impl FromJson for Digest {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let hex = rcmake_json::str(value)?;
        Self::from_hex(hex).ok_or_else(|| FieldError::invalid("Expected 64 hex digits."))
    }
}

/// A versioned namespace for a cache key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Domain {
    /// Compiler and platform probe results.
    ProbeV1,
    /// FetchContent results.
    FetchV1,
    /// Configure results.
    ConfigureV1,
    /// Keys derived from blob identities.
    BlobV1,
}

impl ToJson for Domain {
    fn to_json(&self) -> Value {
        Value::string(match self {
            Self::ProbeV1 => "ProbeV1",
            Self::FetchV1 => "FetchV1",
            Self::ConfigureV1 => "ConfigureV1",
            Self::BlobV1 => "BlobV1",
        })
    }
}

impl FromJson for Domain {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match rcmake_json::str(value)? {
            "ProbeV1" => Ok(Self::ProbeV1),
            "FetchV1" => Ok(Self::FetchV1),
            "ConfigureV1" => Ok(Self::ConfigureV1),
            "BlobV1" => Ok(Self::BlobV1),
            other => Err(FieldError::invalid(format!("unknown domain '{other}'"))),
        }
    }
}

impl Domain {
    pub(crate) const ALL: [Self; 4] = [
        Self::ProbeV1,
        Self::FetchV1,
        Self::ConfigureV1,
        Self::BlobV1,
    ];

    /// Return the stable name used in paths and key hashes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProbeV1 => "probe-v1",
            Self::FetchV1 => "fetch-v1",
            Self::ConfigureV1 => "configure-v1",
            Self::BlobV1 => "blob-v1",
        }
    }

    /// Hash one byte string as a key in this domain.
    pub fn key(self, bytes: &[u8]) -> Digest {
        let mut hasher = KeyHasher::new(self);
        hasher.field(0, bytes);
        hasher.finish()
    }
}

/// Builds a domain-separated key from tagged, length-prefixed fields.
pub struct KeyHasher(Sha256);

impl KeyHasher {
    /// Start a key for a domain.
    pub fn new(domain: Domain) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"rcmake-store\0");
        hasher.update(domain.as_str().as_bytes());
        hasher.update([0]);
        Self(hasher)
    }

    /// Add a field. Sort unordered collections before adding their fields.
    pub fn field(&mut self, tag: u16, bytes: &[u8]) {
        self.0.update(tag.to_le_bytes());
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    /// Return the key after all fields have been added.
    pub fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}
