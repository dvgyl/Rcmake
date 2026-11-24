//! Build file generator for Unix Makefiles.

#![forbid(unsafe_code)]

mod build;
mod prep;
mod progress;

pub use build::build_commands;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use rcmake_base::{BString, ByteVec, shell::quote_always};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{Host, WriteMode, path};
use rcmake_model::{DirId, ProjectModel};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult, Invocation};

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
    let dispatch = dispatch(graph, &targets, &prep, report_progress)?;
    let languages = rcmake_resolve::progress::compile_languages(result, graph);
    let build_files = targets
        .iter()
        .map(|target| build_file(target, &languages))
        .collect::<Result<Vec<_>, _>>()?;

    for (target, contents) in targets.iter().zip(build_files) {
        write_makefile(host, &build_dir, &target.file, &contents)?;
    }
    write_makefile(host, &build_dir, MAKEFILE2, &dispatch)?;
    write_makefile(host, &build_dir, b"Makefile", &top)?;
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
}

struct Step<'a> {
    action: &'a BuildStep,
    goal: BString,
    outputs: Vec<BString>,
    inputs: Vec<BString>,
    depfile: Option<BString>,
    always: bool,
    progress: Option<usize>,
    jobserver_aware: bool,
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

fn plan_step<'a>(
    action: &'a BuildStep,
    step_index: usize,
    result: &GenerationResult,
    build_dir: &Path,
) -> Result<Step<'a>, GenError> {
    let make_path = |value: &BString| build_path(value, build_dir);
    let step = match action {
        BuildStep::Compile {
            object,
            source,
            depfile,
            ..
        } => Step {
            action,
            goal: make_path(object),
            outputs: vec![make_path(object)],
            inputs: std::iter::once(source)
                .chain(result.compile_inputs.get(object).into_iter().flatten())
                .map(make_path)
                .collect(),
            depfile: depfile.as_ref().map(make_path),
            always: false,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
        },
        BuildStep::Link { step, inputs, .. } => Step {
            action,
            goal: make_path(&step.output),
            outputs: vec![make_path(&step.output)],
            inputs: inputs.iter().map(make_path).collect(),
            depfile: None,
            always: false,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
        },
        BuildStep::Custom(custom) => Step {
            action,
            goal: custom.outputs.first().map_or_else(
                || format!("CMakeFiles/rcmake-step-{step_index}").into(),
                make_path,
            ),
            outputs: custom
                .outputs
                .iter()
                .chain(&custom.byproducts)
                .map(make_path)
                .collect(),
            inputs: custom.depends.clone(),
            depfile: custom.depfile.as_ref().map(make_path),
            always: custom.outputs.is_empty() || custom.symbolic,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
        },
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => {
            return Err(GenError::Unimplemented(
                "C++ module build steps for Unix Makefiles",
            ));
        }
    };
    Ok(step)
}

fn plan_targets<'a>(
    result: &GenerationResult,
    graph: &'a BuildGraph,
    build_dir: &Path,
    project: Option<&ProjectModel>,
) -> Result<Vec<Target<'a>>, GenError> {
    let mut targets = Vec::with_capacity(graph.targets.len());
    let mut names = BTreeMap::new();
    for (index, target) in graph.targets.iter().enumerate() {
        let name = target.name.as_slice();
        check_make_path(name)?;
        if name.contains(&b'/')
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
        let codegen = target.name == b"codegen"
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
    out.push_str(b"\t$(SILENT)$(CTEST_COMMAND) --force-new-ctest-process\n");

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
) -> Result<Vec<u8>, GenError> {
    let mut out = makefile();
    out.push_str(b".DELETE_ON_ERROR:\n");
    phony(
        &mut out,
        &target.goal,
        target.rules.iter().map(|step| step.goal.as_slice()),
    );
    phony(&mut out, &target.clean_goal, []);
    for product in target.products.iter().collect::<BTreeSet<_>>() {
        out.push_str(b"\t$(SILENT)rm -f -- ");
        make_recipe_text(&mut out, &quote_always(product))?;
        out.push(b'\n');
    }
    phony(&mut out, FORCE, []);
    for step in &target.rules {
        write_step(&mut out, step, &target.file, languages)?;
    }
    Ok(out)
}

fn write_step(
    out: &mut Vec<u8>,
    step: &Step<'_>,
    file: &[u8],
    languages: &std::collections::HashMap<&[u8], &[u8]>,
) -> Result<(), GenError> {
    for path in std::iter::once(&step.goal)
        .chain(&step.inputs)
        .chain(&step.outputs)
        .chain(&step.depfile)
    {
        check_make_path(path)?;
    }
    if step.always {
        phony(
            out,
            &step.goal,
            step.inputs.iter().map(|value| value.as_slice()),
        );
    } else {
        rule(
            out,
            &step.goal,
            step.inputs
                .iter()
                .map(|value| value.as_slice())
                .chain([file]),
        );
    }

    let mut commands = Vec::new();
    let directories = step
        .outputs
        .iter()
        .chain(&step.depfile)
        .filter_map(|output| path::to_native(output).parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    for directory in directories {
        commands.push(
            [
                b"mkdir -p -- ".as_slice(),
                &quote_always(&path::from_native(&directory)),
            ]
            .concat(),
        );
    }
    let (message, invocations) = match step.action {
        BuildStep::Compile {
            object, invocation, ..
        } => (
            rcmake_resolve::progress::compile_description(
                languages.get(object.as_slice()).copied(),
                &step.goal,
            ),
            std::slice::from_ref(invocation),
        ),
        BuildStep::Link {
            step: link,
            invocations,
            ..
        } => (
            rcmake_resolve::progress::link_description(link, &step.goal),
            invocations.as_slice(),
        ),
        BuildStep::Custom(custom) => (
            custom
                .comment
                .as_ref()
                .map_or_else(Vec::new, |comment| comment.to_vec()),
            custom.invocations.as_slice(),
        ),
        // Step collection rejects module steps before recipes are written.
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => (Vec::new(), [].as_slice()),
    };
    if !message.is_empty() {
        progress::report(out, &message, step.progress)?;
    }
    if step.jobserver_aware {
        for command in commands {
            out.push_str(b"\t$(SILENT)");
            make_recipe_text(out, &command)?;
            out.push(b'\n');
        }
        for invocation in invocations {
            if invocation
                .argv
                .first()
                .is_some_and(|program| program == b"$(CMAKE_COMMAND)")
            {
                out.push_str(b"\t@+");
                if let Some(directory) = &invocation.working_dir {
                    out.push_str(b"cd ");
                    make_recipe_text(out, &quote_always(directory))?;
                    out.push_str(b" && ");
                }
                out.push_str(b"$(CMAKE_COMMAND)");
                for arg in invocation.argv.iter().skip(1) {
                    out.push(b' ');
                    make_recipe_text(out, &rcmake_base::shell::quote(arg))?;
                }
                out.push(b'\n');
            } else if let Some(command) = shell_invocation(invocation) {
                out.push_str(b"\t+$(SILENT)");
                make_recipe_text(out, &command)?;
                out.push(b'\n');
            }
        }
    } else if let commands = commands
        .into_iter()
        .chain(invocations.iter().filter_map(shell_invocation))
        .collect::<Vec<_>>()
        && !commands.is_empty()
    {
        out.push_str(b"\t$(SILENT)");
        for (index, command) in commands.iter().enumerate() {
            if index != 0 {
                out.push_str(b" && \\\n\t  ");
            }
            make_recipe_text(out, command)?;
        }
        out.push(b'\n');
    }

    for output in step.outputs.iter().filter(|output| **output != step.goal) {
        if !step.always {
            // Older GNU Make needs one recipe to own all outputs.
            out.push_str(b"ifeq ($(wildcard ");
            make_word(out, output, Word::Wildcard);
            out.push_str(b"),)\n");
            rule(out, &step.goal, [FORCE]);
            out.push_str(b"endif\n");
        }
        rule(out, output, [step.goal.as_slice()]);
        // A recipe makes Make read the new file timestamp after the producer runs.
        out.push_str(b"\t$(SILENT)test -e ");
        let output = quote_always(output);
        make_recipe_text(out, &output)?;
        out.push_str(b" || test -L ");
        make_recipe_text(out, &output)?;
        out.push(b'\n');
    }
    if let Some(depfile) = &step.depfile {
        out.push_str(b"-include ");
        make_word(out, depfile, Word::Input);
        out.push(b'\n');
    }
    Ok(())
}

fn shell_invocation(invocation: &Invocation) -> Option<Vec<u8>> {
    if invocation.shell.is_empty() && invocation.argv.is_empty() {
        return None;
    }
    let mut command = b"(".to_vec();
    if let Some(directory) = &invocation.working_dir {
        command.push_str(b"CDPATH= cd ");
        if directory.starts_with(b"-") {
            command.push_str(quote_always(&[b"./", directory.as_slice()].concat()));
        } else {
            command.push_str(quote_always(directory));
        }
        command.push_str(b" && ");
    }
    if invocation.shell.is_empty() {
        for (index, arg) in invocation.argv.iter().enumerate() {
            if index != 0 {
                command.push(b' ');
            }
            command.push_str(quote_always(arg));
        }
    } else {
        command.push_str(&invocation.shell);
    }
    command.push(b')');
    Some(command)
}

fn make_recipe_text(out: &mut Vec<u8>, text: &[u8]) -> Result<(), GenError> {
    for &byte in text {
        match byte {
            b'\n' | b'\r' | b'\0' => return Err(invalid_graph("invalid byte in recipe")),
            b'$' => out.push_str(b"$$"),
            _ => out.push(byte),
        }
    }
    Ok(())
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
