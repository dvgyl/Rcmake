use rcmake_base::BString;
use rcmake_gen_common::{GenError, export::ModuleExportJob};
use rcmake_resolve::{BuildStep, DependInfo, progress};

use super::{Graph, StepNodes, is_library_symlink};
use crate::response::{COMPILE_MSVC, Response};

pub(super) struct EdgePlan {
    pub(super) rule: &'static [u8],
    pub(super) inputs: Vec<BString>,
    pub(super) implicit_inputs: Vec<BString>,
    pub(super) order_only: Vec<BString>,
    pub(super) implicit_outputs: Vec<BString>,
    pub(super) dyndep: Option<BString>,
    pub(super) bindings: Vec<(&'static [u8], BString)>,
}

impl EdgePlan {
    fn new(rule: &'static [u8], inputs: Vec<BString>, nodes: &StepNodes<'_>) -> Self {
        Self {
            rule,
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
            self.rule = response.rule;
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
        step: &BuildStep,
        nodes: &StepNodes<'_>,
        cmake: &[u8],
        modmap_flag: &[u8],
        exports: &[ModuleExportJob],
    ) -> Result<EdgePlan, GenError> {
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
                let (rule, command) = if msvc {
                    (COMPILE_MSVC, None)
                } else if self.commands.windows() {
                    (
                        crate::instrumented::ordinary_rule(scanned, depfile.is_some()),
                        None,
                    )
                } else {
                    crate::instrumented::compile(invocation, scanned, depfile.is_some())
                };
                let mut plan = EdgePlan::new(
                    rule,
                    vec![self.node(compile_input.as_ref().unwrap_or(source))],
                    nodes,
                );
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
                    |command| self.in_build_dir(&command).into(),
                );
                let response = self.compile_response(rule, object, invocation, &command);
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
                let mut plan = EdgePlan::new(rule, vec![self.node(source)], nodes);
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
                let path_inputs = inputs.iter().map(|input| self.node(input)).collect();
                let mut plan = EdgePlan::new(step.kind.ninja_rule(), path_inputs, nodes);
                plan.implicit_inputs
                    .extend(implicit_inputs.iter().map(|input| self.node(input)));
                if let Some(prepared) = self.prepared_link(nodes) {
                    prepared.bind(&mut plan);
                } else {
                    let command = self.command_line(invocations, false);
                    let response = self.link_response(step, inputs, invocations, &command);
                    plan.bind_command(command, response);
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
                let resolved = custom
                    .depends
                    .iter()
                    .map(|input| self.custom_input(input))
                    .collect::<Vec<_>>();
                // CMake also lists the absolute path of each file in the build tree,
                // so a depfile that names the absolute path finds this edge.
                let aliases = custom
                    .outputs
                    .iter()
                    .chain(&custom.byproducts)
                    .filter_map(|name| self.absolute_alias(&self.node(name)))
                    .collect::<Vec<_>>();
                if custom.invocations.is_empty() {
                    let inputs = resolved.into_iter().map(|input| input.node).collect();
                    let mut plan = EdgePlan::new(b"phony", inputs, nodes);
                    plan.implicit_outputs.extend(aliases);
                    return Ok(plan);
                }
                // A target without a file only orders the command, as in CMake.
                let (targets, files): (Vec<_>, Vec<_>) =
                    resolved.into_iter().partition(|input| input.target_only);
                let inputs = files.into_iter().map(|input| input.node).collect();
                let rule = if is_library_symlink(custom) {
                    b"CMAKE_SYMLINK_LIBRARY".as_slice()
                } else {
                    b"CUSTOM_COMMAND"
                };
                let mut plan = EdgePlan::new(rule, inputs, nodes);
                let symbolic = custom.symbolic || !custom.symbolic_outputs.is_empty();
                if rule == b"CUSTOM_COMMAND" && (!symbolic || !custom.byproducts.is_empty()) {
                    plan.bind(b"restat", b"1".as_slice());
                }
                plan.order_only = targets.into_iter().map(|input| input.node).collect();
                plan.implicit_outputs.extend(aliases);
                let script = nodes
                    .outputs
                    .first()
                    .and_then(|output| self.custom_scripts.get(output));
                let command = script.map_or_else(
                    || self.command_line(&custom.invocations, true),
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
                if let Some(depfile) = &custom.depfile {
                    let depfile = self.node(depfile);
                    // Ninja removes the depfile when it records dependencies in its database.
                    if !nodes.outputs.contains(&depfile) && !nodes.byproducts.contains(&depfile) {
                        plan.bind(b"deps", b"gcc".as_slice());
                    }
                    plan.bind(b"depfile", depfile);
                }
                let description = custom
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
                    });
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
}
