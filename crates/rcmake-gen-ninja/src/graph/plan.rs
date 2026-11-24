use rcmake_base::BString;
use rcmake_gen_common::export::ModuleExportJob;
use rcmake_resolve::{BuildStep, Invocation, progress};

use super::{Graph, StepNodes, shell_word};

pub(super) struct EdgePlan {
    pub(super) rule: &'static [u8],
    pub(super) inputs: Vec<BString>,
    pub(super) implicit_inputs: Vec<BString>,
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
            implicit_outputs: nodes.byproducts.clone(),
            dyndep: None,
            bindings: Vec::new(),
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
    ) -> EdgePlan {
        match step {
            BuildStep::Compile {
                object,
                source,
                invocation,
                depfile,
                modmap,
                dyndep,
            } => {
                let (rule, command) = crate::instrumented::compile(
                    invocation,
                    dyndep.is_some() || modmap.is_some(),
                    depfile.is_some(),
                );
                let mut plan = EdgePlan::new(rule, vec![self.node(source)], nodes);
                plan.implicit_inputs
                    .extend(modmap.iter().map(|map| self.node(map)));
                plan.implicit_inputs.extend(
                    self.compile_inputs
                        .and_then(|inputs| inputs.get(object))
                        .into_iter()
                        .flatten()
                        .map(|input| self.node(input)),
                );
                plan.bind(
                    b"COMMAND",
                    command
                        .unwrap_or_else(|| command_line(std::slice::from_ref(invocation), false)),
                );
                if let Some(output) = nodes.outputs.first() {
                    plan.bind(
                        b"DESC",
                        progress::compile_description(
                            self.compile_languages.get(object.as_slice()).copied(),
                            output,
                        ),
                    );
                }
                if let Some(depfile) = depfile {
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
                invocation,
                depfile,
                ..
            } => {
                let response = self.scan_responses.get(ddi.as_slice()).copied();
                let rule: &[u8] = match (depfile.is_some(), response.is_some()) {
                    (true, true) => b"CXX_SCAN_RSP",
                    (true, false) => b"CXX_SCAN",
                    (false, true) => b"CXX_SCAN_RSP_NO_DEPFILE",
                    (false, false) => b"CXX_SCAN_NO_DEPFILE",
                };
                let mut plan = EdgePlan::new(rule, vec![self.node(source)], nodes);
                let invocation = response.map_or(invocation, |response| &response.invocation);
                plan.bind(
                    b"COMMAND",
                    command_line(std::slice::from_ref(invocation), false),
                );
                if let Some(response) = response {
                    let rsp = [self.node(ddi).as_slice(), b".rsp"].concat();
                    plan.bind(b"RSP_FILE", rsp);
                    plan.bind(b"RSP_ARGS", response.arguments.clone());
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
                let inputs = ddis.iter().map(|ddi| self.node(ddi)).collect();
                let mut plan = EdgePlan::new(b"CXX_DYNDEP", inputs, nodes);
                plan.implicit_outputs.extend(
                    depend_info
                        .bmi_installation
                        .iter()
                        .filter(|install| !install.script_location.is_empty())
                        .map(|install| self.node(&install.script_location)),
                );
                let tdi = self.node(tdi);
                let mut tdi_arg = Vec::new();
                shell_word(&tdi, &mut tdi_arg);
                plan.implicit_inputs.push(tdi);
                plan.implicit_inputs
                    .extend(implicit_inputs.iter().map(|input| self.node(input)));
                plan.bind(b"CMAKE_COMMAND", cmake);
                plan.bind(b"in_tdi", tdi_arg);
                plan.bind(b"MODMAP_FLAG", modmap_flag);
                let mut export_commands = Vec::new();
                for export in exports.iter().filter(|export| export.dyndep == *dd) {
                    let script = self.node(&export.script);
                    plan.implicit_outputs.push(self.node(&export.output));
                    export_commands.extend_from_slice(b" && ");
                    export_commands.extend_from_slice(cmake);
                    export_commands.extend_from_slice(b" -P ");
                    shell_word(&script, &mut export_commands);
                    plan.implicit_inputs.push(script);
                }
                if !export_commands.is_empty() {
                    plan.bind(b"MODULE_EXPORT_COMMANDS", export_commands);
                }
                plan
            }
            BuildStep::Link {
                inputs,
                step,
                invocations,
            } => {
                let inputs = inputs.iter().map(|input| self.node(input)).collect();
                let mut plan = EdgePlan::new(step.kind.ninja_rule(), inputs, nodes);
                plan.bind(b"COMMAND", command_line(invocations, false));
                if let Some(output) = nodes.outputs.first() {
                    plan.bind(b"DESC", progress::link_description(step, output));
                }
                plan
            }
            BuildStep::Custom(custom) => {
                let inputs = custom
                    .depends
                    .iter()
                    .map(|input| self.custom_input(input))
                    .collect();
                if custom.invocations.is_empty() {
                    return EdgePlan::new(b"phony", inputs, nodes);
                }
                let mut plan = EdgePlan::new(b"CUSTOM_COMMAND", inputs, nodes);
                plan.bind(b"COMMAND", command_line(&custom.invocations, true));
                if let Some(depfile) = &custom.depfile {
                    plan.bind(b"depfile", self.node(depfile));
                }
                let description = custom
                    .comment
                    .as_ref()
                    .map_or(b"Running custom command".as_slice(), |comment| {
                        comment.as_slice()
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
        }
    }
}

fn command_line(invocations: &[Invocation], custom: bool) -> Vec<u8> {
    let mut result = Vec::new();
    for invocation in invocations {
        if !result.is_empty() {
            result.extend_from_slice(b" && ");
        }
        let group = invocations.len() > 1 || invocation.working_dir.is_some();
        if group {
            result.push(b'(');
        }
        if let Some(directory) = &invocation.working_dir {
            result.extend_from_slice(b"cd -- ");
            shell_word(directory, &mut result);
            result.extend_from_slice(b" && ");
        }
        if !invocation.shell.is_empty() && (!custom || invocation.argv.is_empty()) {
            result.extend_from_slice(&invocation.shell);
        } else if let Some((first, rest)) = invocation.argv.split_first() {
            shell_word(first, &mut result);
            for arg in rest {
                result.push(b' ');
                if custom
                    && matches!(
                        arg.as_slice(),
                        b"<" | b">"
                            | b"<<"
                            | b">>"
                            | b"2>"
                            | b"2>>"
                            | b"2>&1"
                            | b"|"
                            | b"||"
                            | b"&&"
                    )
                {
                    result.extend_from_slice(arg);
                } else {
                    shell_word(arg, &mut result);
                }
            }
        } else {
            result.push(b':');
        }
        if group {
            result.push(b')');
        }
    }
    if result.is_empty() {
        result.push(b':');
    }
    result
}
