use std::path::{Path, PathBuf};

use rcmake_base::{BString, ByteSlice as _, truth};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{FileKind, Follow, Host, path};

pub(super) fn rewrite(
    cx: &mut Cx<'_>,
    dirs: Vec<PathBuf>,
) -> Result<(Vec<PathBuf>, bool), CmdError> {
    let suffix = cx.var(b"CMAKE_FIND_LIBRARY_CUSTOM_LIB_SUFFIX").or_else(|| {
        let size = cx.var_or_empty(b"CMAKE_SIZEOF_VOID_P");
        let x32 = super::super::is_elf_x32(cx);
        let props = &cx.global().props;
        [
            (
                size == b"4",
                b"FIND_LIBRARY_USE_LIB32_PATHS".as_slice(),
                "32",
            ),
            (size == b"8", b"FIND_LIBRARY_USE_LIB64_PATHS", "64"),
            (x32, b"FIND_LIBRARY_USE_LIBX32_PATHS", "x32"),
        ]
        .into_iter()
        .find(|(enabled, key, _)| {
            *enabled && props.get(key).is_some_and(|value| truth::is_on(value))
        })
        .map(|(_, _, suffix)| suffix.into())
    });
    let Some(suffix) = suffix else {
        return Ok((dirs, false));
    };
    let mut state = Rewrite {
        host: cx.host(),
        suffix: &suffix,
        remaining: cx.limits().max_value_bytes.min(16 * 1024 * 1024),
        depth: cx.limits().max_recursion.min(64),
        paths: Vec::with_capacity(dirs.len().min(4096)),
    };
    for dir in dirs {
        let mut text = path::from_native_cmake(&dir);
        if !text.ends_with(b"/") {
            text.push(b'/');
        }
        state.visit(&text, 0, true, 0)?;
    }
    Ok((state.paths, true))
}

fn probe_dir(bytes: &[u8]) -> PathBuf {
    path::to_native(super::strip_trailing_slashes(bytes))
}

struct Rewrite<'a> {
    host: &'a dyn Host,
    suffix: &'a [u8],
    remaining: usize,
    depth: u32,
    paths: Vec<PathBuf>,
}

impl Rewrite<'_> {
    fn visit(&mut self, dir: &[u8], start: usize, fresh: bool, depth: u32) -> Result<(), CmdError> {
        let error = || CmdError::status("Library search path expansion exceeded its limit.");
        let cost = dir
            .len()
            .saturating_add(self.suffix.len())
            .saturating_add(64);
        self.remaining = self.remaining.checked_sub(cost).ok_or_else(error)?;
        if depth > self.depth {
            return Err(error());
        }
        let mut known_plain_dir = None;
        if !self.suffix.is_empty()
            && let Some(index) = dir.get(start..).and_then(|tail| tail.find(b"lib/"))
        {
            let split = start + index + 3;
            let (prefix, tail) = dir.split_at_checked(split).ok_or_else(error)?;
            let plain = probe_dir(prefix);
            let prefix: BString = [prefix, self.suffix].concat().into();
            let alternative = probe_dir(&prefix);
            let use_plain = self.is_dir(&plain);
            if tail == b"/" {
                known_plain_dir = Some(use_plain);
            }
            if self.is_dir(&alternative) && !(use_plain && self.linked(&plain, &alternative)) {
                let replaced = [&prefix, tail].concat();
                self.visit(&replaced, prefix.len() + 1, true, depth + 1)?;
            }
            if use_plain && tail != b"/" {
                self.visit(dir, split + 1, false, depth + 1)?;
            }
        }
        if fresh {
            let plain = probe_dir(dir);
            let use_plain = known_plain_dir.unwrap_or_else(|| self.is_dir(&plain));
            if use_plain {
                if !self.suffix.is_empty() {
                    let alternate_bytes = [dir, self.suffix].concat();
                    let alternate = probe_dir(&alternate_bytes);
                    if self.is_dir(&alternate) && !self.linked(&plain, &alternate) {
                        let resolved = self.finalize_dir(alternate);
                        self.paths.push(resolved);
                    }
                }
                let resolved = self.finalize_dir(plain);
                self.paths.push(resolved);
            }
        }
        Ok(())
    }

    fn finalize_dir(&self, probe: PathBuf) -> PathBuf {
        let normalized = super::normalize_owned(probe.clone());
        if normalized == probe {
            return normalized;
        }
        if self
            .host
            .canonicalize(&probe)
            .is_ok_and(|raw| self.host.canonicalize(&normalized).as_ref() == Ok(&raw))
        {
            normalized
        } else {
            probe
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.host
            .metadata(path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
    }

    fn linked(&self, a: &Path, b: &Path) -> bool {
        let symlink = |path| {
            self.host
                .metadata(path, Follow::No)
                .is_ok_and(|meta| meta.kind == FileKind::Symlink)
        };
        (symlink(a) || symlink(b))
            && self
                .host
                .canonicalize(a)
                .is_ok_and(|a| self.host.canonicalize(b).as_ref() == Ok(&a))
    }
}
