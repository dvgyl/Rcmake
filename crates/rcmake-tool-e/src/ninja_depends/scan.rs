use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, Limits};
use rcmake_fortran::{FortranSourceInfo, IncludeResolver, ScanInput, SourceForm, scan};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, path};

pub(crate) struct ScanSettings<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) include_dirs: &'a [PathBuf],
    pub(crate) form: Option<SourceForm>,
    pub(crate) defines: &'a [BString],
    pub(crate) building_intrinsic_modules: bool,
}

pub(crate) fn scan_source(
    host: &dyn Host,
    file: &Path,
    settings: &ScanSettings<'_>,
    bytes_left: &mut u64,
) -> HostResult<FortranSourceInfo> {
    let limits = Limits {
        max_file_read: *bytes_left,
        ..Limits::default()
    };
    let mut resolver = Resolver {
        host,
        settings,
        files: BTreeMap::new(),
        line_files: BTreeSet::new(),
        bytes_left,
        error: None,
    };
    let (identity, text) = resolver.read_file(file)?;
    let form = settings
        .form
        .or_else(|| SourceForm::from_path(&path::from_native_cmake(file)))
        .unwrap_or(SourceForm::Free);
    let result = scan(
        &ScanInput {
            defines: settings.defines,
            building_intrinsic_modules: settings.building_intrinsic_modules,
            ..ScanInput::new(&identity, &text, form)
        },
        Some(&mut resolver),
        &limits,
    );
    if let Some(error) = resolver.error {
        return Err(error);
    }
    let mut result = result.map_err(|error| HostError::unsupported(error.to_string()))?;
    result.resolved_includes = result
        .resolved_includes
        .iter()
        .filter_map(|identity| resolver.files.get(identity))
        .map(|file| path::from_native_cmake(file))
        .chain(resolver.line_files)
        .collect();
    Ok(result)
}

fn is_file(host: &dyn Host, file: &Path) -> bool {
    host.metadata(file, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
}

struct Resolver<'a, 'settings> {
    host: &'a dyn Host,
    settings: &'settings ScanSettings<'settings>,
    files: BTreeMap<BString, PathBuf>,
    line_files: BTreeSet<BString>,
    bytes_left: &'a mut u64,
    error: Option<HostError>,
}

impl Resolver<'_, '_> {
    fn read_file(&mut self, file: &Path) -> HostResult<(BString, BString)> {
        let identity = path::from_native_cmake(&self.host.canonicalize(file)?);
        if self.files.contains_key(&identity) {
            return Ok((identity, BString::default()));
        }
        if !is_file(self.host, file) {
            return Err(HostError::unsupported(
                "Expected a regular Fortran source file.",
            ));
        }
        let text = self.host.read(
            file,
            ReadRange::All {
                limit: *self.bytes_left,
            },
        )?;
        *self.bytes_left = self.bytes_left.saturating_sub(text.len() as u64);
        self.files.insert(identity.clone(), file.into());
        for name in text.split(|byte| *byte == b'\n').filter_map(line_file) {
            if is_file(self.host, &self.settings.dir.join(path::to_native(&name))) {
                self.line_files.insert(name);
            }
        }
        Ok((identity, text.into()))
    }
}

impl IncludeResolver for Resolver<'_, '_> {
    fn read(&mut self, from: &[u8], name: &[u8], _quoted: bool) -> Option<(BString, BString)> {
        if self.error.is_some() {
            return None;
        }
        let name = path::to_native(name);
        let source_dir = self.files.get(from)?.parent()?;
        let dirs = std::iter::once(source_dir)
            .chain(self.settings.include_dirs.iter().map(PathBuf::as_path));
        let file = if name.is_absolute() {
            is_file(self.host, &name).then_some(name)?
        } else {
            dirs.map(|dir| dir.join(&name))
                .find(|file| is_file(self.host, file))?
        };
        match self.read_file(&file) {
            Ok(result) => Some(result),
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }
}

fn line_file(line: &[u8]) -> Option<BString> {
    let line = line
        .trim_ascii_start()
        .strip_prefix(b"#")?
        .trim_ascii_start();
    let directive = line
        .strip_prefix(b"line")
        .or_else(|| line.strip_prefix(b"LINE"));
    let line = directive.map_or(line, <[u8]>::trim_ascii_start);
    let digits = line.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 && directive.is_none() {
        return None;
    }
    let line = line.get(digits..)?.trim_ascii_start().strip_prefix(b"\"")?;
    let mut name = BString::default();
    let mut bytes = line.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'"' {
            return (!name.is_empty() && !name.starts_with(b"<"))
                .then(|| rcmake_base::path_lex::cmake_style(&name));
        }
        if byte == b'\\' && matches!(bytes.peek(), Some(b'\\' | b'"')) {
            name.extend(bytes.next());
        } else {
            name.push(byte);
        }
    }
    None
}
