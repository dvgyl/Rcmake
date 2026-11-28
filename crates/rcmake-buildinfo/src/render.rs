//! Terminal and `rcmake-diff-config-v1` JSON output.

use std::fmt::Write as _;

use rcmake_base::BString;
use rcmake_json::Value;

use crate::diff::{ConfigDiff, ListChange, Pair};
use crate::snapshot::{Attribution, BuildSnapshot, IncludeDir, LinkFragment, PackageSnapshot};

/// Schema of `rcmake diff-config --json`.
pub const DIFF_CONFIG_SCHEMA: &str = "rcmake-diff-config-v1";
const MAX_LISTED_SOURCES: usize = 4;

fn display(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn opt(value: Option<&BString>) -> String {
    value.map_or_else(|| "(none)".to_owned(), |value| display(value))
}

fn config(name: &[u8]) -> String {
    if name.is_empty() {
        "(default)".to_owned()
    } else {
        display(name)
    }
}

fn header(snapshot: &BuildSnapshot) -> String {
    let tool = display(&snapshot.producer.tool);
    let producer = match &snapshot.producer.version {
        Some(version) => format!("{tool} {}", display(version)),
        None => tool,
    };
    let source = snapshot
        .evidence(crate::Section::Compile)
        .label()
        .to_owned();
    format!(
        "{} ({producer}, {}, compile data {source})",
        display(&snapshot.path),
        opt(snapshot.generator.as_ref())
    )
}

fn list_line<T>(
    out: &mut String,
    label: &str,
    change: &ListChange<T>,
    show: impl Fn(&T) -> String,
) {
    if !change.changed() {
        return;
    }
    let mut parts: Vec<String> = change
        .added
        .iter()
        .map(|item| format!("+ {}", show(item)))
        .collect();
    parts.extend(
        change
            .removed
            .iter()
            .map(|item| format!("- {}", show(item))),
    );
    if change.reordered {
        parts.push("order changed".to_owned());
    }
    let _ = writeln!(out, "  {label:<11}{}", parts.join("   "));
}

fn include(dir: &IncludeDir) -> String {
    if dir.system {
        format!("{} (system)", display(&dir.path))
    } else {
        display(&dir.path)
    }
}

fn fragment(item: &LinkFragment) -> String {
    format!("{} [{}]", display(&item.fragment), display(&item.role))
}

fn pair(values: &Pair<BString>) -> String {
    format!("{} -> {}", opt(values.0.as_ref()), opt(values.1.as_ref()))
}

fn paint(text: String, code: u8, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text
    }
}

/// Render the comparison for a terminal. `color` adds ANSI color to the summary.
#[must_use]
pub fn render_text(a: &BuildSnapshot, b: &BuildSnapshot, diff: &ConfigDiff, color: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}\n  vs  {}\n", header(a), header(b));
    if !diff.unmatched_configs.is_empty() {
        out.push_str("Configurations\n");
        for (side, name) in &diff.unmatched_configs {
            let sign = if *side == 'a' { '-' } else { '+' };
            let _ = writeln!(out, "  {sign} {}", config(name));
        }
    }
    if !diff.targets_added.is_empty()
        || !diff.targets_removed.is_empty()
        || diff
            .targets_changed
            .iter()
            .any(|change| change.kind.is_some())
    {
        out.push_str("Targets\n");
        for entry in &diff.targets_added {
            let _ = writeln!(
                out,
                "  + {:<12} {}  [{}]",
                display(&entry.name),
                opt(entry.kind.as_ref()),
                config(&entry.config)
            );
        }
        for entry in &diff.targets_removed {
            let _ = writeln!(
                out,
                "  - {:<12} {}  [{}]",
                display(&entry.name),
                opt(entry.kind.as_ref()),
                config(&entry.config)
            );
        }
        for change in diff
            .targets_changed
            .iter()
            .filter(|change| change.kind.is_some())
        {
            if let Some(kind) = &change.kind {
                let _ = writeln!(out, "  ~ {:<12} {}", display(&change.name), pair(kind));
            }
        }
    }
    for change in &diff.targets_changed {
        let configs = if change.config_a == change.config_b {
            config(&change.config_a)
        } else {
            format!(
                "{} -> {}",
                config(&change.config_a),
                config(&change.config_b)
            )
        };
        let _ = writeln!(out, "{}  [{configs}]", display(&change.name));
        for source in &change.sources_added {
            let _ = writeln!(out, "  source     + {}", display(source));
        }
        for source in &change.sources_removed {
            let _ = writeln!(out, "  source     - {}", display(source));
        }
        for compile in &change.compile {
            let shown: Vec<String> = compile
                .sources
                .iter()
                .take(MAX_LISTED_SOURCES)
                .map(|source| display(source))
                .collect();
            let more = compile.sources.len().saturating_sub(MAX_LISTED_SOURCES);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            let _ = writeln!(out, "  sources    {}{more}", shown.join(", "));
            if let Some((left, right)) = compile.invocations {
                let _ = writeln!(out, "  invocations {left} -> {right}");
            }
            if let Some(compiler) = &compile.compiler {
                let _ = writeln!(out, "  compiler   {}", pair(compiler));
            }
            list_line(&mut out, "launcher", &compile.launcher, |item| {
                display(item)
            });
            if let Some(language) = &compile.language {
                let _ = writeln!(out, "  language   {}", pair(language));
            }
            if let Some(standard) = &compile.standard {
                let _ = writeln!(out, "  standard   {}", pair(standard));
            }
            list_line(&mut out, "defines", &compile.defines, |item| display(item));
            list_line(&mut out, "includes", &compile.includes, include);
            list_line(
                &mut out,
                if compile.opaque { "command" } else { "flags" },
                &compile.fragments,
                |item| display(item),
            );
        }
        if let Some(link) = &change.link {
            list_line(&mut out, "link", link, fragment);
        }
        list_line(&mut out, "depends", &change.dependencies, |item| {
            display(item)
        });
    }
    if !diff.packages.is_empty() {
        out.push_str("Packages\n");
        for package in &diff.packages {
            let side = |snapshot: &Option<PackageSnapshot>| {
                snapshot.as_ref().map_or_else(
                    || "(no record)".to_owned(),
                    |package| {
                        let mut parts = vec![opt(package.cached_dir.as_ref())];
                        if let Some(version) = &package.version_hint {
                            parts.push(format!("version file {}", display(version)));
                        }
                        if let Some(last) = package.observed.last() {
                            parts.push(display(&last.outcome));
                        }
                        parts.join(", ")
                    },
                )
            };
            let _ = writeln!(
                out,
                "  ~ {:<12} {} -> {}",
                display(&package.name),
                side(&package.a),
                side(&package.b)
            );
        }
    }
    if !diff.toolchain.is_empty() {
        out.push_str("Toolchain\n");
        for change in &diff.toolchain {
            let _ = writeln!(
                out,
                "  ~ {:<6} {:<8} {}",
                display(&change.language),
                change.field,
                pair(&change.values)
            );
        }
    }
    if !diff.cache.is_empty() {
        out.push_str("Cache\n");
        for change in &diff.cache {
            let sign = match (&change.values.0, &change.values.1) {
                (None, Some(_)) => '+',
                (Some(_), None) => '-',
                _ => '~',
            };
            let _ = writeln!(
                out,
                "  {sign} {:<28} {}",
                display(&change.name),
                pair(&change.values)
            );
        }
    }
    for (section, reason) in &diff.partial {
        let _ = writeln!(
            out,
            "note: {} uses partial data. {}",
            section.label(),
            display(reason)
        );
    }
    for (section, reason) in &diff.incomplete {
        let _ = writeln!(
            out,
            "note: {} was not compared. {}",
            section.label(),
            display(reason)
        );
    }
    let summary = if diff.has_changes() {
        format!(
            "{} targets added, {} removed, {} changed, {} packages changed, {} toolchain changes, {} cache entries changed.",
            diff.targets_added.len(),
            diff.targets_removed.len(),
            diff.targets_changed.len(),
            diff.packages.len(),
            diff.toolchain.len(),
            diff.cache.len()
        )
    } else if diff.complete() {
        "No differences.".to_owned()
    } else {
        "No differences found, but the evidence is incomplete, so parity is not proved.".to_owned()
    };
    let code = match diff.exit_code() {
        0 => 32,
        1 => 31,
        _ => 33,
    };
    let _ = writeln!(out, "\n{}", paint(summary, code, color));
    out
}

fn string(value: &[u8]) -> Value {
    Value::String(value.into())
}

fn nullable(value: Option<&BString>) -> Value {
    value.map_or(Value::Null, |value| string(value))
}

fn pair_json(values: &Pair<BString>) -> Value {
    Value::object_from([
        ("a", nullable(values.0.as_ref())),
        ("b", nullable(values.1.as_ref())),
    ])
}

fn list_json<T>(change: &ListChange<T>, item: impl Fn(&T) -> Value) -> Value {
    Value::object_from([
        ("added", Value::array_from(change.added.iter().map(&item))),
        (
            "removed",
            Value::array_from(change.removed.iter().map(&item)),
        ),
        ("reordered", Value::Bool(change.reordered)),
    ])
}

fn side_json(snapshot: &BuildSnapshot) -> Value {
    Value::object_from([
        ("path", string(&snapshot.path)),
        (
            "producer",
            Value::object_from([
                ("tool", string(&snapshot.producer.tool)),
                ("version", nullable(snapshot.producer.version.as_ref())),
            ]),
        ),
        ("generator", nullable(snapshot.generator.as_ref())),
        (
            "configs",
            Value::array_from(snapshot.configs.iter().map(|config| string(config))),
        ),
        (
            "evidence",
            Value::object_from(snapshot.evidence.iter().map(|(section, evidence)| {
                let (source, reason) = match evidence {
                    crate::Evidence::Complete(source) => (Some(source.label()), None),
                    crate::Evidence::Partial(source, reason) => {
                        (Some(source.label()), Some(reason))
                    }
                    crate::Evidence::Unavailable(reason) => (None, Some(reason)),
                };
                (
                    section.label(),
                    Value::object_from([
                        ("state", Value::string(evidence.label())),
                        ("source", source.map_or(Value::Null, Value::string)),
                        ("reason", nullable(reason)),
                    ]),
                )
            })),
        ),
        (
            "warnings",
            Value::array_from(snapshot.warnings.iter().map(|warning| string(warning))),
        ),
    ])
}

fn package_json(package: Option<&PackageSnapshot>) -> Value {
    package.map_or(Value::Null, |package| {
        Value::object_from([
            ("cached_dir", nullable(package.cached_dir.as_ref())),
            ("version_hint", nullable(package.version_hint.as_ref())),
            (
                "observed",
                Value::array_from(package.observed.iter().map(|observation| {
                    Value::object_from([
                        (
                            "package_call",
                            observation
                                .package_call
                                .map_or(Value::Null, |call| Value::UInt(u64::from(call))),
                        ),
                        ("outcome", string(&observation.outcome)),
                        ("version", nullable(observation.version.as_ref())),
                        ("selected", nullable(observation.selected.as_ref())),
                    ])
                })),
            ),
        ])
    })
}

fn attribution(value: Attribution) -> Value {
    Value::string(value.label())
}

/// Render the comparison as deterministic `rcmake-diff-config-v1` JSON.
#[must_use]
pub fn render_json(a: &BuildSnapshot, b: &BuildSnapshot, diff: &ConfigDiff) -> BString {
    let targets = diff.targets_changed.iter().map(|change| {
        Value::object_from([
            ("name", string(&change.name)),
            (
                "config",
                Value::object_from([
                    ("a", string(&change.config_a)),
                    ("b", string(&change.config_b)),
                ]),
            ),
            (
                "attribution",
                Value::object_from([
                    ("a", attribution(change.attribution.0)),
                    ("b", attribution(change.attribution.1)),
                ]),
            ),
            ("kind", change.kind.as_ref().map_or(Value::Null, pair_json)),
            (
                "sources_added",
                Value::array_from(change.sources_added.iter().map(|s| string(s))),
            ),
            (
                "sources_removed",
                Value::array_from(change.sources_removed.iter().map(|s| string(s))),
            ),
            (
                "compile",
                Value::array_from(change.compile.iter().map(|compile| {
                    Value::object_from([
                        (
                            "sources",
                            Value::array_from(compile.sources.iter().map(|s| string(s))),
                        ),
                        (
                            "language",
                            compile.language.as_ref().map_or(Value::Null, pair_json),
                        ),
                        (
                            "standard",
                            compile.standard.as_ref().map_or(Value::Null, pair_json),
                        ),
                        ("defines", list_json(&compile.defines, |item| string(item))),
                        (
                            "includes",
                            list_json(&compile.includes, |item| {
                                Value::object_from([
                                    ("path", string(&item.path)),
                                    ("system", Value::Bool(item.system)),
                                ])
                            }),
                        ),
                        ("flags", list_json(&compile.fragments, |item| string(item))),
                        ("opaque", Value::Bool(compile.opaque)),
                        (
                            "compiler",
                            compile.compiler.as_ref().map_or(Value::Null, pair_json),
                        ),
                        (
                            "launcher",
                            list_json(&compile.launcher, |item| string(item)),
                        ),
                        (
                            "invocations",
                            compile.invocations.map_or(Value::Null, |(left, right)| {
                                Value::object_from([
                                    ("a", Value::UInt(left as u64)),
                                    ("b", Value::UInt(right as u64)),
                                ])
                            }),
                        ),
                    ])
                })),
            ),
            (
                "link",
                change.link.as_ref().map_or(Value::Null, |link| {
                    list_json(link, |item| {
                        Value::object_from([
                            ("fragment", string(&item.fragment)),
                            ("role", string(&item.role)),
                        ])
                    })
                }),
            ),
            (
                "dependencies",
                list_json(&change.dependencies, |item| string(item)),
            ),
            ("backtrace", pair_json(&change.backtrace)),
        ])
    });
    let entry = |entry: &crate::diff::TargetEntry| {
        Value::object_from([
            ("name", string(&entry.name)),
            ("config", string(&entry.config)),
            ("kind", nullable(entry.kind.as_ref())),
        ])
    };
    let sections = |list: &[(crate::Section, BString)]| {
        Value::array_from(list.iter().map(|(section, reason)| {
            Value::object_from([
                ("section", Value::string(section.label())),
                ("reason", string(reason)),
            ])
        }))
    };
    let value = Value::object_from([
        ("schema", Value::string(DIFF_CONFIG_SCHEMA)),
        ("a", side_json(a)),
        ("b", side_json(b)),
        ("normalized", Value::Bool(diff.normalized)),
        (
            "config_pairs",
            Value::array_from(diff.config_pairs.iter().map(|(left, right)| {
                Value::object_from([("a", string(left)), ("b", string(right))])
            })),
        ),
        (
            "unmatched_configs",
            Value::array_from(diff.unmatched_configs.iter().map(|(side, name)| {
                Value::object_from([
                    ("side", Value::string(side.to_string())),
                    ("config", string(name)),
                ])
            })),
        ),
        (
            "summary",
            Value::object_from([
                (
                    "targets_added",
                    Value::UInt(diff.targets_added.len() as u64),
                ),
                (
                    "targets_removed",
                    Value::UInt(diff.targets_removed.len() as u64),
                ),
                (
                    "targets_changed",
                    Value::UInt(diff.targets_changed.len() as u64),
                ),
                ("packages_changed", Value::UInt(diff.packages.len() as u64)),
                ("toolchain_changed", Value::Bool(!diff.toolchain.is_empty())),
                ("cache_changed", Value::UInt(diff.cache.len() as u64)),
                ("differences", Value::Bool(diff.has_changes())),
                ("complete", Value::Bool(diff.complete())),
                ("exit_code", Value::Int(i64::from(diff.exit_code()))),
            ]),
        ),
        (
            "targets",
            Value::object_from([
                (
                    "added",
                    Value::array_from(diff.targets_added.iter().map(entry)),
                ),
                (
                    "removed",
                    Value::array_from(diff.targets_removed.iter().map(entry)),
                ),
                ("changed", Value::array_from(targets)),
            ]),
        ),
        (
            "packages",
            Value::array_from(diff.packages.iter().map(|package| {
                Value::object_from([
                    ("name", string(&package.name)),
                    (
                        "fields",
                        Value::array_from(package.fields.iter().map(|field| Value::string(*field))),
                    ),
                    ("a", package_json(package.a.as_ref())),
                    ("b", package_json(package.b.as_ref())),
                ])
            })),
        ),
        (
            "toolchain",
            Value::array_from(diff.toolchain.iter().map(|change| {
                Value::object_from([
                    ("language", string(&change.language)),
                    ("field", Value::string(change.field)),
                    ("a", nullable(change.values.0.as_ref())),
                    ("b", nullable(change.values.1.as_ref())),
                ])
            })),
        ),
        (
            "cache",
            Value::array_from(diff.cache.iter().map(|change| {
                Value::object_from([
                    ("name", string(&change.name)),
                    ("type", string(&change.ty)),
                    ("a", nullable(change.values.0.as_ref())),
                    ("b", nullable(change.values.1.as_ref())),
                ])
            })),
        ),
        ("partial", sections(&diff.partial)),
        ("incomplete", sections(&diff.incomplete)),
    ]);
    let mut out = rcmake_json::write_styled(&value, "  ");
    out.push(b'\n');
    out
}
