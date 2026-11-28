//! Build file generator for Unix Makefiles and NMake Makefiles.

#![forbid(unsafe_code)]

mod build;
mod cuda;
mod directory_info;
mod fortran;
mod plan_step;
mod prep;
mod progress;
mod recipe;
mod response;
mod subdirectories;
use recipe::{make_recipe_text, make_recipe_text_for, shell_invocation_for, write_step};

pub use build::{build_commands, build_commands_for_dialect, build_commands_nmake};
use plan_step::plan_step;

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;

use rcmake_base::{BString, ByteSlice, ByteVec, shell::quote_always};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_gen_common::GenError;
use rcmake_host::{Host, WriteMode, path};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult};

const MAKEFILE2: &[u8] = b"CMakeFiles/Makefile2";
const REGEN_STAMP: &[u8] = b"CMakeFiles/rcmake-regenerate.stamp";
const FORCE: &[u8] = b"CMakeFiles/rcmake-force";
const GLOBAL_TARGETS: &[&[u8]] = &[
    b"all",
    b"clean",
    b"install",
    b"test",
    b"help",
    b"rebuild_cache",
    b"preinstall",
    b"cmake_check_build_system",
];

/// Makefile syntax and shell dialect emitted by [`rcmake-gen-make`](crate).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MakeDialect {
    /// POSIX `/bin/sh` + GNU/BSD `make` (`Unix Makefiles`).
    #[default]
    Unix,
    /// Windows `cmd.exe` + Microsoft `nmake` (`NMake Makefiles`).
    NMake,
}

impl MakeDialect {
    /// Return `true` when emitting `NMake Makefiles`.
    #[must_use]
    pub const fn is_nmake(self) -> bool {
        matches!(self, Self::NMake)
    }

    /// Exact CMake generator name for this dialect.
    #[must_use]
    pub const fn generator_name(self) -> &'static str {
        match self {
            Self::Unix => "Unix Makefiles",
            Self::NMake => "NMake Makefiles",
        }
    }
}

/// Write the top-level `Makefile`, `CMakeFiles/Makefile2`, and per-target build files.
///
/// Use the first graph because Unix Makefiles supports one configuration.
pub fn generate_unix_makefiles(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    generate_with_dialect(result, host, build_dir, None, MakeDialect::Unix)
}

pub use generate_unix_makefiles as generate;

/// Write Unix Makefiles with targets that build the inputs of install rules
/// and tests.
pub fn generate_with_model(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: &ProjectModel,
) -> Result<(), GenError> {
    generate_with_dialect(result, host, build_dir, Some(model), MakeDialect::Unix)
}

/// Write `NMake Makefiles` (`Makefile`, `CMakeFiles/Makefile2`, and per-target build files).
pub fn generate_nmake_makefiles(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    generate_with_dialect(result, host, build_dir, None, MakeDialect::NMake)
}

/// Write `NMake Makefiles` with targets that build the inputs of install rules
/// and tests.
pub fn generate_nmake_with_model(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: &ProjectModel,
) -> Result<(), GenError> {
    generate_with_dialect(result, host, build_dir, Some(model), MakeDialect::NMake)
}

/// Write Makefiles for `dialect` (`Unix Makefiles` or `NMake Makefiles`).
pub fn generate_with_dialect(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
    dialect: MakeDialect,
) -> Result<(), GenError> {
    let build_dir = host.initial_cwd().join(build_dir);
    let empty = BuildGraph::default();
    let graph = result.graphs.first().unwrap_or(&empty);
    let mut targets = plan_targets(result, graph, &build_dir, model, dialect)?;
    response::prepare(&mut targets, &build_dir, host)?;
    let cmake_command = result
        .metadata
        .cmake_command
        .as_ref()
        .filter(|path| !path.is_empty());
    let report_progress = cmake_command.is_some();
    if report_progress {
        progress::assign(&mut targets);
    }
    let cmake = match cmake_command {
        Some(command) => command.clone(),
        None => host
            .current_exe()
            .map(|exe| path::from_native_cmake(&exe))
            .map_err(|error| GenError::Write {
                path: "Makefile".into(),
                message: error.to_string().into(),
            })?,
    };
    let prep = prep::targets(result, graph, &targets, &build_dir, model)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, &build_dir, host)?;
    let mut top = top_level(
        result,
        graph,
        &targets,
        &cmake,
        &prep,
        (model, report_progress),
        verify_globs,
        dialect,
    )?;
    let mut dispatch = dispatch(
        graph,
        &targets,
        &prep,
        report_progress && target_messages(model),
        &result.metadata.clean_files,
        &cmake,
        dialect,
    )?;
    let languages = rcmake_resolve::progress::compile_languages(result, graph);
    let build_files = targets
        .iter()
        .map(|target| build_file(target, &languages, &build_dir, &cmake, host, dialect))
        .collect::<Result<Vec<_>, _>>()?;

    for (target, contents) in targets.iter().zip(build_files) {
        if let Some(fortran) = &target.fortran {
            fortran.write(host, &build_dir, target)?;
        }
        write_makefile(host, &build_dir, &target.file, &contents)?;
    }
    if let Some(model) = model {
        subdirectories::write(
            subdirectories::Context {
                host,
                build: &build_dir,
                model,
                result,
                targets: &targets,
                cmake: &cmake,
                dialect,
            },
            &mut top,
            &mut dispatch,
        )?;
    }
    write_makefile(host, &build_dir, MAKEFILE2, &dispatch)?;
    write_makefile(host, &build_dir, b"Makefile", &top)?;
    if let Some(model) = model {
        directory_info::write(host, model, dialect)?;
    }
    if !result.metadata.listfiles.is_empty() {
        let stamp = build_dir.join(path::to_native(REGEN_STAMP));
        host.write(&stamp, b"", WriteMode::Truncate)
            .map_err(|error| GenError::Write {
                path: path::from_native(&stamp),
                message: error.to_string().into(),
            })?;
    }
    Ok(())
}

struct Target<'a> {
    name: &'a [u8],
    file: BString,
    goal: BString,
    clean_goal: BString,
    dependencies: BTreeSet<usize>,
    rules: Vec<Step<'a>>,
    products: Vec<BString>,
    fortran: Option<fortran::TargetDepend<'a>>,
}

struct Step<'a> {
    action: &'a BuildStep,
    goal: BString,
    outputs: Vec<BString>,
    symbolic_outputs: Vec<BString>,
    revised_invocations: Option<Vec<rcmake_resolve::Invocation>>,
    inputs: Vec<BString>,
    depfile: Option<BString>,
    always: bool,
    progress: Option<usize>,
    jobserver_aware: bool,
    /// Comment of a custom command with outputs and no `COMMENT`.
    default_comment: Option<BString>,
}

fn invalid_graph(message: impl Into<BString>) -> GenError {
    GenError::Write {
        path: "Makefile".into(),
        message: message.into(),
    }
}

fn build_path(value: &[u8], build_dir: &Path) -> BString {
    let build_prefix = path::from_native(build_dir);
    build_path_with_prefix(value, build_dir, &build_prefix)
}

fn build_path_with_prefix(value: &[u8], build_dir: &Path, build_prefix: &[u8]) -> BString {
    fn needs_native(bytes: &[u8]) -> bool {
        bytes.is_empty()
            || bytes.contains(&b'\\')
            || bytes.starts_with(b".")
            || bytes.ends_with(b"/")
            || bytes.find(b"//").is_some()
            || bytes.find(b"/.").is_some()
    }
    if !needs_native(value) && !needs_native(build_prefix) && !build_prefix.ends_with(b":/") {
        if value == build_prefix {
            return BString::default();
        }
        if let Some(rest) = value
            .strip_prefix(build_prefix)
            .and_then(|rest| rest.strip_prefix(b"/"))
        {
            return rest.into();
        }
        return value.into();
    }
    let path = path::to_native(value);
    let path = path.strip_prefix(build_dir).unwrap_or(&path);
    let path: std::path::PathBuf = path.components().collect();
    rcmake_base::path_lex::cmake_style(&path::from_native(&path))
}

fn plan_targets<'a>(
    result: &'a GenerationResult,
    graph: &'a BuildGraph,
    build_dir: &Path,
    project: Option<&ProjectModel>,
    dialect: MakeDialect,
) -> Result<Vec<Target<'a>>, GenError> {
    let mut targets = Vec::with_capacity(graph.targets.len());
    let resolved: BTreeMap<_, _> = result
        .targets
        .iter()
        .filter(|target| target.config == graph.config)
        .map(|target| (target.metadata.name.as_slice(), target))
        .collect();
    let mut names = BTreeMap::new();
    for (index, target) in graph.targets.iter().enumerate() {
        let name = target.name.as_slice();
        check_make_path_for(dialect, name)?;
        let directory_codegen = name.ends_with(b"/codegen")
            && !name.starts_with(b"/")
            && !name
                .split(|&byte| byte == b'/')
                .any(|part| matches!(part, b"" | b"." | b".."));
        if (name.contains(&b'/') && !directory_codegen)
            || GLOBAL_TARGETS.contains(&name)
            || names.insert(name, index).is_some()
        {
            return Err(invalid_graph(format!(
                "invalid or duplicate target name: {}",
                target.name
            )));
        }
        let mut directory = b"CMakeFiles/".to_vec();
        directory.push_str(name);
        directory.push_str(b".dir/");
        targets.push(Target {
            name,
            file: [directory.as_slice(), b"build.make"].concat().into(),
            goal: [directory.as_slice(), b"all"].concat().into(),
            clean_goal: [directory.as_slice(), b"clean"].concat().into(),
            dependencies: target.depends.iter().copied().collect(),
            rules: Vec::new(),
            products: Vec::new(),
            fortran: resolved
                .get(name)
                .and_then(|target| fortran::TargetDepend::new(target)),
        });
    }

    let artifacts: BTreeMap<_, _> = result
        .targets
        .iter()
        .filter(|target| target.config == graph.config)
        .map(|target| (target.metadata.name.as_slice(), &target.artifacts))
        .collect();
    let build_prefix = path::from_native(build_dir);
    let mut owners = BTreeMap::new();
    let mut producers = BTreeMap::new();
    let mut symbolic = BTreeSet::new();
    for index in dependency_order(&targets)? {
        let (Some(model), Some(target)) = (graph.targets.get(index), targets.get_mut(index)) else {
            continue;
        };
        let repeat_rules = project
            .and_then(|project| {
                project
                    .find_target(DirId::ROOT, target.name)
                    .and_then(|id| project.target(id))
            })
            .is_some_and(|target| {
                target.policies.policies.get(PolicyId::new(113)) != PolicyStatus::New
            });
        let codegen = (target.name == b"codegen" || target.name.ends_with(b"/codegen"))
            && project.is_some_and(|project| {
                project
                    .dir(DirId::ROOT)
                    .final_policies
                    .is_some_and(|policies| policies.get(PolicyId::new(171)) == PolicyStatus::New)
            });
        for &step_index in &model.steps {
            let action = graph
                .steps
                .get(step_index)
                .ok_or_else(|| invalid_graph("invalid build step index"))?;
            let owner = *owners.entry(step_index).or_insert(index);
            let repeated = owner != index;
            if repeated && !codegen {
                target.dependencies.insert(owner);
                if !repeat_rules
                    || !matches!(action, BuildStep::Custom(_))
                    || !has_dependency(graph, index, owner)
                {
                    continue;
                }
            }
            let mut step = plan_step(action, step_index, result, build_dir, &build_prefix)?;
            if let BuildStep::Link { step: link, .. } = action {
                let make_path =
                    |value: &BString| build_path_with_prefix(value, build_dir, &build_prefix);
                if let Some(artifacts) = artifacts.get(target.name) {
                    step.outputs.extend(
                        [&artifacts.linker_file, &artifacts.soname_file]
                            .into_iter()
                            .filter_map(Option::as_ref)
                            .map(make_path),
                    );
                    target.products.extend(
                        [&artifacts.file, &artifacts.import_file, &artifacts.pdb_file]
                            .into_iter()
                            .filter_map(Option::as_ref)
                            .map(make_path),
                    );
                }
                if let Some(byproducts) = graph.link_event_byproducts.get(&link.output) {
                    target.products.extend(byproducts.iter().map(make_path));
                }
            }

            let mut seen = HashSet::with_capacity(step.outputs.len());
            step.outputs.retain(|output| seen.insert(output.clone()));
            for output in &step.outputs {
                if !repeated && producers.insert(output.clone(), index).is_some() {
                    return Err(invalid_graph(format!(
                        "multiple build steps produce {output}"
                    )));
                }
            }
            if matches!(action, BuildStep::Custom(custom) if custom.symbolic) {
                symbolic.insert(step.goal.clone());
            }
            symbolic.extend(step.symbolic_outputs.iter().cloned());
            target
                .products
                .extend(step.outputs.iter().chain(&step.depfile).cloned());
            target.rules.push(step);
        }
    }

    let link_outputs: BTreeMap<_, _> = targets
        .iter()
        .filter_map(|target| {
            let link = target
                .rules
                .iter()
                .find(|step| matches!(step.action, BuildStep::Link { .. }))?;
            Some((target.name, link.goal.clone()))
        })
        .collect();
    for (index, target) in targets.iter_mut().enumerate() {
        let own_outputs: HashSet<BString> = target
            .rules
            .iter()
            .flat_map(|rule| rule.outputs.iter().cloned())
            .collect();
        for step in &mut target.rules {
            let is_custom = matches!(step.action, BuildStep::Custom(_));
            let mut inputs = BTreeSet::new();
            for input in std::mem::take(&mut step.inputs) {
                let file = if is_custom && let Some(&dependency) = names.get(input.as_slice()) {
                    if dependency != index {
                        target.dependencies.insert(dependency);
                    }
                    let Some(output) = link_outputs.get(input.as_slice()) else {
                        continue;
                    };
                    output.clone()
                } else if is_custom {
                    build_path_with_prefix(&input, build_dir, &build_prefix)
                } else {
                    input
                };
                if let Some(owner) = producers.get(&file)
                    && *owner != index
                    && !own_outputs.contains(&file)
                {
                    target.dependencies.insert(*owner);
                    if symbolic.contains(&file) {
                        inputs.insert(BString::from(FORCE));
                        continue;
                    }
                }
                inputs.insert(file);
            }
            step.inputs = inputs.into_iter().collect();
        }
    }
    dependency_order(&targets)?;
    Ok(targets)
}

fn has_dependency(graph: &BuildGraph, target: usize, dependency: usize) -> bool {
    let mut pending = vec![target];
    let mut visited = vec![false; graph.targets.len()];
    while let Some(index) = pending.pop() {
        if index == dependency {
            return true;
        }
        if let Some(slot) = visited.get_mut(index)
            && !*slot
        {
            *slot = true;
            if let Some(target) = graph.targets.get(index) {
                pending.extend(&target.depends);
            }
        }
    }
    false
}

fn dependency_order(targets: &[Target<'_>]) -> Result<Vec<usize>, GenError> {
    let mut consumers = vec![Vec::new(); targets.len()];
    let mut counts = Vec::with_capacity(targets.len());
    let mut ready = VecDeque::new();
    for (index, target) in targets.iter().enumerate() {
        counts.push(target.dependencies.len());
        if target.dependencies.is_empty() {
            ready.push_back(index);
        }
        for &dependency in &target.dependencies {
            consumers
                .get_mut(dependency)
                .ok_or_else(|| invalid_graph("invalid target dependency index"))?
                .push(index);
        }
    }
    let mut order = Vec::with_capacity(targets.len());
    while let Some(index) = ready.pop_front() {
        order.push(index);
        for &consumer in consumers.get(index).into_iter().flatten() {
            if let Some(count) = counts.get_mut(consumer) {
                *count -= 1;
                if *count == 0 {
                    ready.push_back(consumer);
                }
            }
        }
    }
    if order.len() != targets.len() {
        return Err(invalid_graph("cycle in target dependencies"));
    }
    Ok(order)
}

fn write_makefile(
    host: &dyn Host,
    directory: &Path,
    file: &[u8],
    contents: &[u8],
) -> Result<(), GenError> {
    let destination = directory.join(path::to_native(file));
    rcmake_gen_common::write_output(host, &destination, contents)
}

fn makefile_header(dialect: MakeDialect, cmake: Option<&[u8]>) -> Result<Vec<u8>, GenError> {
    if !dialect.is_nmake() {
        return Ok(b"# Generated by Rcmake.\n.SUFFIXES:\nSHELL = /bin/sh\n\
SILENT = $(SILENT_$(VERBOSE))\nSILENT_ = @\n\
MAKE_SILENT = $(MAKE_SILENT_$(VERBOSE))\nMAKE_SILENT_ = -s\n\n"
            .to_vec());
    }
    let mut out = b"# Generated by Rcmake.\n\
!IF \"$(OS)\" == \"Windows_NT\"\n\
NULL=\n\
!ELSE\n\
NULL=nul\n\
!ENDIF\n\
SHELL = cmd.exe\n\
!IF \"$(VERBOSE)\" == \"\"\n\
SILENT = @\n\
MAKE_SILENT = /nologo\n\
!ELSE\n\
SILENT =\n\
MAKE_SILENT =\n\
!ENDIF\n"
        .to_vec();
    if let Some(cmake) = cmake {
        out.push_str(b"!IFNDEF CMAKE_COMMAND\nCMAKE_COMMAND = ");
        make_recipe_text_for(dialect, &mut out, &quote_cmd(cmake))?;
        out.push_str(b"\n!ENDIF\n");
    }
    out.push(b'\n');
    Ok(out)
}

/// Return whether each target reports its completion. The `TARGET_MESSAGES`
/// global property has priority over the `CMAKE_TARGET_MESSAGES` variable.
fn target_messages(model: Option<&ProjectModel>) -> bool {
    let Some(model) = model else {
        return true;
    };
    model
        .global
        .props
        .get(b"TARGET_MESSAGES")
        .or_else(|| model.definition(DirId::ROOT, b"CMAKE_TARGET_MESSAGES"))
        .is_none_or(|value| !rcmake_base::truth::is_off(value))
}

#[allow(clippy::too_many_arguments)]
fn top_level(
    result: &GenerationResult,
    graph: &BuildGraph,
    targets: &[Target<'_>],
    cmake: &[u8],
    prep: &prep::Targets,
    (model, report_progress): (Option<&ProjectModel>, bool),
    verify_globs: bool,
    dialect: MakeDialect,
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile_header(dialect, dialect.is_nmake().then_some(cmake))?;
    if dialect.is_nmake() {
        out.push_str(b"!IFNDEF CTEST_COMMAND\nCTEST_COMMAND = ctest\n!ENDIF\n\n");
    } else {
        // Separate entry points can build the same target. Submakes still run in parallel.
        out.push_str(b".NOTPARALLEL:\nCMAKE_COMMAND ?= ");
        let mut cmake_value = Vec::new();
        make_recipe_text(&mut cmake_value, &quote_always(cmake))?;
        for byte in cmake_value {
            if byte == b'#' {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out.push_str(b"\nCTEST_COMMAND ?= ctest\n\n");
        if report_progress {
            out.push_str(b"export CMAKE_COMMAND\n");
        }
    }
    phony_for(
        dialect,
        &mut out,
        b"all",
        [b"cmake_check_build_system".as_slice()],
    );
    let all = graph
        .targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| target.in_all.then_some(index));
    progress::build(
        dialect,
        &mut out,
        b"all",
        report_progress.then(|| progress::count(targets, all)),
    );
    phony_for(dialect, &mut out, b"clean", []);
    recursive_make_for(dialect, &mut out, MAKEFILE2, b"clean");
    phony_for(dialect, &mut out, b"preinstall", [b"all".as_slice()]);
    recursive_make_for(dialect, &mut out, MAKEFILE2, b"preinstall");
    phony_for(dialect, &mut out, b"install", [b"preinstall".as_slice()]);
    out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -P cmake_install.cmake\n");
    let test_depends_all = model
        .and_then(|model| model.definition(DirId::ROOT, b"CMAKE_SKIP_TEST_ALL_DEPENDENCY"))
        .is_some_and(|value| rcmake_base::truth::is_off(value));
    phony_for(
        dialect,
        &mut out,
        b"test",
        test_depends_all.then_some(b"all".as_slice()),
    );
    out.push_str(b"\t$(SILENT)$(CTEST_COMMAND) --force-new-ctest-process");
    for argument in &result.metadata.ctest_arguments {
        out.push(b' ');
        make_recipe_text_for(dialect, &mut out, &quote_arg(dialect, argument))?;
    }
    out.push(b'\n');

    phony_for(dialect, &mut out, b"edit_cache", []);
    out.push_str(b"\t$(SILENT)");
    if let Some(model) = model
        && let Some(editor) = model
            .definition(DirId::ROOT, b"CMAKE_EDIT_COMMAND")
            .filter(|editor| !editor.is_empty())
    {
        let root = model.dir(DirId::ROOT);
        make_recipe_text_for(dialect, &mut out, &quote_arg(dialect, editor))?;
        for (option, dir) in [
            (b" -S".as_slice(), &root.source_dir),
            (b" -B", &root.binary_dir),
        ] {
            out.push_str(option);
            make_recipe_text_for(dialect, &mut out, &quote_arg(dialect, dir))?;
        }
        out.push(b'\n');
    } else {
        out.push_str(b"$(CMAKE_COMMAND) -E echo \"No interactive CMake dialog available.\"\n");
    }

    let mut configure = b"$(CMAKE_COMMAND) -B .".to_vec();
    let listfiles = &result.metadata.listfiles;
    if let Some(source_dir) = listfiles.iter().find_map(|file| {
        let file = path::to_native(file);
        (file.file_name()? == "CMakeLists.txt")
            .then(|| path::from_native_cmake(file.parent().unwrap_or(Path::new("."))))
    }) {
        configure.push_str(b" -S ");
        make_recipe_text_for(dialect, &mut configure, &quote_arg(dialect, &source_dir))?;
    }
    phony_for(dialect, &mut out, b"rebuild_cache", []);
    out.push_str(b"\t$(SILENT)");
    out.push_str(&configure);
    out.push(b'\n');

    if listfiles.is_empty() {
        phony_for(dialect, &mut out, b"cmake_check_build_system", []);
    } else {
        for file in listfiles {
            check_make_path_for(dialect, file)?;
        }
        phony_for(
            dialect,
            &mut out,
            b"cmake_check_build_system",
            [REGEN_STAMP],
        );
        if verify_globs {
            let script = rcmake_gen_common::glob::SCRIPT.as_bytes();
            let marker = b"CMakeFiles/rcmake-glob-mismatch";
            if dialect.is_nmake() {
                out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -E rm -f ");
                out.push_str(marker);
                out.push_str(b"\n\t$(SILENT)$(CMAKE_COMMAND) -D_RCMAKE_GLOB_MISMATCH_MARKER=");
                out.push_str(marker);
                out.push_str(b" -P ");
                out.push_str(script);
                out.push_str(b"\n\t$(SILENT)if exist \"");
                out.push_str(marker);
                out.push_str(b"\" ");
                out.push_str(&configure);
                out.push_str(b" --regenerate-during-build\n");
            } else {
                out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -E rm -f ");
                out.push_str(marker);
                out.push_str(b" && $(CMAKE_COMMAND) -D_RCMAKE_GLOB_MISMATCH_MARKER=");
                out.push_str(marker);
                out.push_str(b" -P ");
                out.push_str(script);
                out.push_str(b" && if test -f ");
                out.push_str(marker);
                out.push_str(b"; then ");
                out.push_str(&configure);
                out.push_str(b" --regenerate-during-build; fi\n");
            }
        }
        rule_for(
            dialect,
            &mut out,
            REGEN_STAMP,
            listfiles.iter().map(|value| value.as_slice()),
        );
        if dialect.is_nmake() {
            out.push_str(b"\t$(SILENT)");
            out.push_str(&configure);
            out.push_str(b" --regenerate-during-build\n\t$(SILENT)$(CMAKE_COMMAND) -E touch ");
            out.push_str(REGEN_STAMP);
            out.push(b'\n');
        } else {
            out.push_str(b"\t$(SILENT)");
            out.push_str(&configure);
            out.push_str(b" --regenerate-during-build && touch ");
            out.push_str(REGEN_STAMP);
            out.push(b'\n');
        }
        for input in listfiles {
            // A removed include must trigger configure again.
            rule_for(dialect, &mut out, input, []);
        }
    }

    phony_for(dialect, &mut out, b"help", []);
    for name in GLOBAL_TARGETS
        .iter()
        .copied()
        .chain(targets.iter().map(|target| target.name))
        .chain(prep.keys().map(|name| name.as_slice()))
    {
        if dialect.is_nmake() {
            out.push_str(b"\t@$(CMAKE_COMMAND) -E echo ");
        } else {
            out.push_str(b"\t@printf '%s\\n' ");
        }
        make_recipe_text_for(
            dialect,
            &mut out,
            &quote_arg(dialect, &[b"... ", name].concat()),
        )?;
        out.push(b'\n');
    }
    let progress_counts = report_progress.then(|| progress::counts_all(targets));
    for (index, target) in targets.iter().enumerate() {
        phony_for(
            dialect,
            &mut out,
            target.name,
            [b"cmake_check_build_system".as_slice()],
        );
        progress::build(
            dialect,
            &mut out,
            &target.goal,
            progress_counts
                .as_ref()
                .and_then(|counts| counts.get(index).copied()),
        );
    }
    for name in prep.keys() {
        phony_for(
            dialect,
            &mut out,
            name,
            [b"cmake_check_build_system".as_slice()],
        );
        recursive_make_for(dialect, &mut out, MAKEFILE2, name);
    }
    Ok(out)
}

fn dispatch(
    graph: &BuildGraph,
    targets: &[Target<'_>],
    prep: &prep::Targets,
    report_progress: bool,
    clean_paths: &[BString],
    cmake: &[u8],
    dialect: MakeDialect,
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile_header(dialect, dialect.is_nmake().then_some(cmake))?;
    phony_for(
        dialect,
        &mut out,
        b"all",
        graph
            .targets
            .iter()
            .zip(targets)
            .filter(|(model, _)| model.in_all)
            .map(|(_, target)| target.goal.as_slice()),
    );
    phony_for(
        dialect,
        &mut out,
        b"clean",
        targets.iter().map(|target| target.clean_goal.as_slice()),
    );
    for path in clean_paths {
        if dialect.is_nmake() {
            out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -E rm -rf ");
        } else {
            out.push_str(b"\t$(SILENT)rm -rf -- ");
        }
        make_recipe_text_for(dialect, &mut out, &quote_arg(dialect, path))?;
        out.push(b'\n');
    }
    phony_for(dialect, &mut out, b"preinstall", []);
    for target in targets {
        phony_for(
            dialect,
            &mut out,
            &target.goal,
            target
                .dependencies
                .iter()
                .filter_map(|&index| targets.get(index))
                .map(|target| target.goal.as_slice()),
        );
        if let Some(fortran) = &target.fortran {
            recursive_make_for(dialect, &mut out, &target.file, &fortran.scan_goal);
        }
        recursive_make_for(dialect, &mut out, &target.file, &target.goal);
        if report_progress {
            progress::complete(dialect, &mut out, target)?;
        }
        phony_for(dialect, &mut out, &target.clean_goal, []);
        recursive_make_for(dialect, &mut out, &target.file, &target.clean_goal);
    }
    for (name, dependencies) in prep {
        phony_for(
            dialect,
            &mut out,
            name,
            dependencies.iter().map(|goal| goal.as_slice()),
        );
    }
    Ok(out)
}

fn build_file(
    target: &Target<'_>,
    languages: &std::collections::HashMap<&[u8], &[u8]>,
    build_dir: &Path,
    cmake: &[u8],
    host: &dyn Host,
    dialect: MakeDialect,
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile_header(dialect, dialect.is_nmake().then_some(cmake))?;
    if !dialect.is_nmake() {
        out.push_str(b".DELETE_ON_ERROR:\n");
    }
    phony_for(
        dialect,
        &mut out,
        &target.goal,
        target.rules.iter().map(|step| step.goal.as_slice()),
    );
    phony_for(dialect, &mut out, &target.clean_goal, []);
    if let Some(fortran) = &target.fortran {
        fortran.emit_clean(dialect, &mut out, cmake)?;
    }
    for product in target.products.iter().collect::<BTreeSet<_>>() {
        if dialect.is_nmake() {
            out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -E rm -f ");
        } else {
            out.push_str(b"\t$(SILENT)rm -f -- ");
        }
        make_recipe_text_for(dialect, &mut out, &quote_arg(dialect, product))?;
        out.push(b'\n');
    }
    phony_for(dialect, &mut out, FORCE, []);
    if let Some(fortran) = &target.fortran {
        fortran.emit(dialect, &mut out, target, cmake, build_dir)?;
    }
    for step in &target.rules {
        if cuda::emit(
            dialect,
            &mut out,
            build_dir,
            step.action,
            &[],
            &target.file,
            step.progress,
            host,
        )? {
            continue;
        }
        cuda::emit_link_depends(dialect, &mut out, build_dir, step.action)?;
        write_step(dialect, &mut out, step, &target.file, languages)?;
    }
    Ok(out)
}

fn check_make_path_for(dialect: MakeDialect, value: &[u8]) -> Result<(), GenError> {
    let invalid = if dialect.is_nmake() {
        value
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r' | b'\0' | b'|' | b'"'))
    } else {
        value.iter().any(|byte| {
            matches!(
                byte,
                b'\n' | b'\r' | b'\0' | b';' | b'=' | b'|' | b'\\' | b'(' | b')'
            )
        })
    };
    if value.is_empty() || invalid {
        return Err(invalid_graph(format!(
            "unsupported make path: {}",
            BString::from(value)
        )));
    }
    Ok(())
}

/// Double-quote one argument for Windows `cmd.exe` / NMake recipes.
fn quote_cmd(value: &[u8]) -> BString {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    let mut backslashes = 0_usize;
    for &byte in value {
        match byte {
            b'\\' => backslashes += 1,
            b'"' => {
                out.resize(out.len() + backslashes * 2 + 1, b'\\');
                out.push(b'"');
                backslashes = 0;
            }
            _ => {
                out.resize(out.len() + backslashes, b'\\');
                backslashes = 0;
                out.push(byte);
            }
        }
    }
    out.resize(out.len() + backslashes * 2, b'\\');
    out.push(b'"');
    out.into()
}

fn quote_arg(dialect: MakeDialect, value: &[u8]) -> BString {
    match dialect {
        MakeDialect::Unix => quote_always(value),
        MakeDialect::NMake => quote_cmd(value),
    }
}

#[derive(Clone, Copy)]
enum Word {
    Target,
    Input,
    Wildcard,
}

fn make_word_for(dialect: MakeDialect, out: &mut Vec<u8>, value: &[u8], word: Word) {
    if dialect.is_nmake() {
        let needs_quotes = value.iter().any(|byte| {
            matches!(
                byte,
                b' ' | b'\t' | b':' | b'#' | b'(' | b')' | b';' | b'=' | b'&' | b'%' | b'^'
            )
        });
        if needs_quotes {
            out.push(b'"');
        }
        for &byte in value {
            if byte == b'$' {
                out.push_str(b"$$");
            } else {
                out.push(byte);
            }
        }
        if needs_quotes {
            out.push(b'"');
        }
        return;
    }
    for &byte in value {
        match byte {
            b'$' => out.push_str(b"$$"),
            b' ' | b'\t' | b'#' | b':' => out.push_str([b'\\', byte]),
            b'%' if matches!(word, Word::Target) => out.push_str(b"\\%"),
            b'*' | b'?' | b'[' if !matches!(word, Word::Target) => out.push_str([b'\\', byte]),
            _ => out.push(byte),
        }
    }
}

fn rule_for<'a>(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    goal: &[u8],
    inputs: impl IntoIterator<Item = &'a [u8]>,
) {
    make_word_for(dialect, out, goal, Word::Target);
    if dialect.is_nmake() && goal.len() == 1 && goal.first().is_some_and(u8::is_ascii_alphabetic) {
        out.push(b' ');
    }
    out.push(b':');
    for input in inputs {
        out.push(b' ');
        make_word_for(dialect, out, input, Word::Input);
    }
    out.push(b'\n');
}

fn phony_for<'a>(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    goal: &[u8],
    inputs: impl IntoIterator<Item = &'a [u8]>,
) {
    if !dialect.is_nmake() {
        rule_for(dialect, out, b".PHONY", [goal]);
    }
    rule_for(dialect, out, goal, inputs);
}

fn recursive_make_for(dialect: MakeDialect, out: &mut Vec<u8>, file: &[u8], goal: &[u8]) {
    out.push_str(b"\t$(SILENT)$(MAKE) $(MAKE_SILENT) -f ");
    let push_escaped = |out: &mut Vec<u8>, value: &[u8]| {
        for &byte in quote_arg(dialect, value).iter() {
            match byte {
                b'$' => out.push_str(b"$$"),
                b'%' if dialect.is_nmake() => out.push_str(b"%%"),
                _ => out.push(byte),
            }
        }
    };
    push_escaped(out, file);
    if dialect.is_nmake() {
        out.push_str(b" -$(MAKEFLAGS) VERBOSE=$(VERBOSE) ");
    } else {
        out.push(b' ');
    }
    push_escaped(out, goal);
    out.push(b'\n');
}

#[cfg(test)]
mod tests;
