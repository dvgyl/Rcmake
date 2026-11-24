//! Byte-preserving templates for probe inputs and output.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_host::path;
use rcmake_store::{Digest, Domain, KeyHasher};
use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEGMENTS: usize = 32 * 1024;

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
        }
    }
}

/// Path and target spellings for one probe call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathBindings(BTreeMap<Token, Vec<u8>>);

impl PathBindings {
    /// Bind the four absolute directories in both native and CMake form.
    ///
    /// Reject a filesystem root because it would match unrelated paths.
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
            if !dir.is_absolute() || dir.parent().is_none() {
                return None;
            }
            values.insert(cmake, path::from_native_cmake(dir).to_vec());
            values.insert(native, path::from_native(dir).to_vec());
        }
        Some(Self(values))
    }

    /// Bind the private scratch directory selected by the worker.
    pub(crate) fn with_scratch(mut self, scratch: &Path) -> Option<Self> {
        if !scratch.is_absolute() || scratch.parent().is_none() {
            return None;
        }
        self.0
            .insert(Token::Scratch, path::from_native_cmake(scratch).to_vec());
        self.0
            .insert(Token::NativeScratch, path::from_native(scratch).to_vec());
        Some(self)
    }

    /// Bind the generated target name.
    pub(crate) fn with_target(mut self, target: &[u8]) -> Option<Self> {
        if target.is_empty()
            || !target
                .iter()
                .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        self.0.insert(Token::Target, target.to_vec());
        Some(self)
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Segment {
    Literal(Vec<u8>),
    Binding(Vec<Token>),
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
        let mut matches = BTreeMap::<&[u8], Vec<Token>>::new();
        for (&token, value) in &bindings.0 {
            matches.entry(value).or_default().push(token);
        }
        let mut matches: Vec<_> = matches.into_iter().collect();
        matches.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        let mut segments = Vec::new();
        let mut literal = Vec::new();
        let mut rest = bytes;
        while let Some((&byte, tail)) = rest.split_first() {
            if let Some((value, tokens)) = matches.iter().find(|(value, _)| rest.starts_with(value))
            {
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
        if self.0.len() > MAX_SEGMENTS {
            return None;
        }
        let mut output = Vec::new();
        for segment in &self.0 {
            let bytes = match segment {
                Segment::Literal(bytes) => bytes,
                Segment::Binding(tokens) => {
                    let (first, rest) = tokens.split_first()?;
                    let value = bindings.0.get(first)?;
                    if !rest
                        .iter()
                        .all(|token| bindings.0.get(token) == Some(value))
                    {
                        return None;
                    }
                    value
                }
            };
            if output.len().saturating_add(bytes.len()) > MAX_BYTES {
                return None;
            }
            output.extend_from_slice(bytes);
        }
        Some(output)
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
                    for token in tokens {
                        key.field(3, token.marker());
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
