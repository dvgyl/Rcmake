use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rcmake_base::{BString, path_lex::Lexical};

use crate::{FortranCompiler, FortranSourceInfo};

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
    /// Two objects provide the same logical module.
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
    requires: BTreeSet<BString>,
    includes: BTreeSet<BString>,
}

#[derive(Default)]
struct Graph {
    edges: BTreeMap<BString, Edge>,
    modules: ModuleMap,
}

/// Write a Ninja dyndep file and the target's exported module paths.
///
/// Local providers take precedence over linked modules. Linked maps are read
/// in link order. This operation adds module file dependencies only.
pub fn collate_ninja_dyndep(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
) -> Result<Collation, CollateError> {
    let graph = collate(objects, info, linked)?;
    let mut text = BString::from("ninja_dyndep_version = 1.0\n");
    for (object, edge) in graph.edges {
        text.extend_from_slice(b"build ");
        ninja_path(&mut text, &object)?;
        ninja_paths(&mut text, &edge.provides)?;
        text.extend_from_slice(b": dyndep");
        ninja_paths(&mut text, &edge.requires)?;
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

/// Write GNU Make dependency rules and the target's exported module paths.
///
/// Include files and module files are prerequisites of the object that needs
/// them. Each local module file also depends on the object that produces it.
pub fn collate_make_depend(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
) -> Result<Collation, CollateError> {
    let graph = collate(objects, info, linked)?;
    let mut text = BString::default();
    for (object, edge) in graph.edges {
        let mut prerequisites = edge.requires;
        prerequisites.extend(edge.includes);
        if !prerequisites.is_empty() {
            make_path(&mut text, &object, true)?;
            text.push(b':');
            for path in prerequisites {
                text.push(b' ');
                make_path(&mut text, &path, false)?;
            }
            text.push(b'\n');
        }
        for module in edge.provides {
            make_path(&mut text, &module, true)?;
            text.extend_from_slice(b": ");
            make_path(&mut text, &object, false)?;
            text.push(b'\n');
        }
    }
    Ok(Collation {
        text,
        modules: graph.modules,
    })
}

fn collate(
    objects: &[ObjectScan],
    info: &DependInfo<'_>,
    linked: &[ModuleMap],
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
    let mut providers = BTreeMap::<BString, BString>::new();
    let module_dir = info.module_dir.as_deref().map_or_else(
        || info.dir_cur_bld.clone(),
        |dir| Lexical::new(&info.dir_cur_bld).append(dir),
    );
    for (object, scan) in &scans {
        let mut edge = Edge::default();
        for name in &scan.provides {
            let module = BString::from(name.to_ascii_lowercase());
            let file = info
                .compiler
                .module_filename(&module)
                .ok_or_else(|| CollateError::InvalidModule(module.clone()))?;
            if let Some(first) = providers.get(&module) {
                if first == object {
                    continue;
                }
                return Err(CollateError::DuplicateProvider {
                    module,
                    objects: [first.clone(), object.clone()],
                });
            }
            let path = relative_to_top(&Lexical::new(&module_dir).append(&file), &info.dir_top_bld);
            if !outputs.insert(path.clone()) {
                return Err(CollateError::DuplicateOutput(path));
            }
            providers.insert(module.clone(), object.clone());
            graph.modules.insert(module, path.clone());
            edge.provides.insert(path);
        }
        edge.includes = scan
            .resolved_includes
            .iter()
            .map(|path| relative_to_top(path, &info.dir_top_bld))
            .collect();
        graph.edges.insert(object.clone(), edge);
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
                if let Some(provider) = providers.get(&name) {
                    if provider == object {
                        continue;
                    }
                    local.insert(provider.clone());
                }
                if let Some(path) = graph.modules.get(&name).or_else(|| external.get(&name)) {
                    edge.requires.insert(path.clone());
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
