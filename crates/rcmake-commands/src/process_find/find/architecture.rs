use std::path::{Path, PathBuf};

use rcmake_base::{BString, ByteSlice as _, truth};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{FileKind, Follow, Host, path};

pub(super) fn rewrite(cx: &mut Cx<'_>, dirs: Vec<PathBuf>) -> Result<Vec<PathBuf>, CmdError> {
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
        return Ok(dirs);
    };
    let mut state = Rewrite {
        host: cx.host(),
        suffix: &suffix,
        remaining: cx.limits().max_value_bytes.min(16 * 1024 * 1024),
        depth: cx.limits().max_recursion.min(64),
        paths: Vec::new(),
    };
    for dir in dirs {
        let mut text = path::from_native_cmake(&dir);
        if !text.ends_with(b"/") {
            text.push(b'/');
        }
        state.visit(&text, 0, true, 0)?;
    }
    Ok(state.paths)
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
        if !self.suffix.is_empty()
            && let Some(index) = dir.get(start..).and_then(|tail| tail.find(b"lib/"))
        {
            let split = start + index + 3;
            let (prefix, tail) = dir.split_at_checked(split).ok_or_else(error)?;
            let plain = path::to_native(prefix);
            let prefix: BString = [prefix, self.suffix].concat().into();
            let alternative = path::to_native(&prefix);
            let use_plain = self.is_dir(&plain);
            if self.is_dir(&alternative) && !(use_plain && self.linked(&plain, &alternative)) {
                let replaced = [&prefix, tail].concat();
                self.visit(&replaced, prefix.len() + 1, true, depth + 1)?;
            }
            if use_plain {
                self.visit(dir, split + 1, false, depth + 1)?;
            }
        }
        if fresh {
            let plain = path::to_native(dir);
            let use_plain = self.is_dir(&plain);
            if !self.suffix.is_empty() {
                let alternate = path::to_native(&[dir, self.suffix].concat());
                if self.is_dir(&alternate) && !(use_plain && self.linked(&plain, &alternate)) {
                    self.paths.push(alternate);
                }
            }
            if use_plain {
                self.paths.push(plain);
            }
        }
        Ok(())
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
