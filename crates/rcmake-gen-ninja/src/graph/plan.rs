use std::borrow::Cow;
use std::collections::HashSet;

use rcmake_base::BString;
use rcmake_gen_common::{GenError, export::ModuleExportJob};
use rcmake_resolve::{BuildStep, DependInfo, LinkFragmentRole, LinkKind, progress};

use super::{Graph, StepNodes, is_executable_symlink, is_library_symlink};
use crate::response::{COMPILE_MSVC, Response};

pub(super) struct EdgePlan {
    pub(super) rule: Cow<'static, [u8]>,
    pub(super) inputs: Vec<BString>,
    pub(super) implicit_inputs: Vec<BString>,
    pub(super) order_only: Vec<BString>,
    pub(super) implicit_outputs: Vec<BString>,
    pub(super) dyndep: Option<BString>,
    pub(super) bindings: Vec<(&'static [u8], BString)>,
}

impl EdgePlan {
    fn new(
        rule: impl Into<Cow<'static, [u8]>>,
        inputs: Vec<BString>,
        nodes: &StepNodes<'_>,
    ) -> Self {
        Self {
            rule: rule.into(),
            inputs,
            implicit_inputs: Vec::new(),
            order_only: Vec::new(),
            implicit_outputs: nodes.byproducts.clone(),
            dyndep: None,
            bindings: Vec::new(),
        }
    }

    pub(super) fn bind_command(&mut self, command: Vec<u8>, response: Option<Response>) {
        if let Some(response) = response {
            self.rule = Cow::Borrowed(response.rule);
            self.bind(b"COMMAND", response.command);
            self.bind(b"RSP_FILE", response.file);
            self.bind(b"RSP_ARGS", response.contents);
        } else {
            self.bind(b"COMMAND", command);
        }
    }

    pub(super) fn bind(&mut self, key: &'static [u8], value: impl Into<BString>) {
        self.bindings.push((key, value.into()));
    }
}

impl<'a> Graph<'a> {
    pub(super) fn plan_step(
        &self,
        step_index: usize,
        step: &BuildStep,
        nodes: &StepNodes<'_>,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
    ) -> Result<EdgePlan, GenError> {
        if let Some(action) = self.input.swift_actions.get(&step_index) {
            let invocations = match step {
                BuildStep::Compile { invocation, .. } => std::slice::from_ref(invocation),
                BuildStep::Custom(custom) => custom.invocations.as_slice(),
                _ => &[],
            };
            if let Some(primary_invocation) = invocations.first() {
                let inputs: Vec<BString> =
                    action.sources.iter().map(|src| self.node(src)).collect();
                let mut launched = None;
                let (rule, special_command): (Cow<'static, [u8]>, Option<Vec<u8>>) =
                    if invocations.len() == 1 {
                        let (rule, command) = crate::instrumented::compile(
                            primary_invocation,
                            false,
                            false,
                            false,
                            &self.commands,
                        );
                        if command.is_some() {
                            (Cow::Borrowed(rule), command)
                        } else if let Some(info) = crate::instrumented::launched(
                            primary_invocation,
                            action
                                .outputs
                                .first()
                                .and_then(|out| self.input.compile_launchers.get(out.as_slice())),
                            &self.launcher_slots,
                            false,
                            false,
                            false,
                            &self.commands,
                        ) {
                            let rule = info.rule.clone();
                            let cmd = info.command.clone();
                            launched = Some(info);
                            (rule, Some(cmd))
                        } else {
                            (Cow::Borrowed(rule), None)
                        }
                    } else {
                        (Cow::Borrowed(b"COMPILE".as_slice()), None)
                    };
                let mut plan = EdgePlan::new(rule, inputs, nodes);
                plan.implicit_inputs.clear();
                plan.implicit_inputs
                    .extend(action.implicit_inputs.iter().map(|path| self.node(path)));
                for output in &action.outputs {
                    plan.implicit_inputs.extend(
                        self.compile_inputs
                            .and_then(|inputs| inputs.get(output))
                            .into_iter()
                            .flatten()
                            .map(|input| self.node(input)),
                    );
                }
                // Serialize split object compiles after emit-module (which shares
                // `output-file-map.json` `.swiftdeps`) alongside order.rs prerequisites.
                plan.order_only
                    .extend(action.order_only_inputs.iter().map(|path| self.node(path)));
                plan.implicit_outputs.clear();
                let ordinary_command = self.command_line(invocations, false);
                let response = self.swift_response(action, invocations, &ordinary_command);
                if response.is_some() {
                    plan.bind_command(ordinary_command, response);
                } else {
                    let command = special_command.map_or(ordinary_command, |command| {
                        if launched.is_some() {
                            command
                        } else {
                            self.in_build_dir(&command).into()
                        }
                    });
                    if let Some(info) = launched {
                        plan.bind(b"LAUNCH_ARGS", info.launch_args);
                        if let Some(launcher) = info.launcher {
                            plan.bind(b"LAUNCHER", launcher);
                        }
                    }
                    plan.bind(b"COMMAND", command);
                }
                plan.bind(b"DESC", action.description.clone());
                if action.restat {
                    plan.bind(b"restat", b"1");
                }
                return Ok(plan);
            }
        }
        let plan = match step {
            BuildStep::Compile {
                object,
                source,
                compile_input,
                invocation,
                depfile,
                modmap,
                dyndep,
                ..
            } => {
                let scan = self.resource_dependency_scan(object, source, invocation);
                let msvc = scan.is_some() || self.msvc_deps(object, invocation);
                let scanned = dyndep.is_some() || modmap.is_some();
                let mut launched = None;
                let (rule, command): (Cow<'static, [u8]>, Option<Vec<u8>>) = if scan.is_some() {
                    (Cow::Borrowed(COMPILE_MSVC), None)
                } else {
                    let (rule, command) = crate::instrumented::compile(
                        invocation,
                        scanned,
                        depfile.is_some(),
                        msvc,
                        &self.commands,
                    );
                    if command.is_some() {
                        (Cow::Borrowed(rule), command)
                    } else if let Some(info) = crate::instrumented::launched(
                        invocation,
                        self.input.compile_launchers.get(object.as_slice()),
                        &self.launcher_slots,
                        scanned,
                        depfile.is_some(),
                        msvc,
                        &self.commands,
                    ) {
                        let rule = info.rule.clone();
                        let cmd = info.command.clone();
                        launched = Some(info);
                        (rule, Some(cmd))
                    } else if let Some(synth_rule) = self
                        .step_owner
                        .get(&step_index)
                        .and_then(|&owner| self.input.targets.get(owner))
                        .and_then(|target| {
                            crate::modules::synthetic_compile_rule_name(&target.name, self.config)
                        })
                    {
                        (Cow::Owned(synth_rule.into()), None)
                    } else if msvc {
                        (Cow::Borrowed(COMPILE_MSVC), None)
                    } else {
                        (Cow::Borrowed(rule), None)
                    }
                };
                let response_rule_name = rule.clone();
                let raw_source = compile_input.as_ref().unwrap_or(source);
                let source_node = if self.assumed_sources.contains(raw_source) {
                    self.commands.path(raw_source)
                } else {
                    self.node(raw_source)
                };
                let mut plan = EdgePlan::new(rule, vec![source_node], nodes);
                plan.implicit_inputs
                    .extend(modmap.iter().map(|map| self.node(map)));
                plan.implicit_inputs.extend(
                    self.compile_inputs
                        .and_then(|inputs| inputs.get(object))
                        .into_iter()
                        .flatten()
                        .map(|input| self.node(input)),
                );
                let command = command.map_or_else(
                    || {
                        if let Some(scan) = scan {
                            self.command_line(&[scan, invocation.clone()], false)
                        } else {
                            self.command_line(std::slice::from_ref(invocation), false)
                        }
                    },
                    |command| {
                        if launched.is_some() {
                            command
                        } else {
                            self.in_build_dir(&command).into()
                        }
                    },
                );
                if let Some(info) = launched {
                    plan.bind(b"LAUNCH_ARGS", info.launch_args);
                    if let Some(launcher) = info.launcher {
                        plan.bind(b"LAUNCHER", launcher);
                    }
                }
                let response =
                    self.compile_response(&response_rule_name, object, invocation, &command);
                plan.bind_command(command, response);
                if let Some(output) = nodes.outputs.first() {
                    plan.bind(
                        b"DESC",
                        progress::compile_description(
                            self.compile_languages.get(object.as_slice()).copied(),
                            output,
                        ),
                    );
                }
                if let Some(pool) = self.pools.get(object) {
                    plan.bind(b"pool", pool.clone());
                }
                if let Some(depfile) = depfile.as_ref().filter(|_| !msvc) {
                    plan.bind(b"DEP_FILE", self.node(depfile));
                }
                if let Some(map) = modmap {
                    plan.bind(b"DYNDEP_MODULE_MAP_FILE", self.node(map));
                }
                if let Some(dd) = dyndep {
                    let node = self.node(dd);
                    plan.bind(b"dyndep", node.clone());
                    plan.dyndep = Some(node);
                }
                plan
            }
            BuildStep::Scan {
                source,
                ddi,
                object,
                invocation,
                depfile,
                ..
            } => {
                let response = self
                    .scan_responses
                    .get(ddi.as_slice())
                    .and_then(|response| {
                        if self.commands.windows() {
                            let argv = rcmake_resolve::lower::rule_arguments(&response.arguments)?;
                            Some((response, self.commands.argv(&argv).into()))
                        } else {
                            Some((response, response.arguments.clone()))
                        }
                    });
                let rule: &[u8] = match (depfile.is_some(), response.is_some()) {
                    (true, true) => b"CXX_SCAN_RSP",
                    (true, false) => b"CXX_SCAN",
                    (false, true) => b"CXX_SCAN_RSP_NO_DEPFILE",
                    (false, false) => b"CXX_SCAN_NO_DEPFILE",
                };
                let source_node = if self.assumed_sources.contains(source) {
                    self.commands.path(source)
                } else {
                    self.node(source)
                };
                let mut plan = EdgePlan::new(rule, vec![source_node], nodes);
                plan.implicit_inputs.extend(
                    self.compile_inputs
                        .and_then(|inputs| inputs.get(object))
                        .into_iter()
                        .flatten()
                        .map(|input| self.node(input)),
                );
                let invocation = response
                    .as_ref()
                    .map_or(invocation, |(response, _)| &response.invocation);
                plan.bind(
                    b"COMMAND",
                    self.command_line(std::slice::from_ref(invocation), false),
                );
                if let Some((_, arguments)) = response {
                    let rsp = [self.node(ddi).as_slice(), b".rsp"].concat();
                    plan.bind(b"RSP_FILE", rsp);
                    plan.bind(b"RSP_ARGS", arguments);
                }
                if let Some(depfile) = depfile {
                    plan.bind(b"DEP_FILE", self.node(depfile));
                }
                plan
            }
            BuildStep::Collate {
                dd,
                ddis,
                tdi,
                implicit_inputs,
                depend_info,
                ..
            } => {
                let DependInfo::Cxx(depend_info) = depend_info.as_ref() else {
                    return Err(GenError::Unimplemented("Fortran collation for Ninja"));
                };
                let inputs = ddis.iter().map(|ddi| self.node(ddi)).collect();
                let rule: &[u8] = if self.commands.windows() {
                    b"CXX_DYNDEP_WINDOWS"
                } else {
                    b"CXX_DYNDEP"
                };
                let mut plan = EdgePlan::new(rule, inputs, nodes);
                plan.implicit_outputs.extend(
                    depend_info
                        .bmi_installation
                        .iter()
                        .filter(|install| !install.script_location.is_empty())
                        .map(|install| self.node(&install.script_location)),
                );
                let tdi_node = self.node(tdi);
                plan.implicit_inputs.push(tdi_node.clone());
                plan.implicit_inputs
                    .extend(implicit_inputs.iter().map(|input| self.node(input)));
                let mut command = cmake.to_vec();
                if self.commands.windows() {
                    command.extend_from_slice(b" -E cmake_ninja_dyndep ");
                    self.commands
                        .word(&[b"--tdi=", tdi.as_slice()].concat(), &mut command);
                    command.extend_from_slice(b" --lang=CXX ");
                    command.extend_from_slice(modmap_flag);
                    command.push(b' ');
                    self.commands
                        .word(&[b"--dd=", dd.as_slice()].concat(), &mut command);
                    command.push(b' ');
                    self.commands
                        .word(&[b"@", dd.as_slice(), b".rsp"].concat(), &mut command);
                } else {
                    let mut tdi_arg = Vec::new();
                    self.commands.word(&tdi_node, &mut tdi_arg);
                    plan.bind(b"CMAKE_COMMAND", cmake);
                    plan.bind(b"in_tdi", tdi_arg);
                    plan.bind(b"MODMAP_FLAG", modmap_flag);
                }
                let mut export_commands = Vec::new();
                for export in exports.iter().filter(|export| export.dyndep == *dd) {
                    let script_node = self.node(&export.script);
                    plan.implicit_outputs.push(self.node(&export.output));
                    export_commands.extend_from_slice(b" && ");
                    export_commands.extend_from_slice(cmake);
                    export_commands.extend_from_slice(b" -P ");
                    self.commands.word(
                        if self.commands.windows() {
                            &export.script
                        } else {
                            &script_node
                        },
                        &mut export_commands,
                    );
                    plan.implicit_inputs.push(script_node);
                }
                if self.commands.windows() {
                    command.extend_from_slice(&export_commands);
                    let command = if !self.prefix.is_empty() {
                        self.in_build_dir(&command).into()
                    } else if export_commands.is_empty() {
                        command
                    } else {
                        self.commands.cmd(&command)
                    };
                    plan.bind(b"COMMAND", command);
                } else if !export_commands.is_empty() {
                    plan.bind(b"MODULE_EXPORT_COMMANDS", export_commands);
                }
                plan
            }
            BuildStep::DeviceLink { .. } => {
                return Err(GenError::Unimplemented("device linking for Ninja"));
            }
            BuildStep::Link {
                inputs,
                implicit_inputs,
                step,
                invocations,
            } => {
                let invocations = self
                    .exe_symlinks
                    .get(&step_index)
                    .and_then(|sym| invocations.get(..sym.split_at))
                    .unwrap_or(invocations.as_slice());
                let mut explicit_inputs = Vec::new();
                let mut lib_implicit = Vec::new();
                if step.kind == LinkKind::StaticArchive {
                    explicit_inputs.extend(inputs.iter().map(|input| self.node(input)));
                } else {
                    for input in inputs {
                        let node = self.node(input);
                        if self.is_link_library_input(&node, step) {
                            lib_implicit.push(node);
                        } else {
                            explicit_inputs.push(node);
                        }
                    }
                }
                let depfile = self.input.link_depfiles.get(&step.output);
                let rule = depfile.map_or_else(|| step.kind.ninja_rule(), |_| b"LINK_DEPFILE");
                let mut plan = EdgePlan::new(rule, explicit_inputs, nodes);
                if let Some(event_byproducts) = self.input.link_event_byproducts.get(&step.output)
                    && !event_byproducts.is_empty()
                {
                    for byproduct in event_byproducts {
                        let node = self.node(byproduct);
                        if let Some(alias) = self.absolute_alias(&node)
                            && !plan.implicit_outputs.contains(&alias)
                        {
                            plan.implicit_outputs.push(alias);
                        }
                    }
                    plan.bind(b"restat", b"1".as_slice());
                }
                let no_shared = step.kind != LinkKind::StaticArchive
                    && self.step_link_depends_no_shared(step_index);
                let explicit_link_depends: HashSet<BString> = if no_shared {
                    implicit_inputs
                        .iter()
                        .map(|input| self.node(input))
                        .collect()
                } else {
                    HashSet::new()
                };
                for node in lib_implicit {
                    if !(no_shared
                        && self.is_shared_library_input(&node)
                        && !explicit_link_depends.contains(&node))
                    {
                        plan.implicit_inputs.push(node);
                    }
                }
                for input in implicit_inputs {
                    let node = self.node(input);
                    if !plan.implicit_inputs.contains(&node) {
                        plan.implicit_inputs.push(node);
                    }
                }
                if step.kind != LinkKind::StaticArchive {
                    let mut seen: Option<HashSet<BString>> = None;
                    for frag in &step.fragments {
                        if frag.role != LinkFragmentRole::Libraries {
                            continue;
                        }
                        let token = frag.text.value.as_slice();
                        if self.is_absolute_library_fragment(token) {
                            let seen = seen.get_or_insert_with(|| {
                                plan.implicit_inputs
                                    .iter()
                                    .chain(&plan.order_only)
                                    .cloned()
                                    .collect()
                            });
                            let node = self.node(token);
                            if seen.insert(node.clone())
                                && !(no_shared
                                    && self.is_shared_library_input(&node)
                                    && !explicit_link_depends.contains(&node))
                            {
                                plan.implicit_inputs.push(node);
                            }
                        }
                    }
                }
                if let Some(prepared) = self.prepared_link(nodes) {
                    prepared.bind(&mut plan);
                } else {
                    let command = self.link_command_line(invocations, false);
                    let response = self.link_response(rule, step, inputs, invocations, &command);
                    plan.bind_command(command, response);
                }
                if let Some(depfile) = depfile {
                    plan.bind(b"DEP_FILE", self.node(depfile));
                }
                if let Some(pool) = self.pools.get(&step.output) {
                    plan.bind(b"pool", pool.clone());
                }
                if let Some(output) = nodes.outputs.first() {
                    let description = self
                        .input
                        .link_descriptions
                        .get(&step.output)
                        .cloned()
                        .unwrap_or_else(|| progress::link_description(step, output).into());
                    plan.bind(b"DESC", description);
                }
                if let Some(directory) = self.input.install_name_dirs.get(&step.output) {
                    plan.bind(b"INSTALLNAME_DIR", directory.clone());
                }
                plan
            }
            BuildStep::Custom(custom) => {
                let lib_symlink = is_library_symlink(custom);
                let exe_symlink = is_executable_symlink(custom);
                let is_symlink = lib_symlink || exe_symlink;
                let is_util_output = self.input.utility_steps.contains(&step_index)
                    && nodes
                        .outputs
                        .first()
                        .is_some_and(|output| output.ends_with(b".util"));
                let resolved = custom
                    .depends
                    .iter()
                    .map(|input| self.custom_input(input))
                    .collect::<Vec<_>>();
                // CMake also lists the absolute path of each file in the build tree,
                // so a depfile that names the absolute path finds this edge.
                // Symlink rules, `.util` utility outputs, and link outputs do not get
                // workdir aliases; byproducts on those edges still do.
                let aliases = if is_symlink {
                    Vec::new()
                } else {
                    nodes
                        .outputs
                        .iter()
                        .enumerate()
                        .filter(|(index, node)| {
                            !(is_util_output && node.ends_with(b".util"))
                                && !(nodes.link && *index == 0)
                        })
                        .map(|(_, node)| node)
                        .chain(&nodes.byproducts)
                        .filter_map(|node| self.absolute_alias(node))
                        .collect::<Vec<_>>()
                };
                if custom.invocations.is_empty() {
                    if self.input.utility_steps.contains(&step_index) || custom.outputs.is_empty() {
                        let (targets, files): (Vec<_>, Vec<_>) =
                            resolved.into_iter().partition(|input| input.target_only);
                        let inputs = files.into_iter().map(|input| input.node).collect();
                        let mut plan = EdgePlan::new(b"phony", inputs, nodes);
                        plan.order_only = targets.into_iter().map(|input| input.node).collect();
                        plan.implicit_outputs.extend(aliases);
                        return Ok(plan);
                    }
                    let inputs = resolved.into_iter().map(|input| input.node).collect();
                    let mut plan = EdgePlan::new(b"phony", inputs, nodes);
                    plan.implicit_outputs.extend(aliases);
                    return Ok(plan);
                }
                // A target without a file only orders the command, as in CMake.
                let (targets, files): (Vec<_>, Vec<_>) =
                    resolved.into_iter().partition(|input| input.target_only);
                let inputs = files.into_iter().map(|input| input.node).collect();
                let rule = if lib_symlink {
                    b"CMAKE_SYMLINK_LIBRARY".as_slice()
                } else if exe_symlink {
                    b"CMAKE_SYMLINK_EXECUTABLE".as_slice()
                } else {
                    b"CUSTOM_COMMAND"
                };
                let mut plan = EdgePlan::new(rule, inputs, nodes);
                let symbolic = custom.symbolic
                    || !custom.symbolic_outputs.is_empty()
                    || self.input.utility_steps.contains(&step_index)
                    || custom.outputs.is_empty();
                if rule == b"CUSTOM_COMMAND"
                    && (!symbolic || is_util_output || !custom.byproducts.is_empty())
                {
                    plan.bind(b"restat", b"1".as_slice());
                }
                plan.order_only = targets.into_iter().map(|input| input.node).collect();
                plan.implicit_outputs.extend(aliases);
                let script = nodes
                    .outputs
                    .first()
                    .and_then(|output| self.custom_scripts.get(output));
                let command = script.map_or_else(
                    || {
                        if lib_symlink {
                            self.commands.symlink_library_line(
                                &custom.invocations,
                                nodes.outputs.len(),
                                &rcmake_host::path::from_native(self.build_dir),
                                !self.prefix.is_empty(),
                            )
                        } else if exe_symlink {
                            self.commands.symlink_executable_line(
                                &custom.invocations,
                                &rcmake_host::path::from_native(self.build_dir),
                                !self.prefix.is_empty(),
                            )
                        } else if nodes.link {
                            self.link_command_line(&custom.invocations, false)
                        } else if is_util_output {
                            self.utility_command_line(&custom.invocations)
                        } else {
                            self.command_line(&custom.invocations, true)
                        }
                    },
                    |script| {
                        let mut argument = b"\"".to_vec();
                        argument.extend_from_slice(script);
                        argument.push(b'"');
                        self.commands.cmd(&argument)
                    },
                );
                if let Some(prepared) = self.prepared_link(nodes) {
                    prepared.bind(&mut plan);
                } else {
                    plan.bind(b"COMMAND", command);
                }
                if let Some(script) = script {
                    plan.implicit_inputs.push(script.clone());
                }
                let link_depfile = custom
                    .outputs
                    .first()
                    .and_then(|output| self.input.link_depfiles.get(output));
                if let Some(depfile) = custom.depfile.as_ref().or(link_depfile) {
                    let depfile = self.node(depfile);
                    // Ninja removes the depfile when it records dependencies in its database.
                    let persistent =
                        nodes.outputs.contains(&depfile) || nodes.byproducts.contains(&depfile);
                    plan.bind(b"depfile", depfile);
                    if !persistent {
                        plan.bind(b"deps", b"gcc".as_slice());
                    }
                }
                let description = if lib_symlink {
                    let mut desc = BString::from("Creating library symlink ");
                    for (index, output) in nodes.outputs.iter().enumerate() {
                        if index > 0 {
                            desc.push(b' ');
                        }
                        desc.extend_from_slice(output);
                    }
                    desc
                } else if exe_symlink {
                    let mut desc = BString::from("Creating executable symlink ");
                    for (index, output) in nodes.outputs.iter().enumerate() {
                        if index > 0 {
                            desc.push(b' ');
                        }
                        desc.extend_from_slice(output);
                    }
                    desc
                } else {
                    custom
                        .outputs
                        .first()
                        .and_then(|output| self.input.link_descriptions.get(output))
                        .cloned()
                        .or_else(|| custom.comment.clone())
                        .unwrap_or_else(|| {
                            if custom.outputs.is_empty() {
                                return "Running custom command".into();
                            }
                            let mut description = BString::from("Generating ");
                            for (index, output) in custom.outputs.iter().enumerate() {
                                if index > 0 {
                                    description.extend_from_slice(b", ");
                                }
                                description.extend_from_slice(output);
                            }
                            description
                        })
                };
                plan.bind(b"DESC", description);
                if custom.uses_terminal {
                    plan.bind(b"pool", b"console".as_slice());
                } else if let Some(pool) = custom.job_pool.as_ref().filter(|pool| !pool.is_empty())
                {
                    plan.bind(b"pool", pool.as_slice());
                }
                plan
            }
        };
        Ok(plan)
    }

    fn is_absolute_library_fragment(&self, token: &[u8]) -> bool {
        if token.is_empty()
            || token.starts_with(b"-")
            || token.ends_with(b"/")
            || token.ends_with(b".framework")
        {
            return false;
        }
        if self.target_is_windows() || self.commands.windows() {
            let drive = token.first().is_some_and(u8::is_ascii_alphabetic)
                && token.get(1) == Some(&b':')
                && matches!(token.get(2), Some(b'/' | b'\\'));
            let unc = token.starts_with(b"//") || token.starts_with(b"\\\\");
            return drive || unc;
        }
        if let Some(rest) = token.strip_prefix(b"/") {
            let first_segment = rest
                .split(|&byte| byte == b'/' || byte == b'\\')
                .next()
                .unwrap_or(rest);
            if first_segment.contains(&b':') {
                return false;
            }
        }
        self.commands.absolute(token)
    }
}
