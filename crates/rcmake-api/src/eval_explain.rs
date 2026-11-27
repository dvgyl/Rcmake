//! Generator expression evaluation for `rcmake explain --eval` and `--eval-property`.

use std::fmt::Write as _;

use rcmake_base::{BString, list};
use rcmake_genex::{
    ConfigId, EvalContext, EvalSite, GenexHost, GenexStep, GenexTrace, SkippedOperand,
    UsageTraversal,
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
        let find = |name: &BString| {
            model
                .targets()
                .find(|(_, target)| target.name == *name)
                .map(|(id, target)| (id, target.dir))
                .or_else(|| {
                    model
                        .find_target(DirId::ROOT, name)
                        .and_then(|id| model.target(id).map(|target| (id, target.dir)))
                })
                .ok_or_else(|| format!("Unknown target {}.", String::from_utf8_lossy(name)))
        };
        let owner = request.owner.as_ref().map(find).transpose()?;
        let consumer = request.consumer.as_ref().map(find).transpose()?;
        let root = &model.dir(DirId::ROOT).source_dir;
        let lookup_dir = match &request.directory {
            Some(relative) => {
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
            None => consumer.or(owner).map_or(DirId::ROOT, |(_, dir)| dir),
        };
        let (expression, property) = match (&request.expression, &request.property, owner) {
            (Some(expression), None, _) => (expression.clone(), None),
            (None, Some(property), Some((id, _))) => {
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
        let cross_evidence = self
            .generation
            .as_ref()
            .is_some_and(|generation| !generation.cross_graphs.is_empty());
        for value in [&request.command_config, &request.output_config]
            .into_iter()
            .flatten()
        {
            if !cross_evidence {
                notes.push(format!(
                    "This tree has no cross-configuration commands, so configuration {} has no evidence.",
                    String::from_utf8_lossy(value)
                ).into());
            }
        }
        let ast = rcmake_genex::parse(&expression).map_err(|error| error.to_string())?;
        let objects = ObjectPaths::new(self.generation.iter().flat_map(|result| &result.targets));
        let mut resolver = UsageResolver::new(model);
        let mut results = Vec::new();
        for config in configs {
            let mut host = resolver.host(lookup_dir).with_object_paths(&objects);
            let owner_handle = request
                .owner
                .as_ref()
                .and_then(|name| host.find_target(name));
            let consumer_handle = request
                .consumer
                .as_ref()
                .and_then(|name| host.find_target(name))
                .or(owner_handle);
            let ctx = EvalContext {
                config: ConfigId(config.clone()),
                head: consumer_handle,
                current: owner_handle.or(consumer_handle),
                language: request.language.clone(),
                link_language: request.link_language.clone(),
                command_config: request
                    .command_config
                    .clone()
                    .filter(|_| cross_evidence)
                    .map(ConfigId),
                output_config: request
                    .output_config
                    .clone()
                    .filter(|_| cross_evidence)
                    .map(ConfigId),
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
        let relative = model
            .dir(lookup_dir)
            .source_dir
            .strip_prefix(root.as_slice())
            .map(|rest| rest.strip_prefix(b"/").unwrap_or(rest))
            .filter(|rest| !rest.is_empty())
            .map_or_else(|| BString::from("."), BString::from);
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
                    .or_else(|| (known.len() == 1).then(|| wanted.clone()))
                    .ok_or_else(|| {
                        format!(
                            "Unknown configuration {}. Available: {}.",
                            String::from_utf8_lossy(wanted),
                            known
                                .iter()
                                .map(|c| String::from_utf8_lossy(c).into_owned())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            })
            .collect()
    }
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

/// Render the report for a terminal.
#[must_use]
pub fn render_eval_terminal(report: &EvalReport, steps: bool) -> String {
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
            Ok(value) if value.is_empty() => "(empty)".to_owned(),
            Ok(value) => display(value),
            Err(error) => format!("error: {}", display(error)),
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
