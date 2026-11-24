//! Stable names for outputs that come from different source directories.

use rcmake_base::{BString, hash::HashAlgo, path_lex::Lexical};
use rcmake_host::{Host, path};

/// Assign stable checksums to files under the configured directories.
pub(super) struct PathChecksum<'a> {
    host: &'a dyn Host,
    roots: [BString; 4],
}

impl<'a> PathChecksum<'a> {
    /// Canonicalize the current and project roots in search order.
    pub(super) fn new(host: &'a dyn Host, roots: [&[u8]; 4]) -> Self {
        Self {
            host,
            roots: roots.map(|root| canonical(host, root)),
        }
    }

    /// Hash the first matching root name and the relative parent path.
    pub(super) fn for_file(&self, file: &[u8]) -> BString {
        let file = canonical(self.host, file);
        let path = Lexical::new(&file);
        let root = path.root_path();
        let (seed, ancestor) = [
            b"CurrentSource".as_slice(),
            b"CurrentBinary",
            b"ProjectSource",
            b"ProjectBinary",
        ]
        .into_iter()
        .zip(&self.roots)
        .find(|(_, dir)| !dir.is_empty() && Lexical::new(dir).prefix(path))
        .unwrap_or((b"FileSystemRoot", &root));

        let relative = Lexical::new(path.parent())
            .relative(Lexical::new(ancestor))
            .filter(|relative| relative != b".")
            .unwrap_or_default();
        encoded_checksum(&[seed, &relative].concat())
    }
}

fn canonical(host: &dyn Host, input: &[u8]) -> BString {
    host.canonicalize(&path::to_native(input)).map_or_else(
        |_| Lexical::new(input).normalize(),
        |p| path::from_native_cmake(&p),
    )
}

fn encoded_checksum(input: &[u8]) -> BString {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let hex = HashAlgo::Sha256.hex_digest(input);
    let mut output = BString::default();
    let mut bits = 0_u8;
    let mut buffer = 0_u16;
    for nibble in hex.bytes().filter_map(|byte| char::from(byte).to_digit(16)) {
        buffer = (buffer << 4) | u16::try_from(nibble).unwrap_or_default();
        bits += 4;
        if bits >= 5 {
            bits -= 5;
            let index = usize::from((buffer >> bits) & 31);
            output.extend(alphabet.get(index).copied());
            if output.len() == 10 {
                break;
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn checksum_uses_the_first_ancestor_and_relative_parent() {
        let host = PureHost::new("/".into(), "/cmake".into());
        let checksum = PathChecksum::new(
            &host,
            [
                b"/project/src",
                b"/project/build",
                b"/project",
                b"/project-out",
            ],
        );
        assert_eq!(checksum.for_file(b"/project/src/main.cpp"), "EWIEGA46WW");
        assert_eq!(checksum.for_file(b"/project/src/other.h"), "EWIEGA46WW");
        assert_eq!(
            checksum.for_file(b"/project/src/include/private.h"),
            "6YEA5652QU"
        );
        assert_eq!(checksum.for_file(b"/outside/main.cpp"), "PPV3PIMIHJ");
        assert_ne!(
            checksum.for_file(b"/project/srcx/main.cpp"),
            checksum.for_file(b"/project/src/main.cpp"),
        );
    }
}
