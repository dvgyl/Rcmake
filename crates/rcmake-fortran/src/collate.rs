use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rcmake_base::{BString, path_lex::Lexical};

use crate::{FortranCompiler, FortranSourceInfo, ModuleCase};

/// Map from lowercase logical module names to build paths.
pub type ModuleMap = BTreeMap<BString, BString>;

/// Source and object paths recorded for a target's dependency scan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependSource {
    /// Object file path.
    pub object: BString,
    /// Original Fortran source path.
    pub source: BString,
    /// Preprocessed source path, when preprocessing is a separate build step.
    pub preprocessed: Option<BString>,
}

/// Target settings for Fortran scanning and collation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependInfo<'a> {
    /// Current build directory.
    pub dir_cur_bld: BString,
    /// Top-level build directory. Render absolute paths under it as relative paths.
    pub dir_top_bld: BString,
    /// Compiler include search directories, in order.
    pub include_dirs: Vec<BString>,
    /// Output directory for module files. Defaults to the current build directory.
    pub module_dir: Option<BString>,
    /// Directories that contain linked targets' dependency summaries, in link order.
    pub linked_target_dirs: Vec<BString>,
    /// Compiler module naming settings.
    pub compiler: FortranCompiler<'a>,
    /// Source paths for the target.
    pub sources: Vec<DependSource>,
}

impl<'a> DependInfo<'a> {
    /// Construct target settings for a compiler.
    pub fn new(compiler: FortranCompiler<'a>) -> Self {
        Self {
            dir_cur_bld: BString::default(),
            dir_top_bld: BString::default(),
            include_dirs: Vec::new(),
            module_dir: None,
            linked_target_dirs: Vec::new(),
            compiler,
            sources: Vec::new(),
        }
    }
}

/// Owned target settings for the build graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FortranTdi {
    /// Current build directory.
    pub dir_cur_bld: BString,
    /// Top-level build directory.
    pub dir_top_bld: BString,
    /// Compiler include search directories, in order.
    pub include_dirs: Vec<BString>,
    /// Directory for module files.
    pub module_dir: Option<BString>,
    /// Directories with linked targets' dependency summaries.
    pub linked_target_dirs: Vec<BString>,
    /// CMake compiler ID.
    pub compiler_id: BString,
    /// Separator between the ancestor and submodule names.
    pub submodule_sep: BString,
    /// Submodule file extension, including the leading dot.
    pub submodule_ext: BString,
    /// Module case recorded in the toolchain.
    pub module_case: ModuleCase,
    /// Source paths for the target.
    pub sources: Vec<DependSource>,
}

impl FortranTdi {
    /// Create collation settings that borrow the compiler data.
    pub fn as_depend_info(&self) -> DependInfo<'_> {
        DependInfo {
            dir_cur_bld: self.dir_cur_bld.clone(),
            dir_top_bld: self.dir_top_bld.clone(),
            include_dirs: self.include_dirs.clone(),
            module_dir: self.module_dir.clone(),
            linked_target_dirs: self.linked_target_dirs.clone(),
            compiler: FortranCompiler {
                id: &self.compiler_id,
                submodule_sep: &self.submodule_sep,
                submodule_ext: &self.submodule_ext,
                module_case: self.module_case,
            },
            sources: self.sources.clone(),
        }
    }
}

/// Scan results assigned to one object path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObjectScan {
    /// Object path as used by the build graph.
    pub object: BString,
    /// Dependencies found by the source scanner.
    pub info: FortranSourceInfo,
}

/// A build dependency file and the module paths exported by this target.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Collation {
    /// Build dependency file bytes.
    pub text: BString,
    /// Module paths for consumers of this target.
    pub modules: ModuleMap,
}

/// Invalid or conflicting inputs to dependency collation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollateError {
    /// A path cannot be represented by the build file format.
    InvalidPath(BString),
    /// An invalid logical module name.
    InvalidModule(BString),
    /// Two scans use the same object path.
    DuplicateObject(BString),
    /// Two objects provide the same logical module in resolved branches.
    DuplicateProvider {
        /// Logical module name.
        module: BString,
        /// Paths of the two providers, in sorted order.
        objects: [BString; 2],
    },
    /// Two outputs map to the same file path.
    DuplicateOutput(BString),
    /// A cycle exists between object files in this target.
    Cycle,
}

impl fmt::Display for CollateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(formatter, "Invalid Fortran build path {path:?}."),
            Self::InvalidModule(module) => {
                write!(formatter, "Invalid Fortran module name {module:?}.")
            }
            Self::DuplicateObject(path) => write!(formatter, "Duplicate Fortran object {path:?}."),
            Self::DuplicateProvider {
                module,
                objects: [first, second],
            } => {
                write!(
                    formatter,
                    "Multiple rules generate Fortran module {module:?} from {first:?} and {second:?}."
                )
            }
            Self::DuplicateOutput(path) => write!(formatter, "Multiple rules generate {path:?}."),
            Self::Cycle => formatter.write_str("Fortran module dependencies form a cycle."),
        }
    }
}

impl std::error::Error for CollateError {}

#[derive(Default)]
struct Edge {
    provides: BTreeSet<BString>,
    requires: BTreeMap<BString, BString>,
    includes: BTreeSet<BString>,
    alternate_inputs: BTreeSet<BString>,
}

#[derive(Default)]
struct Graph {
    edges: BTreeMap<BString, Edge>,
    modules: ModuleMap,
    stamp_inputs: BTreeMap<BString, Vec<BString>>,
}

struct Provider<'a> {
    primary: &'a BString,
    alternatives: Vec<&'a BString>,
    conditional: bool,
}

/// Write a Ninja dyndep file and the target's exported module paths.
///
/// Local providers take precedence over linked modules. Linked maps are read
/// in link order. If several conditional providers remain possible, consumers
/// wait for all their objects.
pub fn collate_ninja_dyndep(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
) -> Result<Collation, CollateError> {
    let graph = collate_with(objects, info, linked, None)?;
    let mut text = BString::from("ninja_dyndep_version = 1.0\n");
    for (object, edge) in graph.edges {
        text.extend_from_slice(b"build ");
        ninja_path(&mut text, &object)?;
        ninja_paths(&mut text, &edge.provides)?;
        text.extend_from_slice(b": dyndep");
        let mut prerequisites = edge.alternate_inputs;
        prerequisites.extend(edge.requires.into_values());
        ninja_paths(&mut text, &prerequisites)?;
        text.push(b'\n');
        if !edge.provides.is_empty() {
            text.extend_from_slice(b"  restat = 1\n");
        }
        text.push(b'\n');
    }
    Ok(Collation {
        text,
        modules: graph.modules,
    })
}

/// Checks whether a candidate module file exists at a compiler include path.
pub type ModuleFileProbe<'a> = dyn Fn(&[u8]) -> bool + 'a;

/// Write GNU Make dependency and module stamp rules.
///
/// The stamp directory is also the target directory that contains its build rule.
/// Pass linked targets' exported stamp paths in link order. The optional
/// file probe checks include directories for modules with no known provider.
pub fn collate_make_depend(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
    stamp_dir: &[u8],
    file_exists: Option<&ModuleFileProbe<'_>>,
) -> Result<Collation, CollateError> {
    let settings = MakeSettings {
        stamp_dir,
        file_exists,
    };
    let graph = collate_with(objects, info, linked, Some(&settings))?;
    let mut text = BString::default();
    for (object, edge) in graph.edges {
        if !edge.includes.is_empty() {
            make_path(&mut text, &object, true)?;
            text.push(b':');
            for include in edge.includes {
                text.extend_from_slice(b" \\\n ");
                make_path(&mut text, &include, false)?;
            }
            text.push(b'\n');
        }
        for path in edge.requires.into_values() {
            make_rule(&mut text, &object, &path)?;
        }
        if edge.provides.is_empty() {
            continue;
        }

        let provides = [object.as_slice(), b".provides.build"].concat();
        for module in edge.provides {
            let stamp = settings.stamp(info, &module);
            make_rule(&mut text, &provides, &stamp)?;
            make_path(&mut text, &stamp, true)?;
            text.extend_from_slice(b": ");
            make_path(&mut text, &object, false)?;
            if let Some(alternatives) = graph.stamp_inputs.get(&stamp) {
                for alternative in alternatives {
                    text.push(b' ');
                    make_path(&mut text, alternative, false)?;
                }
            }
            text.push(b'\n');
            text.extend_from_slice(b"\t$(CMAKE_COMMAND) -E cmake_copy_f90_mod ");
            make_shell_arg(&mut text, &module)?;
            text.push(b' ');
            make_shell_arg(&mut text, &stamp)?;
            if !info.compiler.id.is_empty() {
                text.push(b' ');
                make_shell_arg(&mut text, info.compiler.id)?;
            }
            text.push(b'\n');
        }
        make_path(&mut text, &provides, true)?;
        text.extend_from_slice(b":\n\t$(CMAKE_COMMAND) -E touch ");
        make_shell_arg(&mut text, &provides)?;
        text.push(b'\n');
        let driver = Lexical::new(stamp_dir).append(b"build");
        make_rule(
            &mut text,
            &relative_to_top(&driver, &info.dir_top_bld),
            &provides,
        )?;
    }
    Ok(Collation {
        text,
        modules: graph.modules,
    })
}

struct MakeSettings<'a> {
    stamp_dir: &'a [u8],
    file_exists: Option<&'a ModuleFileProbe<'a>>,
}

impl MakeSettings<'_> {
    fn stamp(&self, info: &DependInfo<'_>, module_path: &[u8]) -> BString {
        let stamp = [Lexical::new(module_path).filename(), b".stamp"].concat();
        relative_to_top(
            &Lexical::new(self.stamp_dir).append(&stamp),
            &info.dir_top_bld,
        )
    }

    fn find_module(&self, info: &DependInfo<'_>, name: &[u8]) -> Option<BString> {
        let exists = self.file_exists?;
        let lower = info.compiler.module_filename(name)?;
        let (stem, extension) = Lexical::new(&lower).stem_extension(true);
        let upper = [stem.to_ascii_uppercase().as_slice(), extension].concat();
        for directory in &info.include_dirs {
            for filename in [lower.as_slice(), upper.as_slice()] {
                let path = Lexical::new(directory).append(filename);
                if exists(&path) {
                    return Some(relative_to_top(&path, &info.dir_top_bld));
                }
            }
        }
        None
    }
}

fn make_rule(out: &mut BString, target: &[u8], prerequisite: &[u8]) -> Result<(), CollateError> {
    make_path(out, target, true)?;
    out.extend_from_slice(b": ");
    make_path(out, prerequisite, false)?;
    out.push(b'\n');
    Ok(())
}

fn make_shell_arg(out: &mut BString, value: &[u8]) -> Result<(), CollateError> {
    if value.is_empty() || value.iter().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
        return Err(CollateError::InvalidPath(value.into()));
    }
    let quote = value
        .iter()
        .any(|byte| !byte.is_ascii_alphanumeric() && !b"/._-@+:".contains(byte));
    if quote {
        out.push(b'"');
    }
    for &byte in value {
        match byte {
            b'$' => out.extend_from_slice(b"\\$$"),
            b'"' | b'`' | b'\\' => out.extend_from_slice(&[b'\\', byte]),
            _ => out.push(byte),
        }
    }
    if quote {
        out.push(b'"');
    }
    Ok(())
}

fn collate_with(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
    make: Option<&MakeSettings<'_>>,
) -> Result<Graph, CollateError> {
    let mut scans = BTreeMap::new();
    for scan in objects {
        let object = relative_to_top(&scan.object, &info.dir_top_bld);
        if scans.insert(object.clone(), &scan.info).is_some() {
            return Err(CollateError::DuplicateObject(object));
        }
    }
    let mut outputs: BTreeSet<_> = scans.keys().cloned().collect();
    let mut graph = Graph::default();
    let mut providers = BTreeMap::<BString, Provider<'_>>::new();
    let module_dir = info.module_dir.as_deref().map_or_else(
        || info.dir_cur_bld.clone(),
        |dir| Lexical::new(&info.dir_cur_bld).append(dir),
    );
    for (object, scan) in &scans {
        for name in &scan.provides {
            let module = BString::from(name.to_ascii_lowercase());
            let conditional = scan.conditional_provides.contains(&module);
            if let Some(previous) = providers.get_mut(&module) {
                if previous.primary == object {
                    continue;
                }
                if conditional {
                    if previous.conditional {
                        previous.alternatives.push(object);
                    }
                    continue;
                }
                if !previous.conditional {
                    return Err(CollateError::DuplicateProvider {
                        module,
                        objects: [previous.primary.clone(), object.clone()],
                    });
                }
            }
            providers.insert(
                module,
                Provider {
                    primary: object,
                    alternatives: Vec::new(),
                    conditional,
                },
            );
        }
        let includes = scan
            .resolved_includes
            .iter()
            .map(|path| relative_to_top(path, &info.dir_top_bld))
            .collect();
        graph.edges.insert(
            object.clone(),
            Edge {
                includes,
                ..Edge::default()
            },
        );
    }
    for (module, provider) in &providers {
        let file = info
            .compiler
            .module_filename(module)
            .ok_or_else(|| CollateError::InvalidModule(module.clone()))?;
        let path = relative_to_top(&Lexical::new(&module_dir).append(&file), &info.dir_top_bld);
        if !outputs.insert(path.clone()) {
            return Err(CollateError::DuplicateOutput(path));
        }
        if let Some(edge) = graph.edges.get_mut(provider.primary) {
            edge.provides.insert(path.clone());
        }
        let exported = make.map_or_else(|| path.clone(), |settings| settings.stamp(info, &path));
        if make.is_some() && !provider.alternatives.is_empty() {
            graph.stamp_inputs.insert(
                exported.clone(),
                provider.alternatives.iter().copied().cloned().collect(),
            );
        }
        graph.modules.insert(module.clone(), exported);
    }

    let mut external = ModuleMap::new();
    for modules in linked {
        for (name, path) in modules {
            external
                .entry(BString::from(name.to_ascii_lowercase()))
                .or_insert_with(|| relative_to_top(path, &info.dir_top_bld));
        }
    }
    let mut dependencies = BTreeMap::new();
    for (object, edge) in &mut graph.edges {
        let mut local = BTreeSet::new();
        if let Some(scan) = scans.get(object) {
            for name in &scan.requires {
                let name = BString::from(name.to_ascii_lowercase());
                if scan.provides.contains(&name) {
                    continue;
                }
                if let Some(provider) = providers.get(&name) {
                    if provider.primary == object {
                        continue;
                    }
                    local.insert(provider.primary.clone());
                    for &alternative in &provider.alternatives {
                        if alternative != object {
                            local.insert(alternative.clone());
                            if make.is_none() {
                                edge.alternate_inputs.insert(alternative.clone());
                            }
                        }
                    }
                }
                if let Some(path) = graph.modules.get(&name).or_else(|| external.get(&name)) {
                    edge.requires.insert(name, path.clone());
                } else if let Some(path) =
                    make.and_then(|settings| settings.find_module(info, &name))
                {
                    edge.requires.insert(name, path);
                }
            }
        }
        dependencies.insert(object.clone(), local);
    }
    check_cycles(dependencies)?;
    Ok(graph)
}

fn check_cycles(dependencies: BTreeMap<BString, BTreeSet<BString>>) -> Result<(), CollateError> {
    let mut dependents = BTreeMap::<BString, Vec<BString>>::new();
    let mut outstanding = BTreeMap::new();
    let mut ready = Vec::new();
    for (object, needs) in dependencies {
        if needs.is_empty() {
            ready.push(object);
            continue;
        }
        outstanding.insert(object.clone(), needs.len());
        for dependency in needs {
            dependents
                .entry(dependency)
                .or_default()
                .push(object.clone());
        }
    }
    while let Some(object) = ready.pop() {
        for dependent in dependents.remove(&object).into_iter().flatten() {
            if let Some(count) = outstanding.get_mut(&dependent) {
                *count -= 1;
                if *count == 0 {
                    outstanding.remove(&dependent);
                    ready.push(dependent);
                }
            }
        }
    }
    if outstanding.is_empty() {
        Ok(())
    } else {
        Err(CollateError::Cycle)
    }
}

fn relative_to_top(path: &[u8], top: &[u8]) -> BString {
    if top.is_empty() {
        return path.into();
    }
    let relative = if top.ends_with(b"/") {
        path.strip_prefix(top)
    } else {
        path.strip_prefix(top)
            .and_then(|suffix| suffix.strip_prefix(b"/"))
    };
    relative.unwrap_or(path).into()
}

fn ninja_paths(out: &mut BString, paths: &BTreeSet<BString>) -> Result<(), CollateError> {
    let mut separator = b" | ".as_slice();
    for path in paths {
        out.extend_from_slice(separator);
        ninja_path(out, path)?;
        separator = b" ";
    }
    Ok(())
}

fn ninja_path(out: &mut BString, path: &[u8]) -> Result<(), CollateError> {
    if path.is_empty()
        || path
            .iter()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'\t' | b'|'))
    {
        return Err(CollateError::InvalidPath(path.into()));
    }
    for &byte in path {
        if matches!(byte, b'$' | b' ' | b':') {
            out.push(b'$');
        }
        out.push(byte);
    }
    Ok(())
}

fn make_path(out: &mut BString, path: &[u8], target: bool) -> Result<(), CollateError> {
    if path.is_empty()
        || path.iter().any(|byte| {
            matches!(
                byte,
                0 | b'\n' | b'\r' | b';' | b'=' | b'|' | b'\\' | b'(' | b')'
            )
        })
    {
        return Err(CollateError::InvalidPath(path.into()));
    }
    for &byte in path {
        match byte {
            b'$' => out.extend_from_slice(b"$$"),
            b' ' | b'\t' | b'#' | b':' => out.extend_from_slice(&[b'\\', byte]),
            b'%' if target => out.extend_from_slice(b"\\%"),
            b'*' | b'?' | b'[' if !target => out.extend_from_slice(&[b'\\', byte]),
            _ => out.push(byte),
        }
    }
    Ok(())
}
