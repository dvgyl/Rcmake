// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use rcmake_base::{
    BString,
    binfmt::{BinFormat, BinaryDeps, ElfDeps, inspect_binary_deps},
};
use rcmake_host::{FileKind, ReadRange};

use super::{
    INPUTS, Parsed, Resolved, Result, Scan, apply_normalization, error, identity, native, warning,
};

mod tool;

#[cfg(test)]
#[path = "elf_tests.rs"]
mod tests;

pub(super) fn scan(scan: &mut Scan<'_, '_, '_>, args: &Parsed, platform: &[u8]) -> Result<()> {
    let mut elf = ElfScan {
        tools: tool::Tools::new(scan.cx, platform)?,
        images: HashMap::new(),
        machine: 0,
        probes: 0,
        retained: 0,
        work: 0,
    };
    for input in INPUTS.into_iter().flat_map(|key| args.multi(key)) {
        let machine = machine(scan, input).ok_or_else(|| invalid(input))?;
        if elf.machine == 0 {
            elf.machine = machine;
        } else if machine != elf.machine {
            return Err(error(
                [
                    b"ELF machine does not match the other inputs:\n  ",
                    &input[..],
                ]
                .concat(),
            ));
        }
        let root = rcmake_base::path_lex::cmake_style(input);
        elf.walk(scan, args.multi("DIRECTORIES"), &root)?;
    }
    Ok(())
}

struct ElfScan {
    tools: tool::Tools,
    images: HashMap<BString, Arc<ElfDeps>>,
    machine: u16,
    probes: usize,
    retained: usize,
    work: usize,
}

#[derive(Eq, Hash, PartialEq)]
enum FileKey {
    Identity(u64, u64),
    Path(BString),
}

struct Visit {
    path: BString,
    inherited: Arc<[BString]>,
    depth: u32,
}

impl ElfScan {
    fn image(&mut self, scan: &Scan<'_, '_, '_>, path: &[u8]) -> Result<Arc<ElfDeps>> {
        if let Some(image) = self.images.get(path) {
            return Ok(Arc::clone(image));
        }
        let image = if self.tools.objdump.is_some() {
            self.tools.read(scan.cx, path)?
        } else {
            let bytes = scan
                .cx
                .host()
                .read(
                    &native(scan.cx, path),
                    ReadRange::All {
                        limit: scan.cx.limits().max_file_read,
                    },
                )
                .map_err(|_| invalid(path))?;
            match inspect_binary_deps(&bytes) {
                Ok(Some(BinaryDeps::Elf(image))) => image,
                _ => return Err(invalid(path)),
            }
        };
        let fields = image
            .needed
            .iter()
            .chain(&image.rpaths)
            .chain(&image.runpaths)
            .chain(&image.soname);
        self.retain(
            scan,
            path.len()
                .saturating_add(fields.fold(0usize, |bytes, field| {
                    bytes
                        .saturating_add(field.len())
                        .saturating_add(std::mem::size_of::<BString>())
                })),
        )?;
        let image = Arc::new(image);
        self.images.insert(path.into(), Arc::clone(&image));
        Ok(image)
    }

    fn retain(&mut self, scan: &Scan<'_, '_, '_>, bytes: usize) -> Result<()> {
        self.retained = self.retained.saturating_add(bytes);
        if self.retained > scan.cx.limits().max_value_bytes {
            return Err(error("ELF dependency scan exceeds the metadata limit"));
        }
        Ok(())
    }

    fn charge_work(&mut self, scan: &Scan<'_, '_, '_>, bytes: usize) -> Result<()> {
        self.work = self.work.saturating_add(bytes);
        if self.work > scan.cx.limits().max_value_bytes {
            return Err(error("ELF dependency scan exceeds the search size limit"));
        }
        Ok(())
    }

    fn walk(
        &mut self,
        scan: &mut Scan<'_, '_, '_>,
        directories: &[BString],
        root: &BString,
    ) -> Result<()> {
        let mut seen = HashSet::from([file_key(scan, root)]);
        let mut names = HashSet::new();
        let mut queue = VecDeque::from([Visit {
            path: root.clone(),
            inherited: Arc::from([]),
            depth: 0,
        }]);
        while let Some(visit) = queue.pop_front() {
            let image = self.image(scan, &visit.path)?;
            let origin = visit
                .path
                .iter()
                .rposition(|&byte| byte == b'/')
                .and_then(|end| visit.path.get(..end.max(1)))
                .unwrap_or(b".");
            for field in image.rpaths.iter().chain(&image.runpaths) {
                let expansion = if field.windows(7).any(|part| part == b"$ORIGIN")
                    || field.windows(9).any(|part| part == b"${ORIGIN}")
                {
                    origin.len()
                } else {
                    0
                };
                self.charge_work(
                    scan,
                    field
                        .len()
                        .saturating_add(expansion)
                        .saturating_add(std::mem::size_of::<BString>()),
                )?;
            }
            let rpaths = expand_paths(&image.rpaths, origin);
            let search = expand_paths(&image.runpaths, origin);
            let search = if search.is_empty() {
                rpaths
                    .iter()
                    .chain(visit.inherited.iter())
                    .collect::<Vec<_>>()
            } else {
                search.iter().collect()
            };
            let inherited = if rpaths.is_empty() {
                Arc::clone(&visit.inherited)
            } else {
                let bytes = visit
                    .inherited
                    .iter()
                    .chain(&rpaths)
                    .fold(0usize, |size, path| {
                        size.saturating_add(path.len())
                            .saturating_add(std::mem::size_of::<BString>())
                    });
                self.charge_work(scan, bytes)?;
                visit
                    .inherited
                    .iter()
                    .cloned()
                    .chain(rpaths.iter().cloned())
                    .collect::<Arc<[_]>>()
            };
            for name in &image.needed {
                self.charge_work(scan, name.len().saturating_add(1))?;
                if names.contains(name) || scan.filters.pre_excluded(name)? {
                    continue;
                }
                if name.contains(&b'/') {
                    return Err(error(
                        [b"Paths to dependencies are not supported: ", &name[..]].concat(),
                    ));
                }
                let Some(path) = self.resolve(scan, name, &search, directories)? else {
                    if scan.unresolved.insert(name.clone()) {
                        self.retain(
                            scan,
                            name.len().saturating_add(std::mem::size_of::<BString>()),
                        )?;
                    }
                    continue;
                };
                names.insert(name.clone());
                if scan.filters.post_excluded(scan.cx, &path)? {
                    continue;
                }
                self.retain(
                    scan,
                    path.len().saturating_add(name.len()).saturating_add(128),
                )?;
                scan.resolved.entry(name.clone()).or_default().insert(
                    path.clone(),
                    Resolved {
                        file: identity(scan.cx, &path),
                        rpaths: None,
                    },
                );
                if seen.insert(file_key(scan, &path)) {
                    if visit.depth >= scan.cx.limits().max_recursion.min(256) {
                        return Err(error("Runtime dependency scan exceeds the depth limit"));
                    }
                    queue.push_back(Visit {
                        path: rcmake_base::path_lex::cmake_style(&path),
                        inherited: Arc::clone(&inherited),
                        depth: visit.depth + 1,
                    });
                }
            }
        }
        Ok(())
    }

    fn resolve(
        &mut self,
        scan: &mut Scan<'_, '_, '_>,
        name: &[u8],
        search: &[&BString],
        directories: &[BString],
    ) -> Result<Option<BString>> {
        // Windows cannot open a filename with a literal POSIX backslash.
        if cfg!(windows) && name.contains(&b'\\') {
            return Ok(None);
        }
        let system = self.tools.system_dirs(self.machine);
        let primary = search
            .iter()
            .copied()
            .chain(system.iter())
            .map(|dir| (dir, false));
        for (dir, fallback) in primary.chain(directories.iter().map(|dir| (dir, true))) {
            self.probes += 1;
            if self.probes > 100_000 {
                return Err(error("ELF dependency scan exceeds the candidate limit"));
            }
            self.charge_work(scan, dir.len().saturating_add(name.len()).saturating_add(1))?;
            if cfg!(windows) && !fallback && dir.contains(&b'\\') {
                continue;
            }
            let path: BString = [dir.as_slice(), b"/", name].concat().into();
            if machine(scan, &path) != Some(self.machine) {
                continue;
            }
            if fallback {
                warning(
                    scan.cx,
                    [
                        b"Dependency ".as_slice(),
                        name,
                        b" found in search directory:\n  ",
                        dir,
                        b"\nSee file(GET_RUNTIME_DEPENDENCIES) documentation for more information.",
                    ]
                    .concat(),
                );
                return Ok(Some(path));
            }
            let normalized = normalize_posix(&path);
            return Ok(Some(apply_normalization(scan.cx, path, normalized)));
        }
        Ok(None)
    }
}

fn normalize_posix(path: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::new();
    if path.starts_with(b"/") {
        normalized.push(b'/');
    }
    for part in path
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty())
    {
        if !normalized.is_empty() && !normalized.ends_with(b"/") {
            normalized.push(b'/');
        }
        normalized.extend_from_slice(part);
    }
    normalized
}

fn invalid(path: &[u8]) -> rcmake_eval::CmdError {
    error(
        [
            b"Could not read ELF dependencies from:\n  ".as_slice(),
            path,
        ]
        .concat(),
    )
}

fn file_key(scan: &Scan<'_, '_, '_>, path: &[u8]) -> FileKey {
    if let Some((dev, ino)) = identity(scan.cx, path) {
        return FileKey::Identity(dev, ino);
    }
    let path = native(scan.cx, path);
    let path = scan.cx.host().canonicalize(&path).unwrap_or(path);
    FileKey::Path(rcmake_host::path::from_native_cmake(&path))
}

fn machine(scan: &Scan<'_, '_, '_>, path: &[u8]) -> Option<u16> {
    let host = scan.cx.host();
    let path = native(scan.cx, path);
    if host.metadata(&path, super::Follow::Yes).ok()?.kind != FileKind::File {
        return None;
    }
    let bytes = host
        .read(
            &path,
            ReadRange::Slice {
                offset: 0,
                len: Some(20),
            },
        )
        .ok()?;
    header(&bytes).map(|(machine, _)| machine)
}

fn header(bytes: &[u8]) -> Option<(u16, BinFormat)> {
    let [0x7f, b'E', b'L', b'F', class, endian, ..] = bytes else {
        return None;
    };
    let machine = bytes.get(18..20)?.try_into().ok()?;
    let machine = match endian {
        1 => u16::from_le_bytes(machine),
        2 => u16::from_be_bytes(machine),
        _ => return None,
    };
    let format = match class {
        1 => BinFormat::Elf32,
        2 => BinFormat::Elf64,
        _ => return None,
    };
    Some((machine, format))
}

fn expand_paths(paths: &[BString], origin: &[u8]) -> Vec<BString> {
    if paths.is_empty() || matches!(paths, [only] if only.is_empty()) {
        return Vec::new();
    }
    // CMake starts the next separator search one byte after the new field starts.
    let raw = paths
        .iter()
        .map(|path| path.as_slice())
        .collect::<Vec<_>>()
        .join(&b':');
    let mut remaining = raw.as_slice();
    let mut skip = 0;
    let mut output = Vec::new();
    while let Some(end) = remaining
        .iter()
        .enumerate()
        .skip(skip)
        .find_map(|(i, &byte)| (byte == b':').then_some(i))
    {
        let (field, suffix) = remaining.split_at(end);
        output.push(expand_origin(field, origin));
        remaining = suffix.strip_prefix(b":").unwrap_or_default();
        skip = 1;
    }
    output.push(expand_origin(remaining, origin));
    output
}

fn expand_origin(path: &[u8], origin: &[u8]) -> BString {
    for token in [b"$ORIGIN".as_slice(), b"${ORIGIN}"] {
        if let Some(start) = path
            .windows(token.len())
            .enumerate()
            .find_map(|(start, part)| {
                let end = start + token.len();
                (part == token
                    && (token.ends_with(b"}")
                        || path
                            .get(end)
                            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')))
                .then_some(start)
            })
        {
            let (prefix, rest) = path.split_at(start);
            if let Some(suffix) = rest.strip_prefix(token) {
                return [prefix, origin, suffix].concat().into();
            }
        }
    }
    path.into()
}
