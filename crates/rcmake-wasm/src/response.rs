use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_api::{ApiError, CompletionKind, ConfigureResult, ResolvedEntry, SourceLocation};
use rcmake_base::{list, truth};
use rcmake_diag::{Diagnostic, Location, MessageKind, SourceMap};
use rcmake_gen_common::{compile_commands, write_if_changed};
use rcmake_host::{FileKind, Host, ReadRange, Stream, path, pure::IoEvent};
use rcmake_model::{DirId, GeneratorKind, Target, UsageField};
use rcmake_resolve::ChainKind;
use serde_json::{Value, json};

use crate::{display_source, project::Evaluated, text};

pub(super) fn failure(category: &str, message: &str) -> Value {
    let diagnostics = vec![json!({
        "severity": "error",
        "category": category,
        "file": "CMakeLists.txt",
        "line": 1,
        "column": 1,
        "message": message,
    })];
    reply(
        false,
        diagnostics,
        Vec::new(),
        BTreeMap::new(),
        BTreeSet::new(),
    )
}

fn reply(
    ok: bool,
    diagnostics: Vec<Value>,
    targets: Vec<Value>,
    files: BTreeMap<String, String>,
    symbols: BTreeSet<String>,
) -> Value {
    json!({ "ok": ok, "diagnostics": diagnostics, "targets": targets, "files": files, "explainSymbols": symbols })
}

pub(super) fn api_error(error: &ApiError, events: &[IoEvent]) -> Value {
    let (diagnostics, sources) = match error {
        ApiError::ConfigureFailed {
            diagnostics,
            sources,
            ..
        }
        | ApiError::GenerateFailed {
            diagnostics,
            sources,
            ..
        } => (diagnostics, sources),
        _ => return failure("CONFIGURE", &error.to_string()),
    };
    let mut diagnostics: Vec<_> = diagnostics
        .iter()
        .map(|diag| diagnostic(diag, sources.as_deref()))
        .collect();
    if diagnostics.is_empty() || matches!(error, ApiError::GenerateFailed { .. }) {
        diagnostics.extend(
            events
                .iter()
                .filter(|event| event.stream == Stream::Stderr)
                .filter_map(|event| {
                    let message = text(&event.bytes);
                    message.starts_with("CMake Error").then(|| json!({
                    "severity": "error", "category": "GENERATE", "message": message.trim_end(),
                    "file": "", "line": 0, "column": 0
                }))
                }),
        );
    }
    if diagnostics.is_empty() {
        return failure("CONFIGURE", &error.to_string());
    }
    reply(
        false,
        diagnostics,
        Vec::new(),
        BTreeMap::new(),
        BTreeSet::new(),
    )
}

fn diagnostic(diagnostic: &Diagnostic, sources: Option<&SourceMap>) -> Value {
    let severity = match diagnostic.kind {
        MessageKind::Error
        | MessageKind::InternalError
        | MessageKind::AuthorError
        | MessageKind::DeprecationError => "error",
        MessageKind::Warning | MessageKind::AuthorWarning | MessageKind::DeprecationWarning => {
            "warning"
        }
        MessageKind::Log => "info",
    };
    let (file, line, column) = diagnostic
        .annotations
        .as_ref()
        .and_then(|labels| labels.labels.iter().find(|label| label.primary))
        .and_then(|label| {
            let source = sources?.get(label.span.source)?;
            let (line, column) = source.line_col(label.span.start as usize);
            Some((display_source(&source.path), line, column))
        })
        .unwrap_or_else(|| match &diagnostic.location {
            Location::None => (String::new(), 0, 0),
            Location::FileLine { file, line } => (display_source(file), *line, 1),
            Location::Backtrace(trace) => SourceLocation::from_backtrace(trace).map_or_else(
                || (String::new(), 0, 0),
                |site| (display_source(&site.path), site.line, 1),
            ),
        });
    json!({
        "severity": severity,
        "category": diagnostic.category.map(|category| category.as_str()),
        "file": file,
        "line": line,
        "column": column,
        "message": text(&diagnostic.body),
    })
}

pub(super) fn configure(project: &Evaluated) -> Value {
    let result = &project.result;
    let diagnostics: Vec<_> = result
        .diagnostics
        .iter()
        .map(|diag| diagnostic(diag, result.facts.source_map.as_deref()))
        .collect();
    let mut diagnostics = diagnostics;
    let files = match generate(project) {
        Ok(files) => files,
        Err(message) => {
            diagnostics.push(json!({
                "severity": "error", "category": "GENERATE", "file": "", "line": 0, "column": 0,
                "message": message,
            }));
            BTreeMap::new()
        }
    };
    let ok = files.contains_key("build.ninja")
        && !diagnostics.iter().any(|diag| diag["severity"] == "error");
    let mut targets = Vec::new();
    let mut symbols = BTreeSet::new();
    let config = result.generation.as_ref().and_then(|generation| {
        generation
            .configs
            .iter()
            .find(|config| config.matches(project.build_type.as_bytes()))
            .or_else(|| generation.configs.first())
    });
    let mut entries = BTreeMap::<_, Vec<_>>::new();
    for entry in &result.facts.resolved_entries {
        if config == Some(&entry.key.config) {
            entries.entry(entry.key.target).or_default().push(entry);
        }
    }
    for (id, target) in result.model.targets() {
        let site = SourceLocation::from_backtrace(&target.backtrace);
        targets.push(target_json(
            &target.name,
            target.kind.as_str(),
            target,
            site.as_ref(),
            entries.get(&id).map_or(&[], Vec::as_slice),
            result,
        ));
        symbols.insert(text(&target.name));
    }
    if let Some(index) = &result.index {
        for alias in &result.facts.aliases {
            if let Some(definition) = index.lookup_target(alias.dir, &alias.name)
                && let Some(target) = result.model.target(definition.id)
            {
                targets.push(target_json(
                    &alias.name,
                    "ALIAS",
                    target,
                    Some(&alias.location),
                    entries.get(&definition.id).map_or(&[], Vec::as_slice),
                    result,
                ));
                symbols.insert(text(&alias.name));
            }
        }
        symbols.extend(
            index
                .completions_for_scope(DirId::ROOT, b"")
                .into_iter()
                .filter(|completion| {
                    completion.kind == CompletionKind::Variable
                        && index
                            .lookup_variable(DirId::ROOT, completion.name)
                            .is_some_and(|var| var.location.is_some())
                })
                .map(|completion| text(completion.name)),
        );
    }
    reply(ok, diagnostics, targets, files, symbols)
}

fn target_json(
    name: &[u8],
    kind: &str,
    target: &Target,
    site: Option<&SourceLocation>,
    resolved: &[&ResolvedEntry],
    result: &ConfigureResult,
) -> Value {
    use UsageField as Field;
    let source_values = target
        .sources
        .iter()
        .chain(
            target
                .file_sets
                .iter()
                .filter_map(|&id| result.model.file_set(id))
                .flat_map(|set| &set.files),
        )
        .flat_map(|entry| list_text(&entry.value, true))
        .collect::<Vec<_>>();
    let usage = |own, interface| -> Vec<String> {
        let values = resolved
            .iter()
            .filter(|entry| entry.key.kind == ChainKind::Usage(own))
            .map(|entry| display_source(&entry.value.value))
            .collect::<Vec<_>>();
        if !values.is_empty() {
            let mut seen = BTreeSet::new();
            return values
                .into_iter()
                .filter(|value| seen.insert(value.clone()))
                .collect();
        }
        let field = if target.kind == rcmake_model::TargetKind::InterfaceLibrary {
            interface
        } else {
            own
        };
        target
            .usage
            .entries(field)
            .iter()
            .flat_map(|entry| list_text(&entry.value, true))
            .collect()
    };
    let links = target
        .usage
        .link_libraries
        .as_ref()
        .or(target.usage.interface_link_libraries.as_ref());
    let links = links
        .into_iter()
        .flatten()
        .flat_map(|item| list_text(&item.text.value, false))
        .collect::<Vec<_>>();
    json!({
        "name": text(name),
        "kind": kind,
        "definedAt": site.map(|site| format!("{}:{}", display_source(&site.path), site.line)),
        "sources": source_values,
        "compileFeatures": usage(Field::CompileFeatures, Field::InterfaceCompileFeatures),
        "compileDefinitions": usage(Field::CompileDefinitions, Field::InterfaceCompileDefinitions),
        "compileOptions": usage(Field::CompileOptions, Field::InterfaceCompileOptions),
        "includeDirectories": usage(Field::IncludeDirectories, Field::InterfaceIncludeDirectories),
        "linkLibraries": links,
    })
}

fn list_text(value: &[u8], source_relative: bool) -> impl Iterator<Item = String> {
    list::split(value, list::Empty::Drop)
        .into_iter()
        .map(move |value| {
            if source_relative {
                display_source(&value)
            } else {
                text(&value)
            }
        })
}

fn generate(project: &Evaluated) -> Result<BTreeMap<String, String>, String> {
    let result = &project.result;
    let host = project.host.as_ref();
    let build = &project.build;
    let model = &result.model;
    if result.completion.is_partial() {
        return Err("Configure stopped before it could generate the build files.".into());
    }
    let generation = result
        .generation
        .as_ref()
        .ok_or("The configure result contains no build graph.")?;
    let generator = project.generator;
    match generator {
        GeneratorKind::Ninja => {
            rcmake_gen_ninja::generate_single_config_with_model(generation, host, build, model)
        }
        GeneratorKind::NinjaMultiConfig => {
            rcmake_gen_ninja::generate_multi_config_with_model(generation, host, build, model)
        }
        _ => return Err("Select Ninja or Ninja Multi-Config.".into()),
    }
    .map_err(|error| error.to_string())?;
    let definition = |name: &[u8]| model.definition(DirId::ROOT, name);
    if definition(b"CMAKE_EXPORT_COMPILE_COMMANDS").is_some_and(|value| truth::is_on(value))
        && let Some(graph) = generation.graphs.first()
    {
        let content = compile_commands::render_for_generator(
            generation,
            graph,
            &path::from_native_cmake(build),
            generator,
        )
        .map_err(|error| error.to_string())?;
        write_if_changed(host, &build.join("compile_commands.json"), &content)
            .map_err(|error| error.to_string())?;
    }
    if !definition(b"CMAKE_SKIP_INSTALL_RULES").is_some_and(|value| truth::is_on(value)) {
        let prefix = definition(b"CMAKE_STAGING_PREFIX")
            .or_else(|| definition(b"CMAKE_INSTALL_PREFIX"))
            .map_or(b"/usr/local".as_slice(), |value| value.as_slice());
        write_install(project, prefix)?;
    }
    build_files(host, build).map_err(|error| error.to_string())
}

fn write_install(project: &Evaluated, prefix: &[u8]) -> Result<(), String> {
    use rcmake_gen_common::{export, install_script};
    use rcmake_resolve::{InstallActionKind, InstallManifest};

    let generation = project
        .result
        .generation
        .as_ref()
        .ok_or("The configure result contains no build graph.")?;
    let host = project.host.as_ref();
    let build = &project.build;
    let mut manifest = InstallManifest::default();
    let mut export_files = Vec::new();
    for action in &generation.install.actions {
        if let InstallActionKind::Export { plan } = action.kind {
            let export = generation
                .exports
                .get(plan)
                .ok_or("The install export is missing.")?;
            let files = export::write_export_plan_with_prefix(
                export,
                &build.join("CMakeFiles/Export").join(plan.to_string()),
                Some(&action.destination),
                Some(prefix),
                host,
            )
            .map_err(|error| error.to_string())?;
            for file in files {
                let mut action = action.clone();
                action.kind = InstallActionKind::Export {
                    plan: export_files.len(),
                };
                export_files.push(path::from_native_cmake(&file));
                manifest.actions.push(action);
            }
        } else {
            manifest.actions.push(action.clone());
        }
    }
    for plan in generation
        .exports
        .iter()
        .filter(|plan| !plan.install && !plan.file.is_empty())
    {
        export::write_export_plan(plan, build, None, host).map_err(|error| error.to_string())?;
    }
    let options = install_script::InstallScriptOptions {
        prefix,
        configuration: project.build_type.as_bytes(),
        export_files: &export_files,
        ..Default::default()
    };
    install_script::write_install_script(&manifest, build, &options, host)
        .map_err(|error| error.to_string())
}

fn build_files(host: &dyn Host, build: &Path) -> rcmake_host::HostResult<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut directories = vec![PathBuf::new()];
    while let Some(directory) = directories.pop() {
        for entry in host.read_dir(&build.join(&directory))? {
            let relative = directory.join(path::to_native(&entry.name));
            match entry.kind {
                FileKind::Dir => directories.push(relative),
                FileKind::File => {
                    let content =
                        host.read(&build.join(&relative), ReadRange::All { limit: u64::MAX })?;
                    if let Ok(content) = String::from_utf8(content) {
                        files.insert(text(&path::from_native_cmake(&relative)), content);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(files)
}
