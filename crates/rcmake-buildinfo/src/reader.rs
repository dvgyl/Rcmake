//! Bounded, contained reads of build-tree artifacts.
//!
//! Every read checks the file kind and containment before it opens the file. Each
//! path component under the root must be a real directory, the last component must
//! be a regular file, and no component can be a symbolic link or a reparse point.
//! The canonical path must stay under the canonical root.
//!
//! The [`Host`] trait opens files by path, so a concurrent replacement can still
//! change a path between the check and the read. After each read, the reader checks
//! the file identity again and rejects the data when the file changed. A replaced
//! file is also bounded by the per-file and cumulative byte limits.

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange};
use rcmake_json::{ParseBudget, ReadOptions, Value};

/// Reason why an artifact read failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The file does not exist or cannot be read.
    Missing,
    /// The path is not a regular file, is a link, or leaves the root.
    Rejected(&'static str),
    /// The file or the cumulative budget exceeds its limit.
    TooLarge,
    /// The file is not valid JSON within the parse budget.
    Json(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("The file is missing or unreadable."),
            Self::Rejected(reason) => f.write_str(reason),
            Self::TooLarge => f.write_str("The file exceeds its size limit."),
            Self::Json(message) => write!(f, "The file is not valid JSON. {message}"),
        }
    }
}

/// Limits of one reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// Largest accepted file.
    pub max_file_bytes: u64,
    /// Largest accepted total of all reads.
    pub max_total_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 << 20,
            max_total_bytes: 512 << 20,
        }
    }
}

/// Reader confined to one canonical root directory.
pub struct ArtifactReader<'h> {
    host: &'h dyn Host,
    root: PathBuf,
    /// Root as the caller spelled it, lexically normalized, for paths in artifacts.
    given: PathBuf,
    limits: ReadLimits,
    used: u64,
}

/// File identity that must stay the same during a read.
#[derive(PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: rcmake_host::FileTime,
}

impl<'h> ArtifactReader<'h> {
    /// Confine reads to `root`. Return `None` when `root` is not a directory.
    pub fn new(host: &'h dyn Host, root: &Path, limits: ReadLimits) -> Option<Self> {
        let canonical = host.canonicalize(root).ok()?;
        host.metadata(&canonical, Follow::Yes)
            .ok()
            .filter(|meta| meta.kind == FileKind::Dir)?;
        let given = rcmake_host::path::to_native(
            &rcmake_base::path_lex::Lexical::new(&rcmake_host::path::from_native_cmake(root))
                .normalize(),
        );
        Some(Self {
            host,
            root: canonical,
            given,
            limits,
            used: 0,
        })
    }

    /// Canonical root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Host of this reader.
    #[must_use]
    pub fn host(&self) -> &'h dyn Host {
        self.host
    }

    /// Bytes read so far.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.used
    }

    /// Return the checked absolute path of `relative` under the root.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is absolute, holds `..`, is a link, or is not
    /// a regular file under the root.
    pub fn check(&self, relative: &Path) -> Result<PathBuf, ReadError> {
        self.checked(relative).map(|(path, _)| path)
    }

    fn checked(&self, relative: &Path) -> Result<(PathBuf, u64), ReadError> {
        if relative.is_absolute()
            || relative.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err(ReadError::Rejected(
                "The path must stay inside the build tree.",
            ));
        }
        let mut path = self.root.clone();
        let parts: Vec<_> = relative
            .components()
            .filter(|part| matches!(part, std::path::Component::Normal(_)))
            .collect();
        for (index, part) in parts.iter().enumerate() {
            path.push(part);
            let meta = self
                .host
                .metadata(&path, Follow::No)
                .map_err(|_| ReadError::Missing)?;
            let last = index + 1 == parts.len();
            match meta.kind {
                FileKind::Symlink => {
                    return Err(ReadError::Rejected(
                        "Links are not followed in the build tree.",
                    ));
                }
                FileKind::Dir if !last => {}
                FileKind::File if last => {}
                _ if last => return Err(ReadError::Rejected("The path is not a regular file.")),
                _ => return Err(ReadError::Rejected("A path component is not a directory.")),
            }
        }
        let meta = self
            .host
            .metadata(&path, Follow::No)
            .map_err(|_| ReadError::Missing)?;
        if meta.kind != FileKind::File {
            return Err(ReadError::Rejected("The path is not a regular file."));
        }
        let canonical = self
            .host
            .canonicalize(&path)
            .map_err(|_| ReadError::Missing)?;
        if !canonical.starts_with(&self.root) {
            return Err(ReadError::Rejected("The path leaves the build tree."));
        }
        if meta.len > self.limits.max_file_bytes {
            return Err(ReadError::TooLarge);
        }
        Ok((canonical, meta.len))
    }

    /// Read a file under the root within the per-file and cumulative limits.
    ///
    /// # Errors
    ///
    /// Returns an error from [`Self::check`], a read failure, or an exceeded limit.
    pub fn read(&mut self, relative: &Path) -> Result<Vec<u8>, ReadError> {
        self.read_limited(relative, self.limits.max_file_bytes)
    }

    /// Read like [`Self::read`] with a smaller per-file limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::read`].
    pub fn read_limited(&mut self, relative: &Path, limit: u64) -> Result<Vec<u8>, ReadError> {
        let (path, len) = self.checked(relative)?;
        let before = self.identity(&path)?;
        let limit = limit.min(self.limits.max_file_bytes);
        let remaining = self.limits.max_total_bytes.saturating_sub(self.used);
        if len > limit || len > remaining {
            return Err(ReadError::TooLarge);
        }
        let bytes = self
            .host
            .read(
                &path,
                ReadRange::All {
                    limit: limit.min(remaining),
                },
            )
            .map_err(|error| {
                if error.kind == std::io::ErrorKind::NotFound {
                    ReadError::Missing
                } else {
                    ReadError::TooLarge
                }
            })?;
        let len = bytes.len() as u64;
        if len > limit || len > remaining {
            self.used = self.limits.max_total_bytes;
            return Err(ReadError::TooLarge);
        }
        self.used += len;
        let after = self.identity(&path)?;
        if before != after || after.len != len {
            return Err(ReadError::Rejected(
                "The file changed while rcmake read it.",
            ));
        }
        Ok(bytes)
    }

    fn identity(&self, path: &Path) -> Result<Identity, ReadError> {
        let meta = self
            .host
            .metadata(path, Follow::No)
            .map_err(|_| ReadError::Missing)?;
        if meta.kind != FileKind::File {
            return Err(ReadError::Rejected("The path is not a regular file."));
        }
        Ok(Identity {
            dev: meta.dev,
            ino: meta.ino,
            len: meta.len,
            mtime: meta.mtime,
        })
    }

    /// Return the root-relative path of an absolute path that an artifact names,
    /// such as a response file. The path is compared lexically with the canonical
    /// root and with the root as the caller spelled it. Links are not resolved here,
    /// so the read rejects a link in any component under the root.
    #[must_use]
    pub fn artifact_relative(&self, absolute: &Path) -> Option<PathBuf> {
        let normalized = rcmake_host::path::to_native(
            &rcmake_base::path_lex::Lexical::new(&rcmake_host::path::from_native_cmake(absolute))
                .normalize(),
        );
        normalized
            .strip_prefix(&self.root)
            .or_else(|_| normalized.strip_prefix(&self.given))
            .ok()
            .map(Path::to_path_buf)
    }

    /// Read and parse a JSON file with a parse budget sized to the file limit.
    ///
    /// # Errors
    ///
    /// Returns a read error or a JSON error.
    pub fn json(&mut self, relative: &Path) -> Result<Value, ReadError> {
        let bytes = self.read(relative)?;
        let budget = ParseBudget::artifact(
            usize::try_from(self.limits.max_file_bytes).unwrap_or(usize::MAX),
        );
        rcmake_json::parse_bounded(&bytes, &ReadOptions::default(), &budget)
            .map_err(|error| ReadError::Json(error.message))
    }

    /// Return the names of a directory under the root, or [`ReadError::TooLarge`]
    /// when it has more than `max` entries. A caller that needs the whole listing,
    /// for example to find the newest index, must not use a cut listing.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::TooLarge`] for more than `max` entries.
    pub fn list_checked(
        &self,
        relative: &Path,
        max: usize,
    ) -> Result<Vec<(BString, FileKind)>, ReadError> {
        let entries = self.list(relative, max.saturating_add(1));
        if entries.len() > max {
            return Err(ReadError::TooLarge);
        }
        Ok(entries)
    }

    /// Return the names of a directory under the root. Links and missing entries are skipped.
    #[must_use]
    pub fn list(&self, relative: &Path, max: usize) -> Vec<(BString, FileKind)> {
        let path = self.root.join(relative);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Vec::new();
        }
        let canonical_ok = self
            .host
            .canonicalize(&path)
            .is_ok_and(|canonical| canonical.starts_with(&self.root));
        if !canonical_ok {
            return Vec::new();
        }
        self.host
            .read_dir(&path)
            .map(|entries| {
                entries
                    .into_iter()
                    .take(max)
                    .map(|entry| (entry.name, entry.kind))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return the path of `absolute` relative to the root, when it is under the root.
    #[must_use]
    pub fn relative(&self, absolute: &Path) -> Option<PathBuf> {
        let canonical = self.host.canonicalize(absolute).ok()?;
        canonical
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_host::PureHost;

    use super::*;

    fn host() -> Arc<PureHost> {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/b/ok.json"), br#"{"a":1}"#)
            .expect("seed");
        host.seed_file(Path::new("/outside/secret.json"), b"{}")
            .expect("seed");
        host.seed_file(Path::new("/b/big.txt"), &[b'x'; 64])
            .expect("seed");
        host.symlink(Path::new("/outside/secret.json"), Path::new("/b/link.json"))
            .expect("link");
        host
    }

    #[test]
    fn rejects_escapes_links_and_parent_components() {
        let host = host();
        let reader = ArtifactReader::new(host.as_ref(), Path::new("/b"), ReadLimits::default())
            .expect("root");
        assert!(reader.check(Path::new("ok.json")).is_ok());
        for bad in [
            "../outside/secret.json",
            "/outside/secret.json",
            "link.json",
        ] {
            assert!(
                matches!(reader.check(Path::new(bad)), Err(ReadError::Rejected(_))),
                "{bad}"
            );
        }
        assert_eq!(
            reader.check(Path::new("nope.json")),
            Err(ReadError::Missing)
        );
    }

    #[test]
    fn enforces_file_and_cumulative_budgets() {
        let host = host();
        let limits = ReadLimits {
            max_file_bytes: 32,
            max_total_bytes: 10,
        };
        let mut reader = ArtifactReader::new(host.as_ref(), Path::new("/b"), limits).expect("root");
        assert_eq!(reader.read(Path::new("big.txt")), Err(ReadError::TooLarge));
        assert!(reader.read(Path::new("ok.json")).is_ok());
        assert_eq!(reader.read(Path::new("ok.json")), Err(ReadError::TooLarge));
    }

    #[test]
    fn rejects_links_in_any_component_and_resolves_artifact_paths_lexically() {
        let host = host();
        host.seed_file(Path::new("/b/real/rsp.txt"), b"-O2")
            .expect("seed");
        host.symlink(Path::new("/b/real"), Path::new("/b/linked"))
            .expect("link");
        host.symlink(Path::new("/b/real/rsp.txt"), Path::new("/b/real/alias.txt"))
            .expect("link");
        let mut reader = ArtifactReader::new(host.as_ref(), Path::new("/b"), ReadLimits::default())
            .expect("root");
        assert!(reader.read(Path::new("real/rsp.txt")).is_ok());
        for bad in ["linked/rsp.txt", "real/alias.txt"] {
            assert!(
                matches!(reader.read(Path::new(bad)), Err(ReadError::Rejected(_))),
                "{bad}"
            );
        }
        let relative = reader
            .artifact_relative(Path::new("/b/sub/../real/alias.txt"))
            .expect("relative");
        assert_eq!(relative, Path::new("real/alias.txt"));
        assert!(matches!(
            reader.read(&relative),
            Err(ReadError::Rejected(_))
        ));
        assert!(reader.artifact_relative(Path::new("/outside/x")).is_none());
    }

    #[test]
    fn checked_listing_reports_overflow() {
        let host = host();
        let reader = ArtifactReader::new(host.as_ref(), Path::new("/b"), ReadLimits::default())
            .expect("root");
        assert_eq!(
            reader.list_checked(Path::new("."), 1),
            Err(ReadError::TooLarge)
        );
        assert!(reader.list_checked(Path::new("."), 16).is_ok());
    }

    #[test]
    fn json_uses_the_parse_budget() {
        let host = host();
        let deep = "[".repeat(80) + &"]".repeat(80);
        host.seed_file(Path::new("/b/deep.json"), deep.as_bytes())
            .expect("seed");
        let mut reader = ArtifactReader::new(host.as_ref(), Path::new("/b"), ReadLimits::default())
            .expect("root");
        assert!(matches!(
            reader.json(Path::new("deep.json")),
            Err(ReadError::Json(_))
        ));
        assert!(reader.json(Path::new("ok.json")).is_ok());
    }
}
