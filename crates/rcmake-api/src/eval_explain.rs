//! Generator expression evaluation for `rcmake explain --eval` and `--eval-property`.

use std::fmt::Write as _;

use rcmake_base::{BString, list};
use rcmake_genex::{
    ConfigId, EvalContext, EvalSite, GenexStep, GenexTrace, SkippedOperand, UsageTraversal,
};
use rcmake_json::Value;
use rcmake_model::{DirId, PropScope};
use rcmake_resolve::usage::{ObjectPaths, UsageResolver};

use crate::ConfigureResult;

/// Schema of `rcmake explain --eval --json`.
pub const EXPLAIN_EVAL_SCHEMA: &str = "rcmake-explain-eval-v1";

/// Operators whose value needs a `Ninja Multi-Config` command or output configuration.
const CROSS_CONFIG: [&[u8]; 2] = [b"COMMAND_CONFIG", b"OUTPUT_CONFIG"];

/// Input of one evaluation request.
#[derive(Clone, Debug, Default)]
pub struct EvalRequest {
    /// Expression text for `--eval`.
    pub expression: Option<BString>,
    /// Target that owns the property for `--eval-property`.
    pub owner: Option<BString>,
    /// Property name for `--eval-property`.
    pub property: Option<BString>,
    /// Consuming target (`head`). Defaults to the owner.
    pub consumer: Option<BString>,
    /// Configurations to evaluate. Empty means every configuration.
    pub configs: Vec<BString>,
    /// `$<COMPILE_LANGUAGE>`.
    pub language: Option<BString>,
    /// `$<LINK_LANGUAGE>`.
    pub link_language: Option<BString>,
    /// Source directory, relative to the project root, for target name lookup.
    pub directory: Option<BString>,
    /// Follow link-only usage.
    pub link: bool,
    /// `$<COMMAND_CONFIG>` configuration.
    pub command_config: Option<BString>,
    /// `$<OUTPUT_CONFIG>` configuration.
    pub output_config: Option<BString>,
    /// Most recorded steps per configuration.
    pub step_limit: usize,
    /// Directory of the bundled CMake modules, for denial attribution.
    pub module_root: Option<BString>,
}

/// Result for one configuration.
#[derive(Clone, Debug)]
pub struct EvalConfigResult {
    /// Configuration name. Empty means the default configuration.
    pub config: BString,
    /// Value or error text.
    pub value: Result<BString, BString>,
    /// The value depends on the consuming target.
    pub head_sensitive: bool,
    /// The value depends on the configuration or language context.
    pub context_sensitive: bool,
    /// The value depends on the link language.
    pub link_language_sensitive: bool,
    /// Evaluated steps.
    pub steps: Vec<GenexStep>,
    /// Operands that short-circuit operators skipped.
    pub skipped: Vec<SkippedOperand>,
    /// Steps and skipped operands past the limit.
    pub omitted: u32,
    /// Operators that had no evidence in this tree, for example `COMMAND_CONFIG`.
    pub unavailable: Vec<BString>,
}

/// Report of one evaluation request.
#[derive(Clone, Debug)]
pub struct EvalReport {
    /// Expression that was evaluated.
    pub expression: BString,
    /// Property name for `--eval-property`.
    pub property: Option<BString>,
    /// Property owner (`current`).
    pub owner: Option<BString>,
    /// Consuming target (`head`).
    pub consumer: Option<BString>,
    /// Lookup directory, relative to the project root.
    pub directory: BString,
    /// `$<COMPILE_LANGUAGE>` value.
    pub language: Option<BString>,
    /// `$<LINK_LANGUAGE>` value.
    pub link_language: Option<BString>,
    /// Usage traversal.
    pub link: bool,
    /// One result per configuration.
    pub results: Vec<EvalConfigResult>,
    /// The context came from a partial evaluation. Values can differ from a full configure.
    pub provisional: bool,
    /// Reasons why the result is provisional or incomplete.
    pub notes: Vec<BString>,
}

impl EvalReport {
    /// Return whether every configuration evaluated and the context is complete.
    #[must_use]
    pub fn success(&self) -> bool {
        !self.provisional
            && !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|result| result.value.is_ok() && result.unavailable.is_empty())
    }
}

impl ConfigureResult {
    /// Evaluate a generator expression or a stored target property.
    ///
    /// # Errors
    ///
    /// Returns a message when a target, directory, property, or configuration is unknown.
    pub fn evaluate_genex(&self, request: &EvalRequest) -> Result<EvalReport, String> {
        let model = &self.model;
        let root = &model.dir(DirId::ROOT).source_dir;
        let named = request.consumer.as_ref().or(request.owner.as_ref());
        let lookup_dir = match (&request.directory, named) {
            (Some(relative), _) => {
                let wanted = if relative.as_slice() == b"." || relative.is_empty() {
                    root.clone()
                } else {
                    let mut path = root.clone();
                    path.push(b'/');
                    path.extend_from_slice(relative.strip_suffix(b"/").unwrap_or(relative));
                    path
                };
                model
                    .dirs()
                    .find(|(_, dir)| dir.source_dir == wanted)
                    .map(|(id, _)| id)
                    .ok_or_else(|| {
                        format!(
                            "Unknown source directory {}.",
                            String::from_utf8_lossy(relative)
                        )
                    })?
            }
            (None, Some(name)) => definition_dir(model, name)?,
            (None, None) => DirId::ROOT,
        };
        // Resolve each role once, in the lookup directory, so the property and the
        // evaluation context always describe the same target.
        let scoped = |name: &BString| {
            model.find_target(lookup_dir, name).ok_or_else(|| {
                format!(
                    "Unknown target {} in source directory {}.",
                    String::from_utf8_lossy(name),
                    display(&relative_dir(model, lookup_dir))
                )
            })
        };
        let owner = request.owner.as_ref().map(scoped).transpose()?;
        let consumer = request.consumer.as_ref().map(scoped).transpose()?;
        let (expression, property) = match (&request.expression, &request.property, owner) {
            (Some(expression), None, _) => (expression.clone(), None),
            (None, Some(property), Some(id)) => {
                let value = model
                    .get_prop(&PropScope::Target(id), property)
                    .ok_or_else(|| {
                        format!(
                            "Target {} has no property {}.",
                            String::from_utf8_lossy(
                                request.owner.as_deref().map_or(&[][..], |v| v.as_slice())
                            ),
                            String::from_utf8_lossy(property)
                        )
                    })?;
                (value, Some(property.clone()))
            }
            _ => return Err("Give either an expression or a target property.".to_owned()),
        };
        let configs = self.eval_configs(&request.configs)?;
        let mut notes = Vec::new();
        let partial = self
            .completion
            .provisional_outside(request.module_root.as_deref().map(|root| root.as_slice()));
        let provisional = partial || self.generation.is_none();
        if partial {
            notes.push("The overlay evaluation was partial. A subprocess was denied or the configure stopped early.".into());
        } else if self.completion.is_partial() {
            notes.push("rcmake skipped platform probes in the bundled CMake modules and used cached values.".into());
        }
        if self.generation.is_none() {
            notes.push(
                "Generation did not finish, so object paths and resolved graphs are unavailable."
                    .into(),
            );
        }
        let (command_config, output_config) = self.cross_context(request)?;
        let cross_evidence = command_config.is_some() || output_config.is_some();
        let ast = rcmake_genex::parse(&expression).map_err(|error| error.to_string())?;
        let objects = ObjectPaths::new(self.generation.iter().flat_map(|result| &result.targets));
        let mut resolver = UsageResolver::new(model);
        let mut results = Vec::new();
        let owner_handle = owner.map(target_handle).transpose()?;
        let consumer_handle = consumer.map(target_handle).transpose()?.or(owner_handle);
        for config in configs {
            let mut host = resolver.host(lookup_dir).with_object_paths(&objects);
            let ctx = EvalContext {
                config: ConfigId(config.clone()),
                head: consumer_handle,
                current: owner_handle.or(consumer_handle),
                language: request.language.clone(),
                link_language: request.link_language.clone(),
                command_config: command_config.clone().map(ConfigId),
                output_config: output_config.clone().map(ConfigId),
                traversal: if request.link {
                    UsageTraversal::Link
                } else {
                    UsageTraversal::Compile
                },
                quiet: true,
                site: EvalSite {
                    property: property.clone(),
                    ..EvalSite::default()
                },
                ..EvalContext::default()
            };
            let mut trace = GenexTrace::with_limit(request.step_limit.max(1));
            let evaluated = rcmake_genex::evaluate_traced(&ast, &ctx, &mut host, &mut trace);
            let unavailable: Vec<BString> = CROSS_CONFIG
                .iter()
                .filter(|name| {
                    trace
                        .steps
                        .iter()
                        .any(|step| step.operator.as_slice() == **name)
                        && (!cross_evidence
                            || (**name == b"COMMAND_CONFIG" && ctx.command_config.is_none())
                            || (**name == b"OUTPUT_CONFIG" && ctx.output_config.is_none()))
                })
                .map(|name| BString::from(*name))
                .collect();
            let (value, flags) = match evaluated {
                Ok((value, flags)) => (Ok(value), Some(flags)),
                Err(error) => (Err(BString::from(error.to_string())), None),
            };
            results.push(EvalConfigResult {
                config,
                value,
                head_sensitive: flags.is_some_and(|flags| flags.head_sensitive),
                context_sensitive: flags.is_some_and(|flags| flags.context_sensitive),
                link_language_sensitive: flags.is_some_and(|flags| flags.link_language_sensitive),
                omitted: trace.omitted,
                steps: trace.steps,
                skipped: trace.skipped,
                unavailable,
            });
        }
        let relative = relative_dir(model, lookup_dir);
        Ok(EvalReport {
            expression,
            property,
            owner: request.owner.clone(),
            consumer: request.consumer.clone().or_else(|| request.owner.clone()),
            directory: relative,
            language: request.language.clone(),
            link_language: request.link_language.clone(),
            link: request.link,
            results,
            provisional,
            notes,
        })
    }

    fn eval_configs(&self, requested: &[BString]) -> Result<Vec<BString>, String> {
        let mut known: Vec<BString> = self
            .generation
            .iter()
            .flat_map(|generation| generation.configs.iter().map(|config| config.0.clone()))
            .collect();
        if known.is_empty() {
            let types = self
                .model
                .definition(DirId::ROOT, b"CMAKE_CONFIGURATION_TYPES")
                .filter(|value| !value.is_empty());
            known = match types {
                Some(types) => list::split(types, list::Empty::Drop),
                None => vec![
                    self.model
                        .definition(DirId::ROOT, b"CMAKE_BUILD_TYPE")
                        .cloned()
                        .unwrap_or_default(),
                ],
            };
        }
        if requested.is_empty() {
            return Ok(known);
        }
        requested
            .iter()
            .map(|wanted| {
                known
                    .iter()
                    .find(|config| config.eq_ignore_ascii_case(wanted))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "Unknown configuration {}. Available: {}.",
                            String::from_utf8_lossy(wanted),
                            known
                                .iter()
                                .map(|c| config_label(c))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            })
            .collect()
    }

    /// Return the command and output configurations for the request.
    ///
    /// A name must match a cross-configuration graph of this tree. When both
    /// names are given, the pair must match one graph.
    fn cross_context(
        &self,
        request: &EvalRequest,
    ) -> Result<(Option<BString>, Option<BString>), String> {
        if request.command_config.is_none() && request.output_config.is_none() {
            return Ok((None, None));
        }
        let pairs: Vec<(&BString, &BString)> = self
            .generation
            .iter()
            .flat_map(|generation| generation.cross_graphs.keys())
            .map(|(command, output)| (&command.0, &output.0))
            .collect();
        if pairs.is_empty() {
            return Err(
                "This tree has no cross-configuration commands. Omit --command-config and --output-config."
                    .to_owned(),
            );
        }
        let matches = |wanted: Option<&BString>, actual: &BString| {
            wanted.is_none_or(|wanted| wanted.eq_ignore_ascii_case(actual))
        };
        let found = pairs.iter().find(|(command, output)| {
            matches(request.command_config.as_ref(), command)
                && matches(request.output_config.as_ref(), output)
        });
        let Some((command, output)) = found else {
            let available: Vec<String> = pairs
                .iter()
                .map(|(command, output)| format!("{}:{}", display(command), display(output)))
                .collect();
            return Err(format!(
                "No cross-configuration command matches the requested configurations. Available command:output pairs: {}.",
                available.join(", ")
            ));
        };
        Ok((
            request.command_config.as_ref().map(|_| (*command).clone()),
            request.output_config.as_ref().map(|_| (*output).clone()),
        ))
    }
}

/// Return the directory that defines the only target named `name`.
fn definition_dir(model: &rcmake_model::ProjectModel, name: &BString) -> Result<DirId, String> {
    let mut dirs: Vec<DirId> = model
        .targets()
        .filter(|(_, target)| target.name == *name)
        .map(|(_, target)| target.dir)
        .collect();
    dirs.dedup();
    match dirs.as_slice() {
        [dir] => Ok(*dir),
        [] => model
            .find_target(DirId::ROOT, name)
            .and_then(|id| model.target(id))
            .map(|target| target.dir)
            .ok_or_else(|| format!("Unknown target {}.", String::from_utf8_lossy(name))),
        _ => Err(format!(
            "More than one directory defines a target named {}. Give --directory to choose one.",
            String::from_utf8_lossy(name)
        )),
    }
}

fn relative_dir(model: &rcmake_model::ProjectModel, dir: DirId) -> BString {
    let root = &model.dir(DirId::ROOT).source_dir;
    model
        .dir(dir)
        .source_dir
        .strip_prefix(root.as_slice())
        .map(|rest| rest.strip_prefix(b"/").unwrap_or(rest))
        .filter(|rest| !rest.is_empty())
        .map_or_else(|| BString::from("."), BString::from)
}

fn target_handle(id: rcmake_model::TargetId) -> Result<rcmake_genex::TargetHandle, String> {
    u32::try_from(id.index())
        .map(rcmake_genex::TargetHandle)
        .map_err(|_| "The project has too many targets.".to_owned())
}

fn display(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn config_label(config: &[u8]) -> String {
    if config.is_empty() {
        "(default)".to_owned()
    } else {
        display(config)
    }
}

fn paint(text: String, code: u8, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text
    }
}

/// Render the report for a terminal. `color` adds ANSI color to the values.
#[must_use]
pub fn render_eval_terminal(report: &EvalReport, steps: bool, color: bool) -> String {
    let mut out = String::new();
    let mut context = Vec::new();
    if let Some(owner) = &report.owner {
        context.push(format!("owner {}", display(owner)));
    }
    if let Some(consumer) = &report.consumer
        && report.owner.as_ref() != Some(consumer)
    {
        context.push(format!("consumer {}", display(consumer)));
    } else if report.owner.is_none()
        && let Some(consumer) = &report.consumer
    {
        context.push(format!("target {}", display(consumer)));
    }
    if let Some(language) = &report.language {
        context.push(format!("language {}", display(language)));
    }
    if let Some(property) = &report.property {
        let _ = writeln!(
            out,
            "{} = {}",
            display(property),
            display(&report.expression)
        );
    }
    let _ = writeln!(
        out,
        "{}   {}\n",
        display(&report.expression),
        context.join(", ")
    );
    for result in &report.results {
        let value = match &result.value {
            Ok(value) if value.is_empty() => paint("(empty)".to_owned(), 2, color),
            Ok(value) => paint(display(value), 32, color),
            Err(error) => paint(format!("error: {}", display(error)), 31, color),
        };
        let _ = writeln!(out, "{:<8} {value}", config_label(&result.config));
        if steps {
            for step in &result.steps {
                let value = match &step.result {
                    Ok(value) if value.is_empty() => "(empty)".to_owned(),
                    Ok(value) => display(value),
                    Err(_) => "(error)".to_owned(),
                };
                let host = if step.host {
                    "   (from the project model)"
                } else {
                    ""
                };
                let indent = "  ".repeat(usize::try_from(step.depth).unwrap_or(1));
                let _ = writeln!(out, "{indent}{:<44} {value}{host}", display(&step.source));
                for skipped in result
                    .skipped
                    .iter()
                    .filter(|skipped| skipped.parent == Some(step.id))
                {
                    let kind = if skipped.literal {
                        "literal"
                    } else {
                        "expression"
                    };
                    let _ = writeln!(
                        out,
                        "{indent}  parameter {} not evaluated ({kind} {})",
                        skipped.index + 1,
                        display(&skipped.source)
                    );
                }
            }
            if result.omitted > 0 {
                let _ = writeln!(out, "  {} more steps were not recorded.", result.omitted);
            }
        }
        for name in &result.unavailable {
            let _ = writeln!(out, "  $<{}> has no evidence in this tree.", display(name));
        }
    }
    let mut depends = Vec::new();
    if report.results.iter().any(|result| result.context_sensitive) {
        depends.push("configuration or language");
    }
    if report.results.iter().any(|result| result.head_sensitive) {
        depends.push("consuming target");
    }
    if report
        .results
        .iter()
        .any(|result| result.link_language_sensitive)
    {
        depends.push("link language");
    }
    if !depends.is_empty() {
        let _ = writeln!(out, "\nDepends on: {}", depends.join(", "));
    }
    for note in &report.notes {
        let _ = writeln!(out, "{}", display(note));
    }
    if report.provisional {
        out.push_str("This result is provisional.\n");
    }
    out
}

fn opt(value: Option<&BString>) -> Value {
    value.map_or(Value::Null, |value| Value::String(value.clone()))
}

/// Render the report as `rcmake-explain-eval-v1` JSON.
#[must_use]
pub fn render_eval_json(report: &EvalReport) -> BString {
    let results = report.results.iter().map(|result| {
        let (value, error) = match &result.value {
            Ok(value) => (Value::String(value.clone()), Value::Null),
            Err(error) => (Value::Null, Value::String(error.clone())),
        };
        let list = match &result.value {
            Ok(value) => Value::array_from(
                list::split(value, list::Empty::Drop)
                    .into_iter()
                    .map(Value::String),
            ),
            Err(_) => Value::Null,
        };
        Value::object_from([
            ("config", Value::String(result.config.clone())),
            ("value", value),
            ("list", list),
            ("error", error),
            (
                "depends_on",
                Value::object_from([
                    ("head", Value::Bool(result.head_sensitive)),
                    ("context", Value::Bool(result.context_sensitive)),
                    ("link_language", Value::Bool(result.link_language_sensitive)),
                ]),
            ),
            (
                "steps",
                Value::array_from(result.steps.iter().map(|step| {
                    let (value, error) = match &step.result {
                        Ok(value) => (Value::String(value.clone()), Value::Null),
                        Err(error) => (Value::Null, Value::String(error.clone())),
                    };
                    Value::object_from([
                        ("id", Value::UInt(u64::from(step.id))),
                        (
                            "parent",
                            step.parent
                                .map_or(Value::Null, |id| Value::UInt(u64::from(id))),
                        ),
                        ("depth", Value::UInt(u64::from(step.depth))),
                        ("source", Value::String(step.source.clone())),
                        ("operator", Value::String(step.operator.clone())),
                        ("value", value),
                        ("value_truncated", Value::Bool(step.value_truncated)),
                        ("from_model", Value::Bool(step.host)),
                        ("error", error),
                    ])
                })),
            ),
            (
                "skipped",
                Value::array_from(result.skipped.iter().map(|skipped| {
                    Value::object_from([
                        (
                            "parent",
                            skipped
                                .parent
                                .map_or(Value::Null, |id| Value::UInt(u64::from(id))),
                        ),
                        ("index", Value::UInt(u64::from(skipped.index))),
                        ("source", Value::String(skipped.source.clone())),
                        ("literal", Value::Bool(skipped.literal)),
                    ])
                })),
            ),
            ("steps_omitted", Value::UInt(u64::from(result.omitted))),
            ("steps_truncated", Value::Bool(result.omitted > 0)),
            (
                "unavailable",
                Value::array_from(result.unavailable.iter().cloned().map(Value::String)),
            ),
        ])
    });
    let value = Value::object_from([
        ("schema", Value::string(EXPLAIN_EVAL_SCHEMA)),
        ("expression", Value::String(report.expression.clone())),
        ("property", opt(report.property.as_ref())),
        (
            "context",
            Value::object_from([
                ("owner", opt(report.owner.as_ref())),
                ("target", opt(report.consumer.as_ref())),
                ("language", opt(report.language.as_ref())),
                ("link_language", opt(report.link_language.as_ref())),
                ("directory", Value::String(report.directory.clone())),
                (
                    "traversal",
                    Value::string(if report.link { "link" } else { "compile" }),
                ),
            ]),
        ),
        ("results", Value::array_from(results)),
        (
            "notes",
            Value::array_from(report.notes.iter().cloned().map(Value::String)),
        ),
        ("provisional", Value::Bool(report.provisional)),
        ("success", Value::Bool(report.success())),
    ]);
    let mut out = rcmake_json::write_styled(&value, "  ");
    out.push(b'\n');
    out
}

#[cfg(test)]
#[path = "eval_explain_tests.rs"]
mod tests;
