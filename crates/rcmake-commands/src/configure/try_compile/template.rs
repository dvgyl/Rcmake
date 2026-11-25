//! Byte-preserving templates for probe inputs and output.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Component, Path};

use rcmake_host::{Host, path};
use rcmake_store::{Digest, Domain, KeyHasher};
use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEGMENTS: usize = 32 * 1024;
const MAX_BINDING_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum Token {
    Scratch,
    Build,
    Source,
    CmakeRoot,
    NativeScratch,
    NativeBuild,
    NativeSource,
    NativeCmakeRoot,
    Target,
    CanonicalScratch,
    CanonicalBuild,
    CanonicalSource,
    CanonicalCmakeRoot,
    AlternateScratch,
    AlternateBuild,
    AlternateSource,
    AlternateCmakeRoot,
}

impl Token {
    fn marker(self) -> &'static [u8] {
        match self {
            Self::Scratch => b"{{SCRATCH}}",
            Self::Build => b"{{BUILD}}",
            Self::Source => b"{{SOURCE}}",
            Self::CmakeRoot => b"{{CMAKE_ROOT}}",
            Self::NativeScratch => b"{{NATIVE_SCRATCH}}",
            Self::NativeBuild => b"{{NATIVE_BUILD}}",
            Self::NativeSource => b"{{NATIVE_SOURCE}}",
            Self::NativeCmakeRoot => b"{{NATIVE_CMAKE_ROOT}}",
            Self::Target => b"{{TARGET}}",
            Self::CanonicalScratch => b"{{CANONICAL_SCRATCH}}",
            Self::CanonicalBuild => b"{{CANONICAL_BUILD}}",
            Self::CanonicalSource => b"{{CANONICAL_SOURCE}}",
            Self::CanonicalCmakeRoot => b"{{CANONICAL_CMAKE_ROOT}}",
            Self::AlternateScratch => b"{{ALTERNATE_SCRATCH}}",
            Self::AlternateBuild => b"{{ALTERNATE_BUILD}}",
            Self::AlternateSource => b"{{ALTERNATE_SOURCE}}",
            Self::AlternateCmakeRoot => b"{{ALTERNATE_CMAKE_ROOT}}",
        }
    }
}

/// Path and target spellings for one probe call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathBindings(BTreeMap<Token, Vec<u8>>);

impl PathBindings {
    /// Bind the four absolute directories in both native and CMake form.
    ///
    /// Reject shallow paths because they can match unrelated paths.
    pub(crate) fn new(
        scratch: &Path,
        build: &Path,
        source: &Path,
        cmake_root: &Path,
    ) -> Option<Self> {
        let mut values = BTreeMap::new();
        for (dir, cmake, native) in [
            (scratch, Token::Scratch, Token::NativeScratch),
            (build, Token::Build, Token::NativeBuild),
            (source, Token::Source, Token::NativeSource),
            (cmake_root, Token::CmakeRoot, Token::NativeCmakeRoot),
        ] {
            if !valid_binding(dir) {
                return None;
            }
            values.insert(cmake, path::from_native_cmake(dir).to_vec());
            values.insert(native, path::from_native(dir).to_vec());
        }
        Some(Self(values))
    }

    /// Bind the private scratch directory selected by the worker.
    pub(crate) fn with_scratch(mut self, scratch: &Path) -> Option<Self> {
        if !valid_binding(scratch) {
            return None;
        }
        self.0.remove(&Token::CanonicalScratch);
        self.0.remove(&Token::AlternateScratch);
        self.0
            .insert(Token::Scratch, path::from_native_cmake(scratch).to_vec());
        self.0
            .insert(Token::NativeScratch, path::from_native(scratch).to_vec());
        Some(self)
    }

    /// Resolve alternate native spellings through the same host that runs the probe.
    pub(crate) fn with_canonical(mut self, host: &dyn Host) -> Self {
        for (native, canonical, alternate) in [
            (
                Token::NativeScratch,
                Token::CanonicalScratch,
                Token::AlternateScratch,
            ),
            (
                Token::NativeBuild,
                Token::CanonicalBuild,
                Token::AlternateBuild,
            ),
            (
                Token::NativeSource,
                Token::CanonicalSource,
                Token::AlternateSource,
            ),
            (
                Token::NativeCmakeRoot,
                Token::CanonicalCmakeRoot,
                Token::AlternateCmakeRoot,
            ),
        ] {
            self.0.remove(&canonical);
            self.0.remove(&alternate);
            let Some(value) = self.0.get(&native) else {
                continue;
            };
            let native_path = path::to_native(value);
            let Some(resolved) = canonical_with_missing_tail(host, &native_path) else {
                continue;
            };
            if valid_binding(&resolved) {
                self.0
                    .insert(canonical, path::from_native(&resolved).to_vec());
            }
            for (long, short) in [("/private/var", "/var"), ("/private/tmp", "/tmp")] {
                let alias = native_path
                    .strip_prefix(long)
                    .ok()
                    .map(|tail| Path::new(short).join(tail))
                    .or_else(|| {
                        native_path
                            .strip_prefix(short)
                            .ok()
                            .map(|tail| Path::new(long).join(tail))
                    });
                if let Some(alias) = alias
                    && valid_binding(&alias)
                    && canonical_with_missing_tail(host, &alias).as_ref() == Some(&resolved)
                    && alias != resolved
                {
                    self.0.insert(alternate, path::from_native(&alias).to_vec());
                    break;
                }
            }
        }
        self
    }

    /// Check that the alias scan can cover the path spelling on this platform.
    pub(crate) fn can_scan_aliases(&self) -> bool {
        !cfg!(any(windows, target_os = "macos")) || self.0.values().all(|value| value.is_ascii())
    }

    /// Detect a binding that remains in a literal or executable.
    pub(crate) fn has_leak(&self, bytes: &[u8]) -> bool {
        self.leaks(bytes, true)
    }

    /// Detect a path binding that remains in a compiled artifact.
    ///
    /// CMake gives each probe target a random name, and Apple linkers sign an
    /// executable with that name. No caller can depend on the name in the
    /// artifact, but an embedded path can change behavior after relocation.
    pub(crate) fn has_path_leak(&self, bytes: &[u8]) -> bool {
        self.leaks(bytes, false)
    }

    fn leaks(&self, bytes: &[u8], target: bool) -> bool {
        self.0.iter().any(|(&token, value)| {
            if token == Token::Target && !target {
                return false;
            }
            spellings(token, value).into_iter().any(|(_, spelling)| {
                let insensitive =
                    (cfg!(windows) || cfg!(target_os = "macos")) && token != Token::Target;
                let separators = token != Token::Target && cfg!(windows);
                contains(bytes, &spelling, insensitive, separators)
                    || String::from_utf8(spelling).is_ok_and(|text| {
                        let wide: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
                        contains(bytes, &wide, insensitive, separators)
                    })
            })
        })
    }

    /// Bind the generated target name.
    pub(crate) fn with_target(mut self, target: &[u8]) -> Option<Self> {
        if target.is_empty()
            || target.len() > 128
            || !target
                .iter()
                .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        self.0.insert(Token::Target, target.to_vec());
        Some(self)
    }

    /// Estimate bytes retained by these bindings.
    pub(crate) fn byte_size(&self) -> usize {
        self.0.values().map(|value| 64 + value.len()).sum()
    }

    /// Identify the exact call when relocation has no proof.
    pub(crate) fn digest(&self) -> Digest {
        let mut key = KeyHasher::new(Domain::ProbeV1);
        key.field(0, b"path-bindings");
        for (token, value) in &self.0 {
            key.field(1, token.marker());
            key.field(2, value);
        }
        key.finish()
    }
}

fn canonical_with_missing_tail(host: &dyn Host, path: &Path) -> Option<std::path::PathBuf> {
    use rcmake_host::Follow;
    use std::io::ErrorKind;

    for ancestor in path.ancestors() {
        match host.canonicalize(ancestor) {
            Ok(resolved) => {
                let suffix = path.strip_prefix(ancestor).ok()?;
                return Some(join_like(&resolved, suffix));
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                // A dangling link can resolve outside its spelling, so do not
                // reconstruct it from a canonical parent.
                if !host
                    .metadata(ancestor, Follow::No)
                    .is_err_and(|error| error.kind == ErrorKind::NotFound)
                {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// Append `suffix` to `base` with the separator that `base` uses. `Path::join`
/// adds `\` on Windows, so a `/`-form canonical path would get mixed separators
/// and stop matching the spelling of an existing directory.
fn join_like(base: &Path, suffix: &Path) -> std::path::PathBuf {
    let mut bytes = path::from_native(base).to_vec();
    let separator = if cfg!(windows) && bytes.contains(&b'\\') {
        b'\\'
    } else {
        b'/'
    };
    for part in suffix.components() {
        if !(bytes.ends_with(b"/") || cfg!(windows) && bytes.ends_with(b"\\")) {
            bytes.push(separator);
        }
        bytes.extend_from_slice(&path::from_native(Path::new(part.as_os_str())));
    }
    path::to_native(&bytes)
}

fn valid_binding(path: &Path) -> bool {
    let bytes = path::from_native(path);
    path.is_absolute()
        && !bytes.contains(&0)
        && bytes.len() <= MAX_BINDING_BYTES
        && path
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .count()
            >= 2
        && !path.components().any(|part| part == Component::ParentDir)
        && path::to_native(&bytes) == path
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum Encoding {
    Raw,
    Forward,
    Backslash,
    LowerDrive,
    UpperDrive,
    Escaped,
    Quoted,
}

impl Encoding {
    fn apply(self, value: &[u8]) -> Vec<u8> {
        let mut value = value.to_vec();
        match self {
            Self::Raw => {}
            Self::Forward => value
                .iter_mut()
                .filter(|byte| **byte == b'\\')
                .for_each(|byte| *byte = b'/'),
            Self::Backslash => value
                .iter_mut()
                .filter(|byte| **byte == b'/')
                .for_each(|byte| *byte = b'\\'),
            Self::LowerDrive | Self::UpperDrive => {
                let index = drive_index(&value);
                if let Some(byte) = index.and_then(|index| value.get_mut(index)) {
                    if self == Self::LowerDrive {
                        byte.make_ascii_lowercase();
                    } else {
                        byte.make_ascii_uppercase();
                    }
                }
            }
            Self::Escaped | Self::Quoted => {
                return value
                    .into_iter()
                    .flat_map(|byte| {
                        (matches!(byte, b'\\' | b'"')
                            || (self == Self::Escaped && matches!(byte, b' ' | b'\'')))
                        .then_some(b'\\')
                        .into_iter()
                        .chain([byte])
                    })
                    .collect();
            }
        }
        value
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Binding {
    token: Token,
    encodings: Vec<Encoding>,
    raw_special: Vec<u8>,
}

impl Binding {
    fn value(&self, bindings: &PathBindings) -> Option<Vec<u8>> {
        if self.encodings.is_empty() || self.encodings.len() > 3 {
            return None;
        }
        let mut value = bindings.0.get(&self.token)?.clone();
        for encoding in &self.encodings {
            value = encoding.apply(&value);
        }
        if special_bytes(&value) != self.raw_special {
            return None;
        }
        Some(value)
    }
}

fn special_bytes(value: &[u8]) -> Vec<u8> {
    value
        .iter()
        .copied()
        .filter(|byte| {
            !byte.is_ascii_alphanumeric()
                && !matches!(byte, b'/' | b'\\' | b':' | b'.' | b'-' | b'_')
        })
        .collect()
}

fn drive_index(bytes: &[u8]) -> Option<usize> {
    let (offset, bytes) = if let Some(tail) = bytes
        .strip_prefix(b"\\\\?\\")
        .or_else(|| bytes.strip_prefix(b"//?/"))
    {
        (4, tail)
    } else {
        (0, bytes)
    };
    matches!(bytes, [letter, b':', ..] if letter.is_ascii_alphabetic()).then_some(offset)
}

fn spellings(token: Token, value: &[u8]) -> Vec<(Binding, Vec<u8>)> {
    let mut spellings = Vec::new();
    let mut add = |encodings: Vec<Encoding>| {
        let encoded = encodings
            .iter()
            .fold(value.to_vec(), |value, encoding| encoding.apply(&value));
        if !spellings.iter().any(|(_, spelling)| *spelling == encoded) {
            let raw_special = special_bytes(&encoded);
            spellings.push((
                Binding {
                    token,
                    encodings,
                    raw_special,
                },
                encoded,
            ));
        }
    };
    add(vec![Encoding::Raw]);
    if token != Token::Target {
        if drive_index(value).is_some() || value.starts_with(b"\\\\") || value.starts_with(b"//") {
            for slash in [Encoding::Raw, Encoding::Forward, Encoding::Backslash] {
                for drive in [Encoding::Raw, Encoding::LowerDrive, Encoding::UpperDrive] {
                    add(vec![slash, drive]);
                    add(vec![slash, drive, Encoding::Escaped]);
                    add(vec![slash, drive, Encoding::Quoted]);
                }
            }
        }
        add(vec![Encoding::Escaped]);
        add(vec![Encoding::Quoted]);
    }
    spellings
}

fn contains(haystack: &[u8], needle: &[u8], insensitive: bool, separators: bool) -> bool {
    let separator = |byte| matches!(byte, b'/' | b'\\');
    haystack.windows(needle.len()).any(|window| {
        window.iter().zip(needle).all(|(&left, &right)| {
            left == right
                || (insensitive && left.eq_ignore_ascii_case(&right))
                || (separators && separator(left) && separator(right))
        })
    })
}

fn delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'"' | b'\'' | b':' | b';' | b'=' | b'(' | b')' | b'[' | b']' | b'<' | b'>' | b','
        )
}

fn boundary(
    bytes: &[u8],
    offset: usize,
    len: usize,
    token: Token,
    object_directory: Option<&[u8]>,
) -> bool {
    let previous = offset
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .copied();
    let next = bytes.get(offset + len).copied();
    let separator = |byte| matches!(byte, b'/' | b'\\');
    let inside_object_name = matches!(
        token,
        Token::Source
            | Token::NativeSource
            | Token::CanonicalSource
            | Token::AlternateSource
            | Token::CmakeRoot
            | Token::NativeCmakeRoot
            | Token::CanonicalCmakeRoot
            | Token::AlternateCmakeRoot
    ) && object_directory.is_some_and(|directory| {
        bytes
            .get(..offset)
            .is_some_and(|prefix| prefix.rsplit(|&byte| delimiter(byte)).next() == Some(directory))
    });
    // An attached option such as `-I` or `-isystem` starts a path argument.
    let attached_option = token != Token::Target
        && bytes.get(..offset).is_some_and(|prefix| {
            prefix
                .rsplit(|&byte| delimiter(byte))
                .next()
                .and_then(|word| word.strip_prefix(b"-"))
                .is_some_and(|name| {
                    (1..=16).contains(&name.len()) && name.iter().all(u8::is_ascii_alphabetic)
                })
        });
    (inside_object_name
        || attached_option
        || previous
            .is_none_or(|byte| delimiter(byte) || (token == Token::Target && separator(byte))))
        && next.is_none_or(|byte| {
            delimiter(byte) || separator(byte) || (token == Token::Target && byte == b'.')
        })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Segment {
    Literal(Vec<u8>),
    Binding(Vec<Binding>),
}

/// Probe bytes with typed substitutions. Literal marker text remains literal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Templated(Vec<Segment>);

impl Templated {
    /// Preserve bytes as a literal, including text that looks like a path or marker.
    pub(crate) fn literal(bytes: &[u8]) -> Option<Self> {
        (bytes.len() <= MAX_BYTES).then(|| Self(vec![Segment::Literal(bytes.to_vec())]))
    }

    /// Substitute the longest matching binding at each byte position.
    ///
    /// Keep all aliases of a binding. Render only when they still have equal values.
    pub(crate) fn capture(bytes: &[u8], bindings: &PathBindings) -> Option<Self> {
        if bytes.len() > MAX_BYTES {
            return None;
        }
        let mut matches = BTreeMap::<Vec<u8>, Vec<Binding>>::new();
        for (&token, value) in &bindings.0 {
            for (binding, spelling) in spellings(token, value) {
                matches.entry(spelling).or_default().push(binding);
            }
        }
        let mut matches: Vec<_> = matches.into_iter().collect();
        matches.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        let mut segments = Vec::new();
        let mut literal = Vec::new();
        let mut rest = bytes;
        let object_directory = bindings
            .0
            .get(&Token::Target)
            .map(|target| [b"CMakeFiles/".as_slice(), target, b".dir"].concat());
        while let Some((&byte, tail)) = rest.split_first() {
            let offset = bytes.len() - rest.len();
            if let Some((value, tokens)) = matches.iter().find(|(value, tokens)| {
                rest.starts_with(value)
                    && tokens.iter().all(|binding| {
                        boundary(
                            bytes,
                            offset,
                            value.len(),
                            binding.token,
                            object_directory.as_deref(),
                        )
                    })
            }) {
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                segments.push(Segment::Binding(tokens.clone()));
                rest = rest.get(value.len()..)?;
            } else {
                literal.push(byte);
                rest = tail;
            }
            if segments.len() > MAX_SEGMENTS {
                return None;
            }
        }
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        let template = Self(segments);
        round_trip_verify(&template, bindings, bytes).then_some(template)
    }

    /// Render bytes for the current call. Reject missing or conflicting bindings.
    pub(crate) fn render(&self, bindings: &PathBindings) -> Option<Vec<u8>> {
        self.render_bounded(bindings, MAX_BYTES)
    }

    /// Render while enforcing the supplied remaining byte budget.
    pub(crate) fn render_bounded(&self, bindings: &PathBindings, limit: usize) -> Option<Vec<u8>> {
        let limit = limit.min(MAX_BYTES);
        if self.0.len() > MAX_SEGMENTS {
            return None;
        }
        let mut output = Vec::new();
        for segment in &self.0 {
            let rendered;
            let bytes = match segment {
                Segment::Literal(bytes) => bytes,
                Segment::Binding(tokens) => {
                    if tokens.len() > 32 {
                        return None;
                    }
                    let (first, rest) = tokens.split_first()?;
                    rendered = first.value(bindings)?;
                    if !rest
                        .iter()
                        .all(|token| token.value(bindings).as_ref() == Some(&rendered))
                    {
                        return None;
                    }
                    &rendered
                }
            };
            if output.len().saturating_add(bytes.len()) > limit {
                return None;
            }
            output.extend_from_slice(bytes);
        }
        Some(output)
    }

    /// Return a conservative JSON byte bound without encoding the template.
    pub(crate) fn encoded_size_bound(&self) -> usize {
        self.0.iter().fold(16usize, |size, segment| {
            let bytes = match segment {
                Segment::Literal(bytes) => bytes.len().saturating_mul(4),
                Segment::Binding(bindings) => bindings.iter().fold(0usize, |sum, binding| {
                    sum.saturating_add(160)
                        .saturating_add(binding.encodings.len().saturating_mul(16))
                        .saturating_add(binding.raw_special.len().saturating_mul(4))
                }),
            };
            size.saturating_add(32).saturating_add(bytes)
        })
    }

    /// Check for binding bytes that remain outside typed substitutions.
    pub(crate) fn has_leak(&self, bindings: &PathBindings) -> bool {
        self.0
            .iter()
            .any(|segment| matches!(segment, Segment::Literal(bytes) if bindings.has_leak(bytes)))
    }

    /// Identify the typed segments independently of the values of their bindings.
    pub(crate) fn digest(&self) -> Digest {
        let mut key = KeyHasher::new(Domain::ProbeV1);
        key.field(0, b"template");
        for segment in &self.0 {
            match segment {
                Segment::Literal(bytes) => key.field(1, bytes),
                Segment::Binding(tokens) => {
                    key.field(2, &[]);
                    for binding in tokens {
                        key.field(3, binding.token.marker());
                        for encoding in &binding.encodings {
                            key.field(4, &[*encoding as u8]);
                        }
                        key.field(5, &binding.raw_special);
                    }
                }
            }
        }
        key.finish()
    }
}

/// Check that rendering restores every original byte.
pub(crate) fn round_trip_verify(
    template: &Templated,
    bindings: &PathBindings,
    original: &[u8],
) -> bool {
    template.render(bindings).as_deref() == Some(original)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn bindings(root: &Path) -> Option<PathBindings> {
        PathBindings::new(
            &root.join("build/scratch"),
            &root.join("build"),
            &root.join("source"),
            &root.join("cmake"),
        )?
        .with_target(b"cmTC_1")
    }

    #[test]
    fn artifact_path_leaks_ignore_the_probe_target_name() -> Result<(), &'static str> {
        let paths = bindings(Path::new("/old")).ok_or("Missing bindings.")?;
        assert!(paths.has_leak(b"\0cmTC_1\0"));
        assert!(!paths.has_path_leak(b"\0cmTC_1\0"));
        assert!(paths.has_path_leak(b"\0/old/build/scratch/x.o\0"));
        Ok(())
    }

    #[test]
    fn attached_options_rebind_paths_and_keep_other_words() -> Result<(), &'static str> {
        let old = bindings(Path::new("/old")).ok_or("Missing original bindings.")?;
        let new = bindings(Path::new("/new")).ok_or("Missing new bindings.")?;
        let original = b"cc -I/old/source/inc -isystem/old/cmake/x -L/old/build x/old/source";
        let template = Templated::capture(original, &old).ok_or("Cannot capture output.")?;
        let rendered = template.render(&new).ok_or("Cannot render output.")?;
        assert_eq!(
            rendered.as_slice(),
            b"cc -I/new/source/inc -isystem/new/cmake/x -L/new/build x/old/source".as_slice()
        );
        assert!(template.has_leak(&old));
        let attached = Templated::capture(b"-I/old/source/inc -D1/old/source", &old)
            .ok_or("Cannot capture attached output.")?;
        assert!(attached.has_leak(&old));
        Ok(())
    }

    #[test]
    fn round_trip_and_rebind_opaque_bytes() -> Result<(), &'static str> {
        let old = bindings(Path::new("/old")).ok_or("Missing original bindings.")?;
        let new = bindings(Path::new("/new")).ok_or("Missing new bindings.")?;
        let original =
            b"\xff /old/build/scratch/cmTC_1 /old/build /old/source /old/cmake {{SCRATCH}}";
        let template = Templated::capture(original, &old).ok_or("Cannot capture output.")?;
        assert!(round_trip_verify(&template, &old, original));
        assert!(!round_trip_verify(&template, &new, original));
        assert_eq!(
            template.render(&new).as_deref(),
            Some(
                b"\xff /new/build/scratch/cmTC_1 /new/build /new/source /new/cmake {{SCRATCH}}"
                    .as_slice()
            )
        );
        Ok(())
    }

    #[test]
    fn preserves_component_boundaries_and_rejects_leaked_aliases() -> Result<(), &'static str> {
        let old = bindings(Path::new("/old")).ok_or("Missing original bindings.")?;
        let new = bindings(Path::new("/new")).ok_or("Missing new bindings.")?;
        let input = b"/old/buildtools/bin/cc my_cmTC_1_helper /old/build";
        let template = Templated::capture(input, &old).ok_or("Cannot capture output.")?;
        assert_eq!(
            template.render(&new).as_deref(),
            Some(b"/old/buildtools/bin/cc my_cmTC_1_helper /new/build".as_slice())
        );
        assert!(template.has_leak(&old));
        assert!(bindings(Path::new("/")).is_none());
        Ok(())
    }

    #[test]
    fn recognizes_resolved_aliases_and_escaped_paths() -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_host::PureHost;

        let host = PureHost::default();
        host.create_dir_all(Path::new("/private/var/example/build/scratch"))?;
        host.create_dir_all(Path::new("/var"))?;
        host.symlink(Path::new("/private/var/example"), Path::new("/var/example"))?;
        let paths = PathBindings::new(
            Path::new("/var/example/build/scratch"),
            Path::new("/var/example/build"),
            Path::new("/var/example/source files"),
            Path::new("/var/example/cmake"),
        )
        .ok_or("Missing original bindings.")?
        .with_canonical(&host);
        let artifact = b"embedded -> /private/var/example/build/scratch/probe.c";
        let canonical_paths = PathBindings::new(
            Path::new("/private/var/example/build/scratch"),
            Path::new("/private/var/example/build"),
            Path::new("/private/var/example/source files"),
            Path::new("/private/var/example/cmake"),
        )
        .ok_or("Missing canonical bindings.")?
        .with_canonical(&host);
        assert!(canonical_paths.has_leak(b"/var/example/build/scratch/probe.c"));
        assert!(paths.has_leak(artifact));
        assert!(paths.has_leak(b"/var/example/source\\ files"));
        let template = Templated::capture(artifact, &paths).ok_or("Cannot capture output.")?;
        assert!(!template.has_leak(&paths));
        Ok(())
    }

    #[test]
    fn an_aliased_path_cannot_split_at_replay() -> Result<(), &'static str> {
        let old = PathBindings::new(
            Path::new("/old/scratch"),
            Path::new("/old/work"),
            Path::new("/old/work"),
            Path::new("/old/cmake"),
        )
        .ok_or("Missing original bindings.")?;
        let new = bindings(Path::new("/new")).ok_or("Missing new bindings.")?;
        let template = Templated::capture(b"/old/work", &old).ok_or("Cannot capture output.")?;
        assert_eq!(template.render(&new), None);
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn binds_drive_and_slash_variants() -> Result<(), &'static str> {
        let bindings = PathBindings::new(
            Path::new("C:/old/build/scratch"),
            Path::new("C:/old/build"),
            Path::new("C:/old/source files"),
            Path::new("C:/old/cmake"),
        )
        .ok_or("Missing original bindings.")?;
        let relocated = PathBindings::new(
            Path::new("D:/new/build/scratch"),
            Path::new("D:/new/build"),
            Path::new("D:/new/source files"),
            Path::new("D:/new/cmake"),
        )
        .ok_or("Missing relocated bindings.")?;
        let input = br#"c:\old\source files\probe.c C:/old/build/probe.obj"#;
        let template = Templated::capture(input, &bindings).ok_or("Cannot capture output.")?;
        assert!(round_trip_verify(&template, &bindings, input));
        assert!(!template.has_leak(&bindings));
        assert_eq!(
            template.render(&relocated).as_deref(),
            Some(br#"d:\new\source files\probe.c D:/new/build/probe.obj"#.as_slice())
        );

        let neighbor = b"C:/old/buildtools/cc";
        let template = Templated::capture(neighbor, &bindings).ok_or("Cannot capture neighbor.")?;
        assert_eq!(
            template.render(&relocated).as_deref(),
            Some(neighbor.as_slice())
        );
        assert!(template.has_leak(&bindings));

        let alias = r"c:\OLD\BUILD\SCRATCH\a.obj";
        let utf16: Vec<_> = alias.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(bindings.has_path_leak(alias.as_bytes()));
        assert!(bindings.has_path_leak(&utf16));
        Ok(())
    }
}

#[cfg(test)]
mod canonical_tests {
    use super::*;
    use rcmake_host::PureHost;

    #[cfg(unix)]
    #[test]
    fn missing_scratch_joins_a_unix_parent_with_a_literal_backslash() -> rcmake_host::HostResult<()>
    {
        let host = PureHost::default();
        let root = Path::new(r"/work\");
        host.create_dir_all(root)?;
        let scratch = root.join("scratch");
        assert_eq!(canonical_with_missing_tail(&host, &scratch), Some(scratch));
        Ok(())
    }

    #[test]
    fn missing_scratch_keeps_the_canonical_separator_style() -> Result<(), String> {
        let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::default();
        host.create_dir_all(root).map_err(|e| e.to_string())?;
        let bindings = PathBindings::new(
            &root.join("scratch"),
            &root.join("build"),
            &root.join("source"),
            &root.join("cmake"),
        )
        .ok_or("Missing bindings.")?
        .with_canonical(&host);
        let expected = format!("{}/scratch", root.display());
        assert_eq!(
            bindings.0.get(&Token::CanonicalScratch).map(Vec::as_slice),
            Some(expected.as_bytes())
        );
        assert_eq!(
            bindings.0.get(&Token::Scratch).map(Vec::as_slice),
            Some(expected.as_bytes())
        );
        Ok(())
    }
}
