use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_fortran::{DependInfo, ModuleMap, ObjectScan, SourceForm, collate_make_depend};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode, path};

use crate::copy_f90_mod::module_file_names;
use crate::ninja_depends::{ScanSettings, compiler, physical_name, scan_source};

use super::Source;

/// One Fortran source and its Makefiles object output.
pub struct FortranSource {
    /// Source path, object path, and include directories.
    pub source: Source,
    /// Optional source form from the `Fortran_FORMAT` property.
    pub format: Option<BString>,
}

/// Target settings for a Make Fortran dependency scan.
#[derive(Default)]
pub struct FortranTarget {
    /// Target build directory.
    pub build_dir: PathBuf,
    /// Directory for compiler module files, relative to the build directory or absolute.
    pub module_dir: Option<PathBuf>,
    /// CMake Fortran compiler ID.
    pub compiler_id: BString,
    /// Compiler separator between the ancestor module and submodule names.
    pub submodule_sep: Option<BString>,
    /// Compiler submodule file extension.
    pub submodule_ext: Option<BString>,
    /// Preprocessor definitions for the target.
    pub defines: Vec<BString>,
    /// Whether explicitly intrinsic module imports are dependencies.
    pub building_intrinsic_modules: bool,
    /// Dependency information files of linked targets.
    pub linked_info_files: Vec<PathBuf>,
    /// Sources to scan.
    pub sources: Vec<FortranSource>,
}

pub(super) struct Output {
    pub(super) rules: BString,
    pub(super) internal: BString,
    modules: BString,
    clean: Option<BString>,
}

impl Output {
    pub(super) fn write(&self, host: &dyn Host, output_dir: &Path) -> HostResult<()> {
        let clean = output_dir.join("cmake_clean_Fortran.cmake");
        if let Some(data) = &self.clean {
            host.write(&clean, data, WriteMode::Truncate)?;
        } else if let Err(error) = host.remove_file(&clean)
            && error.kind != std::io::ErrorKind::NotFound
        {
            return Err(error);
        }
        host.write(
            &output_dir.join("fortran.internal"),
            &self.modules,
            WriteMode::Truncate,
        )
    }
}

pub(super) fn collect(
    host: &dyn Host,
    build_root: &Path,
    output_dir: &Path,
    target: &FortranTarget,
    bytes_left: &mut u64,
) -> HostResult<Output> {
    let build_dir = build_root.join(&target.build_dir);
    let mut info = DependInfo::new(compiler(
        &target.compiler_id,
        target.submodule_sep.as_ref().map(|value| value.as_slice()),
        target.submodule_ext.as_ref().map(|value| value.as_slice()),
    ));
    info.dir_top_bld = path::from_native_cmake(build_root);
    info.dir_cur_bld = path::from_native_cmake(&build_dir);
    info.module_dir = target.module_dir.as_deref().map(path::from_native_cmake);

    let mut internal = BString::default();
    let mut scans = Vec::new();
    let mut include_dirs_seen = BTreeSet::new();
    let mut required_names = BTreeMap::<_, BTreeSet<_>>::new();
    let mut sources: Vec<_> = target.sources.iter().collect();
    sources.sort_by(|first, second| first.source.object.cmp(&second.source.object));
    for entry in sources {
        let source = &entry.source;
        let settings = ScanSettings {
            dir: build_root,
            include_dirs: &source.include_dirs,
            form: match entry.format.as_ref().map(|value| value.as_slice()) {
                Some(b"FIXED") => Some(SourceForm::Fixed),
                Some(b"FREE") => Some(SourceForm::Free),
                _ => None,
            },
            defines: &target.defines,
            building_intrinsic_modules: target.building_intrinsic_modules,
        };
        let result = scan_source(host, &source.input, &settings, bytes_left)?;
        let object = path::from_native_cmake(&source.object);
        let relative = source
            .object
            .strip_prefix(build_root)
            .unwrap_or(&source.object);
        internal.extend_from_slice(&path::from_native_cmake(relative));
        for file in std::iter::once(path::from_native_cmake(&source.input))
            .chain(result.resolved_includes.iter().cloned())
        {
            if file.iter().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
                return Err(HostError::unsupported("Invalid Fortran include path."));
            }
            internal.extend_from_slice(b"\n ");
            internal.extend_from_slice(&file);
        }
        internal.push(b'\n');
        for directory in &source.include_dirs {
            let directory = path::from_native_cmake(directory);
            if include_dirs_seen.insert(directory.clone()) {
                info.include_dirs.push(directory);
            }
        }
        for required in &result.requires {
            let name = physical_name(&info.compiler, required).map_err(fortran_error)?;
            required_names
                .entry(name)
                .or_default()
                .insert(required.clone());
        }
        for provided in &result.provides {
            physical_name(&info.compiler, provided).map_err(fortran_error)?;
        }
        scans.push(ObjectScan {
            object,
            info: result,
        });
    }

    let linked = if required_names.is_empty() {
        Vec::new()
    } else {
        target
            .linked_info_files
            .iter()
            .map(|file| linked_modules(host, build_root, file, &required_names, bytes_left))
            .collect::<HostResult<Vec<_>>>()?
    };
    let exists = |file: &[u8]| {
        host.metadata(&path::to_native(file), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    };
    let result = collate_make_depend(
        &scans,
        &info,
        &linked,
        &path::from_native_cmake(output_dir),
        Some(&exists),
    )
    .map_err(|error| HostError::unsupported(error.to_string()))?;

    let provided: BTreeSet<_> = result
        .modules
        .keys()
        .map(|name| physical_name(&info.compiler, name).map_err(fortran_error))
        .collect::<HostResult<_>>()?;
    let mut modules = BString::from("# The fortran modules provided by this target.\nprovides\n");
    for name in &provided {
        modules.push(b' ');
        modules.extend_from_slice(name);
        modules.push(b'\n');
    }

    let clean = if provided.is_empty() {
        None
    } else {
        let module_dir = target
            .module_dir
            .as_ref()
            .map_or_else(|| build_dir.clone(), |dir| build_dir.join(dir));
        let mut contents =
            BString::from("# Remove fortran modules provided by this target.\nFILE(REMOVE");
        for name in provided {
            let (lower, upper) = module_file_names(&name);
            let stamp = [name.as_slice(), b".stamp"].concat();
            for file in [
                module_dir.join(path::to_native(&lower)),
                module_dir.join(path::to_native(&upper)),
                output_dir.join(path::to_native(&stamp)),
            ] {
                let relative = file.strip_prefix(&build_dir).unwrap_or(&file);
                append_clean_path(&mut contents, &path::from_native_cmake(relative))?;
            }
        }
        contents.extend_from_slice(b"  )\n");
        Some(contents)
    };
    let mut rules = BString::from(
        "# Note that incremental build could trigger a call to cmake_copy_f90_mod on each re-build\n",
    );
    rules.extend_from_slice(&result.text);
    Ok(Output {
        rules,
        internal,
        modules,
        clean,
    })
}

fn linked_modules(
    host: &dyn Host,
    build_root: &Path,
    info_file: &Path,
    required: &BTreeMap<BString, BTreeSet<BString>>,
    bytes_left: &mut u64,
) -> HostResult<ModuleMap> {
    let info_file = build_root.join(info_file);
    let directory = info_file
        .parent()
        .ok_or_else(|| HostError::unsupported("Invalid linked Fortran target path."))?;
    let file = directory.join("fortran.internal");
    let contents = host
        .read(&file, ReadRange::All { limit: *bytes_left })
        .map_err(|error| {
            HostError::from_io(
                std::io::Error::new(
                    error.kind,
                    format!(
                        "Cannot read {} for Fortran module information: {error}",
                        file.display()
                    ),
                ),
                Some(file),
            )
        })?;
    *bytes_left = bytes_left.saturating_sub(contents.len() as u64);
    let mut provided = false;
    let mut modules = ModuleMap::new();
    for line in contents.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(name) = line.strip_prefix(b" ") {
            if provided {
                let (name, _) = module_file_names(name);
                if let Some(names) = required.get(&name) {
                    let stamp = [name.as_slice(), b".stamp"].concat();
                    let file = directory.join(path::to_native(&stamp));
                    for logical in names {
                        modules.insert(logical.clone(), path::from_native_cmake(&file));
                    }
                }
            }
        } else {
            provided = line == b"provides";
        }
    }
    Ok(modules)
}

fn append_clean_path(text: &mut BString, file: &[u8]) -> HostResult<()> {
    text.extend_from_slice(b"\n  \"");
    for &byte in file {
        match byte {
            0 | b'\r' | b'\n' => {
                return Err(HostError::unsupported("Invalid Fortran module path."));
            }
            b'\\' | b'"' | b'$' => text.push(b'\\'),
            _ => {}
        }
        text.push(byte);
    }
    text.push(b'"');
    Ok(())
}

fn fortran_error(error: BString) -> HostError {
    HostError::unsupported(error.to_string())
}

#[cfg(test)]
mod tests;
