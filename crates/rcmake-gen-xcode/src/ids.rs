//! Stable Xcode object IDs and cache entries.

use std::fmt::Write as _;

use rcmake_model::{Cache, CacheEntry, CacheType};

use sha2::{Digest, Sha256};

use crate::object::Isa;

pub(crate) fn generate(isa: Isa, key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((isa as u32).to_le_bytes());
    hash.update(key.as_bytes());
    let mut id = String::with_capacity(24);
    for byte in hash.finalize().iter().take(12) {
        let _ = write!(id, "{byte:02X}");
    }
    id
}

pub(crate) fn cached<'a>(cache: &'a Cache, key: &str) -> Option<&'a str> {
    let value = std::str::from_utf8(cache.get(key.as_bytes())?.value.as_slice()).ok()?;
    (value.len() == 24
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)))
    .then_some(value)
}

pub(crate) fn store(cache: &mut Cache, name: String, isa: Isa, key: &str) {
    if cached(cache, &name).is_none() {
        cache.insert(
            name.into(),
            CacheEntry {
                value: generate(isa, key).into(),
                ty: CacheType::Internal,
                help: "Stored Xcode object GUID".into(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_the_little_endian_type_and_the_role() {
        let id = generate(Isa::PBXNativeTarget, "target/app");
        assert_eq!(id, "C32980B9E35378C7F8F1F755");
        assert_ne!(id, generate(Isa::PBXAggregateTarget, "target/app"));
        assert_ne!(id, generate(Isa::PBXNativeTarget, "target/other"));
    }
}
