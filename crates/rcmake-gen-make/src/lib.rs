//! Build file generator for Unix Makefiles.

#![forbid(unsafe_code)]

mod build;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
mod cuda;
mod directory_info;
#[allow(
    dead_code,
    reason = "The language activation wave uses this emitter or planner"
)]
mod fortran;
mod plan_step;
mod prep;
mod progress;
mod recipe;
mod response;
use recipe::{make_recipe_text, shell_invocation, write_step};

pub use build::build_commands;
use plan_step::plan_step;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use rcmake_base::{BString, ByteVec, shell::quote_always};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_gen_common::{GenError, write_if_changed};
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

/// Write the top-level `Makefile`, `CMakeFiles/Makefile2`, and per-target build files.
///
/// Use the first graph because Unix Makefiles supports one configuration.
pub fn generate_unix_makefiles(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
) -> Result<(), GenError> {
    generate_impl(result, host, build_dir, None)
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
    generate_impl(result, host, build_dir, Some(model))
}

fn generate_impl(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
) -> Result<(), GenError> {
    let build_dir = host.initial_cwd().join(build_dir);
    let empty = BuildGraph::default();
    let graph = result.graphs.first().unwrap_or(&empty);
    let mut targets = plan_targets(result, graph, &build_dir, model)?;
    response::prepare(&mut targets, &graph.config.0, &build_dir, host)?;
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
            .map(|exe| path::from_native(&exe))
            .map_err(|error| GenError::Write {
                path: "Makefile".into(),
                message: error.to_string().into(),
            })?,
    };
    let prep = prep::targets(result, graph, &targets, &build_dir, model)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, &build_dir, host)?;
    let top = top_level(
        result,
        graph,
        &targets,
        &cmake,
        &prep,
        report_progress,
        verify_globs,
    )?;
    let dispatch = dispatch(
        graph,
        &targets,
        &prep,
        report_progress && target_messages(model),
        &result.metadata.clean_files,
    )?;
    let languages = rcmake_resolve::progress::compile_languages(result, graph);
    let build_files = targets
        .iter()
        .map(|target| build_file(target, &languages, &build_dir, &cmake, host))
        .collect::<Result<Vec<_>, _>>()?;

    for (target, contents) in targets.iter().zip(build_files) {
        if let Some(fortran) = &target.fortran {
            fortran.write(host, &build_dir, target)?;
        }
        write_makefile(host, &build_dir, &target.file, &contents)?;
    }
    write_makefile(host, &build_dir, MAKEFILE2, &dispatch)?;
    write_makefile(host, &build_dir, b"Makefile", &top)?;
    if let Some(model) = model {
        directory_info::write(host, model)?;
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
        check_make_path(name)?;
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
            let mut step = plan_step(action, step_index, result, build_dir)?;
            if matches!(action, BuildStep::Link { .. })
                && let Some(artifacts) = artifacts.get(target.name)
            {
                let make_path = |value: &BString| build_path(value, build_dir);
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

            let mut seen = BTreeSet::new();
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
        let own_outputs: BTreeSet<_> = target
            .rules
            .iter()
            .flat_map(|rule| rule.outputs.iter().cloned())
            .collect();
        for step in &mut target.rules {
            let mut inputs = BTreeSet::new();
            for input in &step.inputs {
                let file = if matches!(step.action, BuildStep::Custom(_))
                    && let Some(&dependency) = names.get(input.as_slice())
                {
                    if dependency != index {
                        target.dependencies.insert(dependency);
                    }
                    let Some(output) = link_outputs.get(input.as_slice()) else {
                        continue;
                    };
                    output.clone()
                } else {
                    build_path(input, build_dir)
                };
                if let Some(owner) = producers.get(&file)
                    && *owner != index
                    && !own_outputs.contains(&file)
                {
                    target.dependencies.insert(*owner);
                    if symbolic.contains(&file) && !own_outputs.contains(&file) {
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
    let mut visited = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if index == dependency {
            return true;
        }
        if visited.insert(index)
            && let Some(target) = graph.targets.get(index)
        {
            pending.extend(&target.depends);
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
    if let Some(parent) = destination.parent() {
        host.create_dir_all(parent)
            .map_err(|error| GenError::Write {
                path: path::from_native(parent),
                message: error.to_string().into(),
            })?;
    }
    write_if_changed(host, &destination, contents)
}

fn makefile() -> Vec<u8> {
    b"# Generated by Rcmake.\n.SUFFIXES:\nSHELL = /bin/sh\n\
SILENT = $(SILENT_$(VERBOSE))\nSILENT_ = @\n\
MAKE_SILENT = $(MAKE_SILENT_$(VERBOSE))\nMAKE_SILENT_ = -s\n\n"
        .to_vec()
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

fn top_level(
    result: &GenerationResult,
    graph: &BuildGraph,
    targets: &[Target<'_>],
    cmake: &[u8],
    prep: &prep::Targets,
    report_progress: bool,
    verify_globs: bool,
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile();
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
    phony(&mut out, b"all", [b"cmake_check_build_system".as_slice()]);
    let all = graph
        .targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| target.in_all.then_some(index));
    progress::build(
        &mut out,
        b"all",
        report_progress.then(|| progress::count(targets, all)),
    );
    phony(&mut out, b"clean", []);
    recursive_make(&mut out, MAKEFILE2, b"clean");
    phony(&mut out, b"preinstall", [b"all".as_slice()]);
    recursive_make(&mut out, MAKEFILE2, b"preinstall");
    phony(&mut out, b"install", [b"preinstall".as_slice()]);
    out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -P cmake_install.cmake\n");
    phony(&mut out, b"test", []);
    out.push_str(b"\t$(SILENT)$(CTEST_COMMAND) --force-new-ctest-process");
    for argument in &result.metadata.ctest_arguments {
        out.push(b' ');
        make_recipe_text(&mut out, &quote_always(argument))?;
    }
    out.push(b'\n');

    let mut configure = b"$(CMAKE_COMMAND) -B .".to_vec();
    let listfiles = &result.metadata.listfiles;
    if let Some(source_dir) = listfiles.iter().find_map(|file| {
        let file = path::to_native(file);
        (file.file_name()? == "CMakeLists.txt")
            .then(|| path::from_native(file.parent().unwrap_or(Path::new("."))))
    }) {
        configure.push_str(b" -S ");
        make_recipe_text(&mut configure, &quote_always(&source_dir))?;
    }
    phony(&mut out, b"rebuild_cache", []);
    out.push_str(b"\t$(SILENT)");
    out.push_str(&configure);
    out.push(b'\n');

    if listfiles.is_empty() {
        phony(&mut out, b"cmake_check_build_system", []);
    } else {
        for file in listfiles {
            check_make_path(file)?;
        }
        phony(&mut out, b"cmake_check_build_system", [REGEN_STAMP]);
        if verify_globs {
            let script = rcmake_gen_common::glob::SCRIPT.as_bytes();
            let marker = b"CMakeFiles/rcmake-glob-mismatch";
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
        rule(
            &mut out,
            REGEN_STAMP,
            listfiles.iter().map(|value| value.as_slice()),
        );
        out.push_str(b"\t$(SILENT)");
        out.push_str(&configure);
        out.push_str(b" --regenerate-during-build && touch ");
        out.push_str(REGEN_STAMP);
        out.push(b'\n');
        for input in listfiles {
            // A removed include must trigger configure again.
            rule(&mut out, input, []);
        }
    }

    phony(&mut out, b"help", []);
    for name in GLOBAL_TARGETS
        .iter()
        .copied()
        .chain(targets.iter().map(|target| target.name))
        .chain(prep.keys().map(|name| name.as_slice()))
    {
        out.push_str(b"\t@printf '%s\\n' ");
        make_recipe_text(&mut out, &quote_always(&[b"... ", name].concat()))?;
        out.push(b'\n');
    }
    for (index, target) in targets.iter().enumerate() {
        phony(
            &mut out,
            target.name,
            [b"cmake_check_build_system".as_slice()],
        );
        progress::build(
            &mut out,
            &target.goal,
            report_progress.then(|| progress::count(targets, [index])),
        );
    }
    for name in prep.keys() {
        phony(&mut out, name, [b"cmake_check_build_system".as_slice()]);
        recursive_make(&mut out, MAKEFILE2, name);
    }
    Ok(out)
}

fn dispatch(
    graph: &BuildGraph,
    targets: &[Target<'_>],
    prep: &prep::Targets,
    report_progress: bool,
    clean_paths: &[BString],
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile();
    phony(
        &mut out,
        b"all",
        graph
            .targets
            .iter()
            .zip(targets)
            .filter(|(model, _)| model.in_all)
            .map(|(_, target)| target.goal.as_slice()),
    );
    phony(
        &mut out,
        b"clean",
        targets.iter().map(|target| target.clean_goal.as_slice()),
    );
    for path in clean_paths {
        out.push_str(b"	$(SILENT)rm -rf -- ");
        make_recipe_text(&mut out, &quote_always(path))?;
        out.push(b'\n');
    }
    phony(&mut out, b"preinstall", []);
    for target in targets {
        phony(
            &mut out,
            &target.goal,
            target
                .dependencies
                .iter()
                .filter_map(|&index| targets.get(index))
                .map(|target| target.goal.as_slice()),
        );
        if let Some(fortran) = &target.fortran {
            recursive_make(&mut out, &target.file, &fortran.scan_goal);
        }
        recursive_make(&mut out, &target.file, &target.goal);
        if report_progress {
            progress::complete(&mut out, target)?;
        }
        phony(&mut out, &target.clean_goal, []);
        recursive_make(&mut out, &target.file, &target.clean_goal);
    }
    for (name, dependencies) in prep {
        phony(
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
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile();
    out.push_str(b".DELETE_ON_ERROR:\n");
    phony(
        &mut out,
        &target.goal,
        target.rules.iter().map(|step| step.goal.as_slice()),
    );
    phony(&mut out, &target.clean_goal, []);
    if let Some(fortran) = &target.fortran {
        fortran.emit_clean(&mut out, cmake)?;
    }
    for product in target.products.iter().collect::<BTreeSet<_>>() {
        out.push_str(b"\t$(SILENT)rm -f -- ");
        make_recipe_text(&mut out, &quote_always(product))?;
        out.push(b'\n');
    }
    phony(&mut out, FORCE, []);
    if let Some(fortran) = &target.fortran {
        fortran.emit(&mut out, target, cmake, build_dir)?;
    }
    for step in &target.rules {
        if cuda::emit(
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
        cuda::emit_link_depends(&mut out, build_dir, step.action)?;
        write_step(&mut out, step, &target.file, languages)?;
    }
    Ok(out)
}

fn check_make_path(value: &[u8]) -> Result<(), GenError> {
    if value.is_empty()
        || value.iter().any(|byte| {
            matches!(
                byte,
                b'\n' | b'\r' | b'\0' | b';' | b'=' | b'|' | b'\\' | b'(' | b')'
            )
        })
    {
        return Err(invalid_graph(format!(
            "unsupported make path: {}",
            BString::from(value)
        )));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Word {
    Target,
    Input,
    Wildcard,
}

fn make_word(out: &mut Vec<u8>, value: &[u8], word: Word) {
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

fn rule<'a>(out: &mut Vec<u8>, goal: &[u8], inputs: impl IntoIterator<Item = &'a [u8]>) {
    make_word(out, goal, Word::Target);
    out.push(b':');
    for input in inputs {
        out.push(b' ');
        make_word(out, input, Word::Input);
    }
    out.push(b'\n');
}

fn phony<'a>(out: &mut Vec<u8>, goal: &[u8], inputs: impl IntoIterator<Item = &'a [u8]>) {
    rule(out, b".PHONY", [goal]);
    rule(out, goal, inputs);
}

fn recursive_make(out: &mut Vec<u8>, file: &[u8], goal: &[u8]) {
    out.push_str(b"\t$(SILENT)$(MAKE) $(MAKE_SILENT) -f ");
    for (index, part) in [file, goal].into_iter().enumerate() {
        if index != 0 {
            out.push(b' ');
        }
        for &byte in quote_always(part).iter() {
            if byte == b'$' {
                out.push(b'$');
            }
            out.push(byte);
        }
    }
    out.push(b'\n');
}

#[cfg(test)]
mod tests;
