//! Shared [`Host`] conformance suite executed against both [`crate::NativeHost`] and
//! [`crate::PureHost`] to guarantee identical semantics (expanded in Wave M0-b).

use std::path::Path;

use crate::{Host, Stream};

/// Run core invariant assertions against any [`Host`] implementation rooted at `root`.
pub fn run_all(host: &dyn Host, _root: &Path) {
    // Environment overlay round-trip
    host.env_set(b"RCM_TEST_ENV", Some(b"hello"));
    assert_eq!(
        host.env_get(b"RCM_TEST_ENV").as_ref().map(|b| b.as_slice()),
        Some(&b"hello"[..])
    );
    host.env_set(b"RCM_TEST_ENV", None);
    assert!(host.env_get(b"RCM_TEST_ENV").is_none());

    // Relative paths must always be rejected
    assert!(
        host.read(
            Path::new("relative/path.txt"),
            crate::ReadRange::All { limit: 16 }
        )
        .is_err()
    );

    // Smoke stdio write
    host.write_out(Stream::Stdout, b"");
}
