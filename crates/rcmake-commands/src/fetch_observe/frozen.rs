// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rcmake_eval::{CmdError, Cx, FetchLockMode};
use rcmake_fetch::{ContentHash, GitMirror, UrlEntry, atomic_write};
use rcmake_host::{
    FileKind, Follow, Metadata, ReadRange,
    path::{from_native_cmake, to_native},
};
use rcmake_store::Digest;

use super::runtime::{Population, Runtime, required, stop};
use super::{BlockReason, DeclaredSource, archive, git, portable_source};

const RECEIPT_LIMIT: u64 = 32 * 1024;
const HEADER_SIZE: usize = 1 + 4 * 32;

pub(super) enum Frozen {
    Archive(Box<Archive>),
    Git {
        pin: String,
        executable: rcmake_base::BString,
        checked: bool,
    },
}

pub(super) struct Archive {
    pin: UrlEntry,
    expected: Option<ContentHash>,
    no_extract: bool,
    file: Option<PathBuf>,
    inner: Option<PathBuf>,
    download: Option<Download>,
}

struct Download {
    stamp: PathBuf,
    wrapper: Digest,
    previous: Option<(Metadata, Metadata)>,
}

impl Frozen {
    pub(super) fn select(
        cx: &mut Cx<'_>,
        runtime: &Arc<Runtime>,
        pop: &Population,
    ) -> Result<Option<Self>, CmdError> {
        let (mode, declaration, credentials) = runtime.with_fetch(pop, |fetch| {
            (
                fetch.mode,
                fetch.get(&pop.dependency).cloned(),
                fetch.credentials.contains(&pop.dependency),
            )
        });
        if mode != FetchLockMode::Frozen {
            return Ok(None);
        }
        if credentials {
            required(cx, runtime, pop, BlockReason::Unsupported)?;
        }
        let Some(declaration) = declaration else {
            return Ok(None);
        };
        match declaration.source {
            DeclaredSource::Archive { ref urls, ref hash } => {
                let local_dir = match urls.as_slice() {
                    [url] => archive::local_url_path(url).is_some_and(|path| {
                        is_kind(cx, &cx.resolve_path(path.as_bytes()), FileKind::Dir)
                    }),
                    _ => false,
                };
                if local_dir && declaration.supported() {
                    return Ok(None);
                }
                if !declaration.supported()
                    || pop.subbuild
                    || !urls.iter().all(|url| portable_source(url))
                {
                    required(cx, runtime, pop, BlockReason::Unsupported)?;
                }
                let expected = hash
                    .as_deref()
                    .map(ContentHash::parse)
                    .transpose()
                    .map_err(|error| stop(cx, error.to_string()))?;
                let pin = archive::frozen_pin(
                    cx,
                    Arc::clone(runtime),
                    pop,
                    urls.clone(),
                    expected.clone(),
                )?
                .ok_or_else(|| failure(cx, pop, "The archive has no lock entry."))?;
                Ok(Some(Self::Archive(Box::new(Archive {
                    pin,
                    expected,
                    no_extract: declaration.no_extract,
                    file: None,
                    inner: None,
                    download: None,
                }))))
            }
            DeclaredSource::Git { .. } => {
                if pop.subbuild || !declaration.supported() {
                    required(cx, runtime, pop, BlockReason::Unsupported)?;
                }
                let pin = git::frozen_pin(cx, Arc::clone(runtime), pop.clone())?
                    .ok_or_else(|| failure(cx, pop, "The Git declaration has no valid pin."))?;
                Ok(Some(Self::Git {
                    pin,
                    executable: Default::default(),
                    checked: false,
                }))
            }
            DeclaredSource::Other => Ok(None),
        }
    }

    pub(super) fn remote_archive(
        &mut self,
        cx: &mut Cx<'_>,
        pop: &Population,
        script: PathBuf,
        file: PathBuf,
    ) -> Result<(), CmdError> {
        if let Self::Archive(archive) = self {
            let directory = to_native(&cx.var_or_empty(b"_EP_DOWNLOAD_DIR"));
            if !contained(&file, &directory) {
                return Err(failure(
                    cx,
                    pop,
                    "The archive path is outside the download directory.",
                ));
            }
            archive.file = Some(file);
            archive.inner = Some(script);
        }
        Ok(())
    }

    pub(super) fn local_archive(&mut self, cx: &Cx<'_>) {
        if let Self::Archive(archive) = self {
            let value = cx.var_or_empty(b"_EP_URL");
            archive.file = super::text(&value)
                .and_then(archive::local_url_path)
                .filter(|path| !path.is_empty() && !path.contains(';'))
                .map(|path| cx.resolve_path(path.as_bytes()));
        }
    }

    pub(super) fn git_executable(&mut self, program: &[u8]) {
        if let Self::Git { executable, .. } = self {
            *executable = program.into();
        }
    }

    pub(super) fn ran_update(&mut self) {
        if let Self::Git { checked, .. } = self {
            *checked = false;
        }
    }

    pub(super) fn step(
        &mut self,
        cx: &mut Cx<'_>,
        runtime: &Runtime,
        pop: &Population,
        stamp: &Path,
        wrapper: Digest,
    ) -> Result<(), CmdError> {
        match (stamp.file_name().and_then(|name| name.to_str()), self) {
            (Some("download.stamp"), Self::Archive(archive)) => {
                if let Some(inner) = &archive.inner
                    && runtime
                        .generated_digest(pop, inner, super::runtime::Script::Archive)
                        .is_none()
                {
                    return Err(failure(
                        cx,
                        pop,
                        "Could not verify the generated archive download script.",
                    ));
                }
                archive.before_download(cx, pop, stamp, wrapper)
            }
            (Some("update.stamp"), Self::Archive(archive)) => archive.after_download(cx, pop),
            (
                Some("download.stamp"),
                Self::Git {
                    pin,
                    executable,
                    checked,
                },
            ) if is_kind(cx, stamp, FileKind::File) => {
                check_git(cx, pop, pin, executable)?;
                *checked = true;
                Ok(())
            }
            (
                Some("patch.stamp"),
                Self::Git {
                    pin,
                    executable,
                    checked: false,
                },
            ) => check_git(cx, pop, pin, executable),
            _ => Ok(()),
        }
    }

    pub(super) fn disconnected(
        &self,
        cx: &mut Cx<'_>,
        runtime: &Runtime,
        pop: &Population,
    ) -> Result<(), CmdError> {
        match self {
            Self::Git { pin, .. } => check_git(cx, pop, pin, b"git"),
            Self::Archive(archive) => {
                let binary = to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR"));
                let base = to_native(&cx.var_or_empty(b"FETCHCONTENT_BASE_DIR"));
                let script_mode = cx.mode() == rcmake_eval::RunMode::Script;
                let (stamp_dir, script_dir) = if script_mode {
                    (
                        base.join(format!("{}-stamp", pop.dependency)),
                        base.join(format!("{}-tmp", pop.dependency)),
                    )
                } else {
                    (
                        binary.join("CMakeFiles/fc-stamp").join(&pop.dependency),
                        binary.join("CMakeFiles/fc-tmp").join(&pop.dependency),
                    )
                };
                let stamp = stamp_dir.join("download.stamp");
                let proof = read_receipt(cx, &stamp);
                let Some(bytes) = proof.as_deref() else {
                    return Err(failure(cx, pop, "The archive receipt is missing."));
                };
                let Some(file) = bytes
                    .get(HEADER_SIZE..)
                    .filter(|path| !path.is_empty())
                    .map(to_native)
                else {
                    return Err(failure(cx, pop, "The archive receipt is incomplete."));
                };
                let local = archive
                    .pin
                    .urls
                    .iter()
                    .filter_map(|url| archive::local_url_path(url))
                    .any(|path| cx.resolve_path(path.as_bytes()) == file);
                let downloads = base.join(format!("{}-tmp", pop.dependency));
                if !local && !contained(&file, &downloads) {
                    return Err(failure(cx, pop, "The receipt has an invalid archive path."));
                }
                let wrapper = runtime.script_digest(&script_dir.join("download.cmake"));
                let expected =
                    wrapper.and_then(|wrapper| archive.receipt(cx, pop, &file, &stamp, wrapper));
                if expected.as_deref() != Some(bytes)
                    || archive.verified(cx, &file).is_none()
                    || !is_kind(cx, &pop.source, FileKind::Dir)
                    || !archive.verify_copy(cx, pop, &file)
                {
                    return Err(failure(
                        cx,
                        pop,
                        "The archive receipt or retained content differs from the lock.",
                    ));
                }
                Ok(())
            }
        }
    }
}

impl Archive {
    fn before_download(
        &mut self,
        cx: &mut Cx<'_>,
        pop: &Population,
        stamp: &Path,
        wrapper: Digest,
    ) -> Result<(), CmdError> {
        let Some(file) = self.file.as_ref() else {
            return Err(failure(
                cx,
                pop,
                "Could not identify the archive used by the download step.",
            ));
        };
        let warm_stamp = metadata(cx, stamp);
        let mut previous = None;
        if let Some(stamp_meta) = warm_stamp {
            let receipt = read_receipt(cx, stamp);
            let verified = self.verified(cx, file);
            if metadata(cx, file).is_some() && verified.is_none() {
                return Err(failure(
                    cx,
                    pop,
                    "The retained archive differs from the lock.",
                ));
            }
            let current = self.receipt(cx, pop, file, stamp, wrapper);
            if let Some(archive_meta) = verified
                && receipt.is_some()
                && receipt == current
                && is_kind(cx, &pop.source, FileKind::Dir)
            {
                previous = Some((stamp_meta, archive_meta));
            } else {
                clear_stamps(cx, pop, stamp)?;
            }
        } else if self
            .expected
            .as_ref()
            .is_some_and(|hash| hash.verify_file(cx.host(), file).is_ok())
            && self.verified(cx, file).is_none()
        {
            return Err(failure(
                cx,
                pop,
                "The retained archive matches the declared hash but differs from the lock.",
            ));
        }
        self.download = Some(Download {
            stamp: stamp.to_owned(),
            wrapper,
            previous,
        });
        Ok(())
    }

    fn after_download(&self, cx: &mut Cx<'_>, pop: &Population) -> Result<(), CmdError> {
        let (Some(file), Some(step)) = (&self.file, &self.download) else {
            return Err(failure(
                cx,
                pop,
                "The download step has no verification state.",
            ));
        };
        let same = step.previous.as_ref().is_some_and(|(stamp, archive)| {
            metadata(cx, &step.stamp)
                .is_some_and(|meta| file_fingerprint(&meta) == file_fingerprint(stamp))
                && metadata(cx, file)
                    .is_some_and(|meta| file_fingerprint(&meta) == file_fingerprint(archive))
        });
        if (!same && self.verified(cx, file).is_none())
            || !is_kind(cx, &pop.source, FileKind::Dir)
            || !self.verify_copy(cx, pop, file)
        {
            return Err(failure(
                cx,
                pop,
                "The download step did not produce the locked archive and source directory.",
            ));
        }
        if !same {
            let bytes = self
                .receipt(cx, pop, file, &step.stamp, step.wrapper)
                .ok_or_else(|| failure(cx, pop, "Could not create the download receipt."))?;
            atomic_write(cx.host(), &receipt_path(&step.stamp), &bytes).map_err(|error| {
                failure(
                    cx,
                    pop,
                    &format!("Could not write the download receipt. {error}"),
                )
            })?;
        }
        Ok(())
    }

    fn verify_copy(&self, cx: &Cx<'_>, pop: &Population, file: &Path) -> bool {
        !self.no_extract
            || file
                .file_name()
                .is_some_and(|name| self.verified(cx, &pop.source.join(name)).is_some())
    }

    fn verified(&self, cx: &Cx<'_>, file: &Path) -> Option<Metadata> {
        let meta = metadata(cx, file)?;
        if meta.len != self.pin.size {
            return None;
        }
        ContentHash::sha256(self.pin.sha256)
            .verify_file(cx.host(), file)
            .ok()?;
        if let Some(expected) = &self.expected
            && *expected != ContentHash::sha256(self.pin.sha256)
        {
            expected.verify_file(cx.host(), file).ok()?;
        }
        Some(meta)
    }

    fn pin_digest(&self) -> Digest {
        let mut data = self.pin.size.to_le_bytes().to_vec();
        data.extend_from_slice(self.pin.sha256.as_bytes());
        for url in &self.pin.urls {
            data.extend_from_slice(Digest::hash(url.as_bytes()).as_bytes());
        }
        Digest::hash(&data)
    }

    fn receipt(
        &self,
        cx: &Cx<'_>,
        pop: &Population,
        file: &Path,
        stamp: &Path,
        wrapper: Digest,
    ) -> Option<Vec<u8>> {
        let meta = metadata(cx, stamp)?;
        let file = from_native_cmake(file);
        if file.len() > RECEIPT_LIMIT as usize - HEADER_SIZE {
            return None;
        }
        let identity =
            receipt_identity(&pop.dependency, self.no_extract, &pop.source, &file, stamp);
        let stamp: Vec<_> = file_fingerprint(&meta)
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect();
        let mut bytes = vec![1];
        for digest in [self.pin_digest(), identity, wrapper, Digest::hash(&stamp)] {
            bytes.extend_from_slice(digest.as_bytes());
        }
        bytes.extend_from_slice(&file);
        Some(bytes)
    }
}

fn receipt_identity(
    dependency: &str,
    no_extract: bool,
    source: &Path,
    archive: &[u8],
    stamp: &Path,
) -> Digest {
    let mut identity = vec![u8::from(no_extract)];
    for value in [
        dependency.as_bytes(),
        &from_native_cmake(source),
        archive,
        &from_native_cmake(stamp),
    ] {
        identity.extend_from_slice(Digest::hash(value).as_bytes());
    }
    Digest::hash(&identity)
}

fn file_fingerprint(meta: &Metadata) -> [u64; 5] {
    [
        meta.len,
        meta.dev,
        meta.ino,
        meta.mtime.secs.cast_unsigned(),
        u64::from(meta.mtime.nanos),
    ]
}

fn metadata(cx: &Cx<'_>, path: &Path) -> Option<Metadata> {
    cx.host()
        .metadata(path, Follow::Yes)
        .ok()
        .filter(|meta| meta.kind == FileKind::File)
}

fn is_kind(cx: &Cx<'_>, path: &Path, kind: FileKind) -> bool {
    cx.host()
        .metadata(path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == kind)
}

fn contained(file: &Path, directory: &Path) -> bool {
    directory.is_absolute()
        && file.strip_prefix(directory).is_ok_and(|sub| {
            sub.components().next().is_some()
                && sub
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
        })
}

fn receipt_path(stamp: &Path) -> PathBuf {
    stamp.with_added_extension("rcmake")
}

fn read_receipt(cx: &Cx<'_>, stamp: &Path) -> Option<Vec<u8>> {
    cx.host()
        .read(
            &receipt_path(stamp),
            ReadRange::All {
                limit: RECEIPT_LIMIT,
            },
        )
        .ok()
}

fn clear_stamps(cx: &mut Cx<'_>, pop: &Population, stamp: &Path) -> Result<(), CmdError> {
    for name in [
        "download.stamp",
        "update.stamp",
        "patch.stamp",
        "download.stamp.rcmake",
    ] {
        if let Err(error) = cx.host().remove_file(&stamp.with_file_name(name))
            && error.kind != ErrorKind::NotFound
        {
            return Err(failure(
                cx,
                pop,
                &format!("Could not remove a stale population stamp. {error}"),
            ));
        }
    }
    Ok(())
}

fn check_git(cx: &mut Cx<'_>, pop: &Population, pin: &str, program: &[u8]) -> Result<(), CmdError> {
    if program.is_empty() {
        return Err(failure(cx, pop, "Could not identify the Git executable."));
    }
    let result = GitMirror::new(cx.host(), &cx.host().initial_cwd()).and_then(|git| {
        git.with_program(program.into())
            .with_timeout(Duration::from_secs(30))
            .checkout_head(&pop.source)
    });
    if !result.is_ok_and(|head| head == pin) {
        return Err(failure(
            cx,
            pop,
            "The Git checkout does not match the locked commit.",
        ));
    }
    Ok(())
}

fn failure(cx: &mut Cx<'_>, pop: &Population, reason: &str) -> CmdError {
    stop(
        cx,
        format!(
            "FetchContent dependency {:?}. {reason} Restore the locked content or run --fetch-lock=update if the change is intentional.",
            pop.dependency
        ),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn receipt_identity_uses_cmake_path_separators() {
        let identity = |source, archive, stamp| {
            receipt_identity(
                "archive",
                true,
                Path::new(source),
                &from_native_cmake(Path::new(archive)),
                Path::new(stamp),
            )
        };
        assert_eq!(
            identity(
                "C:/work\\src",
                "C:/work/tmp\\archive.zip",
                "C:/work\\stamps/download.stamp"
            ),
            identity(
                "C:/work/src",
                "C:/work/tmp/archive.zip",
                "C:/work/stamps/download.stamp"
            )
        );
    }
}
