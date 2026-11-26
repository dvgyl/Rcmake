//! User-supplied setup inputs of configure replay.

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host};
use rcmake_model::{Cache, CacheEntry};

/// The user-supplied setup inputs of one configure attempt.
///
/// The command-line driver attaches this value with `Interp::add_session_data`
/// before the configure starts. Replay compares the inputs with the inputs of the
/// previous successful configure. A difference disables all replay hits for the
/// attempt. Replay then records again, so the next configure can use the record.
///
/// Example: a changed preset environment or a new `PreLoad.cmake` file disables
/// replay hits for one configure.
#[derive(Clone, Debug, Default)]
pub struct ReplaySetupInputs {
    pub(super) fields: Vec<(String, BString)>,
    pub(super) files: Vec<(String, PathBuf)>,
    pub(super) loaded_cache: Option<Vec<(BString, CacheEntry)>>,
    pub(super) edits: Vec<(BString, Option<BString>)>,
}

impl ReplaySetupInputs {
    /// Create an empty set of inputs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one value, for example the generator name. `label` names the input in
    /// the replay report.
    pub fn field(&mut self, label: &str, value: impl AsRef<[u8]>) {
        self.fields.push((label.to_owned(), value.as_ref().into()));
    }

    /// Add one setup file, for example a `-C` script. Replay compares the lexical
    /// path and the resolved path of the file, so a retargeted symbolic link or a
    /// file that shadows another file in a search path disables replay hits. The
    /// file content is only reported: setup files run live in each configure.
    pub fn file(&mut self, label: &str, path: impl Into<PathBuf>) {
        self.files.push((label.to_owned(), path.into()));
    }

    /// Record one effective command-line or preset cache edit: a definition with
    /// its value, or a removal (`None`). Later edits of the same name replace
    /// earlier ones.
    ///
    /// When the previous configure applied the same edit, replay compares the
    /// entry by the edit and not by its value after setup. Example: each configure
    /// gets `-DCMAKE_TOOLCHAIN_FILE=tc.cmake`, but only a cold configure replaces
    /// the value with the absolute path.
    pub fn edit(&mut self, name: &[u8], value: Option<&[u8]>) {
        self.edits.retain(|(known, _)| known.as_slice() != name);
        self.edits.push((name.into(), value.map(Into::into)));
    }

    /// Record the cache that the configure loaded from `CMakeCache.txt`, before
    /// any command-line or preset edit. Replay checks the cache entries that
    /// differ from this state when the setup ends.
    pub fn loaded_cache(&mut self, cache: &Cache) {
        self.loaded_cache = Some(
            cache
                .iter()
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect(),
        );
    }
}

/// Resolve a toolchain selection the way `CMakeDetermineSystem.cmake` does: an
/// absolute path as given, a relative path first in the binary directory and then
/// in the source directory.
#[must_use]
pub fn resolve_setup_file(host: &dyn Host, value: &Path, source: &Path, binary: &Path) -> PathBuf {
    if value.is_absolute() {
        return value.to_path_buf();
    }
    let in_binary = binary.join(value);
    if host
        .metadata(&in_binary, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        return in_binary;
    }
    source.join(value)
}
