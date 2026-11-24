//! Evaluate a configure request and resolve its build graph through a host.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind};
use rcmake_eval::{Interp, InterpConfig, RunMode, StopReason};
use rcmake_genex::ConfigId;
use rcmake_host::{FileKind, Follow, Host, path};
use rcmake_model::{CacheEntry, CacheType, DirId, GeneratorKind, GeneratorSpec, ProjectModel};
use rcmake_resolve::{ResolveOptions, ide::IdeFlavor, resolve_with_options};

use crate::{
    ApiError, CompletionState, ConfigureRequest, ConfigureResult, LedgerLevel, PreparedConfigure,
    ResolvedEntry, VarLedgerSnapshot,
};

/// Evaluate CMakeLists.txt and resolve the configured project's build graphs.
///
/// All file and process operations use the prepared session host. The result
/// contains the evaluated model, any resolved graphs, and the requested index.
/// A denied process makes the result partial. A stop during evaluation returns
/// the facts collected so far when the evaluator reached the root listfile.
/// Cancellation and command limits stop evaluation before resolution.
///
/// # Errors
///
/// Return an error for an invalid request, an unavailable host operation, or an
/// evaluation or resolution failure that cannot provide a usable snapshot.
pub fn run_configure(req: ConfigureRequest) -> Result<ConfigureResult, ApiError> {
    let prepared = req.prepare()?;
    let host = prepared.host();
    let generator = select_generator(&prepared, host.as_ref())?;
    let command = host.current_exe()?;
    let command = host.canonicalize(&command).unwrap_or(command);

    let mut interp = Interp::new(
        InterpConfig {
            host,
            mode: RunMode::Configure,
            cmake_root: prepared.cmake_root.clone(),
            cmake_command: command,
            cwd: prepared.cwd.clone(),
            limits: prepared.limits.clone(),
            diag: prepared.diag.clone(),
            defines: prepared.defines.clone(),
            script_args: Vec::new(),
            services: prepared.services.clone(),
        },
        rcmake_commands::register_all,
    );

    let host = prepared.host();
    let preloads = preload_files(&prepared, host.as_ref());
    initialize(&mut interp, &prepared, generator);
    let outcome = interp.run_configure(&prepared.source_dir, &prepared.build_dir, &preloads);
    let stop = interp.stop_reason().or_else(|| {
        prepared
            .services
            .cancel
            .as_ref()
            .filter(|token| token.is_cancelled())
            .map(|_| StopReason::Cancelled)
    });

    let var_ledger = interp
        .take_var_ledger()
        .map(|ledger| VarLedgerSnapshot::from_ledger(ledger, prepared.services.provenance.clone()));
    let mut result = ConfigureResult {
        diagnostics: interp.diag_engine().diagnostics().to_vec(),
        model: std::mem::take(interp.cx().project()),
        var_ledger,
        ..ConfigureResult::default()
    };
    let root = result.model.dir(DirId::ROOT);
    let has_snapshot = root.final_policies.is_some() || result.model.targets().next().is_some();
    match stop {
        Some(StopReason::Cancelled) if !has_snapshot => return Err(ApiError::Cancelled),
        Some(StopReason::Cancelled) => result.completion = partial(true),
        Some(StopReason::StepBudgetExceeded { limit }) if !has_snapshot => {
            return Err(ApiError::StepBudgetExceeded { limit });
        }
        Some(StopReason::StepBudgetExceeded { limit }) => {
            result.completion = partial(false);
            result.diagnostics.push(Diagnostic {
                kind: MessageKind::Error,
                effect: Effect::Fatal,
                category: None,
                policy: None,
                location: Location::None,
                body: ApiError::StepBudgetExceeded { limit }.to_string().into(),
                annotations: None,
            });
        }
        Some(StopReason::DeniedProcess) if !has_snapshot => {
            return Err(ApiError::DeniedProcess {
                commands: prepared.denied_processes(),
            });
        }
        Some(StopReason::DeniedProcess) => result.completion = partial(false),
        None if outcome.exit_code == 0 => resolve_project(&prepared, generator, &mut result)?,
        None => {
            return Err(ApiError::ConfigureFailed {
                exit_code: outcome.exit_code,
                diagnostics: result.diagnostics,
            });
        }
    }
    Ok(prepared.finish(result))
}

fn partial(cancelled: bool) -> CompletionState {
    CompletionState::Partial {
        denied_processes: Vec::new(),
        omitted_denials: 0,
        omitted_denial_bytes: 0,
        cancelled,
    }
}

fn select_generator(
    prepared: &PreparedConfigure,
    host: &dyn Host,
) -> Result<GeneratorKind, ApiError> {
    let saved = prepared
        .defines
        .iter()
        .rev()
        .find(|def| def.name == b"CMAKE_GENERATOR")
        .map(|def| &def.value);
    let saved_kind = saved
        .map(|name| {
            GeneratorKind::from_name(name).ok_or_else(|| ApiError::InvalidConfig {
                field: "generator",
                message: format!("Unknown generator {name}."),
            })
        })
        .transpose()?;
    if let (Some(requested), Some(saved)) = (prepared.generator, saved_kind)
        && requested != saved
    {
        return Err(ApiError::InvalidConfig {
            field: "generator",
            message: format!(
                "The build directory uses {}. Select a build directory for {}.",
                saved.name(),
                requested.name()
            ),
        });
    }
    Ok(prepared
        .generator
        .or(saved_kind)
        .or_else(|| {
            host.env_get(b"CMAKE_GENERATOR")
                .and_then(|name| GeneratorKind::from_name(&name))
                .filter(|kind| kind.host_available())
        })
        .unwrap_or(GeneratorKind::UnixMakefiles))
}

fn initialize(interp: &mut Interp, prepared: &PreparedConfigure, generator: GeneratorKind) {
    let mut cx = interp.cx();
    let cache = cx.cache();
    let setting = |key: &[u8]| {
        cache
            .get(key)
            .map(|entry| entry.value.clone())
            .unwrap_or_default()
    };
    let spec = GeneratorSpec {
        toolset: setting(b"CMAKE_GENERATOR_TOOLSET"),
        platform: setting(b"CMAKE_GENERATOR_PLATFORM"),
        instance: setting(b"CMAKE_GENERATOR_INSTANCE"),
        ..GeneratorSpec::default()
    };
    for (key, value) in [
        ("CMAKE_GENERATOR", BString::from(generator.name())),
        (
            "CMAKE_HOME_DIRECTORY",
            path::from_native_cmake(&prepared.source_dir),
        ),
    ] {
        cache.insert(
            key.into(),
            CacheEntry {
                value,
                ty: CacheType::Internal,
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
    let project = cx.project();
    project.generator = Some(generator);
    project.generator_spec = spec;
}

fn preload_files(prepared: &PreparedConfigure, host: &dyn Host) -> Vec<PathBuf> {
    [&prepared.source_dir, &prepared.build_dir]
        .into_iter()
        .enumerate()
        .filter(|(index, dir)| *index == 0 || *dir != &prepared.source_dir)
        .map(|(_, dir)| dir.join("PreLoad.cmake"))
        .filter(|path| {
            host.metadata(path, Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
        })
        .collect()
}

fn resolve_project(
    prepared: &PreparedConfigure,
    generator: GeneratorKind,
    result: &mut ConfigureResult,
) -> Result<(), ApiError> {
    let configs = configurations(&result.model, generator);
    let options = ResolveOptions {
        provenance: !matches!(prepared.services.provenance, LedgerLevel::Off),
        ide: match generator {
            GeneratorKind::Xcode => Some(IdeFlavor::Xcode),
            GeneratorKind::VisualStudio(_) => Some(IdeFlavor::VisualStudio),
            _ => None,
        },
    };
    let host = prepared.host();
    let mut generation = resolve_with_options(&result.model, host.as_ref(), &configs, &options)
        .map_err(|error| ApiError::GenerateFailed {
            message: error.to_string(),
            diagnostics: std::mem::take(&mut result.diagnostics),
        })?;
    result.facts.resolved_entries = std::mem::take(&mut generation.provenance_entries)
        .into_iter()
        .map(|entry| ResolvedEntry {
            key: entry.key,
            value: entry.value,
            link_targets: entry.link_targets,
        })
        .collect();
    result.generation = Some(generation);
    Ok(())
}

fn configurations(model: &ProjectModel, generator: GeneratorKind) -> Vec<ConfigId> {
    if !generator.multi_config() {
        // The resolver reads CMAKE_BUILD_TYPE for an empty request.
        return Vec::new();
    }
    match model.definition(DirId::ROOT, b"CMAKE_CONFIGURATION_TYPES") {
        Some(value) => list::split(value, list::Empty::Drop)
            .into_iter()
            .map(ConfigId)
            .collect(),
        None => generator
            .default_configuration_types()
            .iter()
            .map(|name| ConfigId((*name).into()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::sync::Arc;

    use rcmake_host::{PureHost, ReadRange, RunOutput};

    use super::*;
    use crate::{CancelToken, ProcessPolicy, Services, WriteMode};

    const PROJECT: &str = r#"
cmake_minimum_required(VERSION 4.5)
project(RunnerTest LANGUAGES NONE)
add_library(core INTERFACE)
add_library(util INTERFACE)
target_link_libraries(util INTERFACE core)
add_custom_target(app DEPENDS util)
"#;

    fn fixture(script: &str) -> Result<(Arc<PureHost>, ConfigureRequest), ApiError> {
        let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
        let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
        host.seed_file(&root.join("src/CMakeLists.txt"), script.as_bytes())?;
        let request = ConfigureRequest::new("src", "build", "cmake", host.clone())
            .collect_index(true)
            .services(Services {
                provenance: LedgerLevel::Full { per_name_cap: 16 },
                ..Services::default()
            });
        Ok((host, request))
    }

    #[test]
    fn resolves_three_targets_and_indexes_them() -> Result<(), ApiError> {
        let (_, request) = fixture(PROJECT)?;
        let result = run_configure(request)?;
        assert_eq!(result.completion, CompletionState::Complete);
        assert_eq!(result.model.targets().count(), 3);
        assert!(result.generation.is_some());
        let names = [b"core".as_slice(), b"util", b"app"];
        assert!(result.index.as_ref().is_some_and(|index| {
            names
                .iter()
                .all(|name| index.lookup_target(DirId::ROOT, name).is_some())
        }));
        Ok(())
    }

    #[test]
    fn overlay_keeps_files_off_the_base_host() -> Result<(), ApiError> {
        let script =
            format!("{PROJECT}\nfile(WRITE \"${{CMAKE_BINARY_DIR}}/value.txt\" \"overlay\")\n");
        let (host, request) = fixture(&script)?;
        let output = host.initial_cwd().join("build/value.txt");
        let result = run_configure(request.write_mode(WriteMode::Overlay))?;
        assert_eq!(
            result
                .overlay_diff
                .as_ref()
                .and_then(|diff| diff.read_file(&output)),
            Some(b"overlay".as_slice())
        );
        assert!(matches!(
            host.read(&output, ReadRange::All { limit: 32 }),
            Err(error) if error.kind == ErrorKind::NotFound
        ));
        Ok(())
    }

    #[test]
    fn denied_execution_marks_an_available_result_partial() -> Result<(), ApiError> {
        let script =
            format!("{PROJECT}\nexecute_process(COMMAND skipped RESULT_VARIABLE result)\n");
        let (host, request) = fixture(&script)?;
        host.set_command_handler(|host, _, _| {
            host.env_set(b"RAN_PROCESS", Some(b"YES"));
            Ok(RunOutput::default())
        })?;
        let result = run_configure(request.process_policy(ProcessPolicy::Deny))?;
        assert!(matches!(
            result.completion,
            CompletionState::Partial { denied_processes, cancelled: false, .. }
                if denied_processes.len() == 1
                    && denied_processes[0].pipeline.first().is_some_and(|cmd| cmd.program == b"skipped")
        ));
        assert_eq!(host.env_get(b"RAN_PROCESS"), None);
        assert!(result.index.is_some_and(|index| index.is_provisional()));
        Ok(())
    }

    #[test]
    fn fatal_denial_keeps_the_targets_evaluated_so_far() -> Result<(), ApiError> {
        let script = format!(
            "{PROJECT}\nexecute_process(COMMAND skipped COMMAND_ERROR_IS_FATAL ANY)\n\
             add_library(late INTERFACE)\n"
        );
        let (_, request) = fixture(&script)?;
        let result = run_configure(request.process_policy(ProcessPolicy::Deny))?;
        assert!(matches!(
            result.completion,
            CompletionState::Partial { denied_processes, cancelled: false, .. }
                if denied_processes.len() == 1
        ));
        assert_eq!(result.model.targets().count(), 3);
        assert!(result.generation.is_none());
        Ok(())
    }

    #[test]
    fn unrelated_configure_failure_retains_its_error_after_denial() -> Result<(), ApiError> {
        let script = format!(
            "{PROJECT}\nexecute_process(COMMAND skipped RESULT_VARIABLE result)\n\
             message(FATAL_ERROR \"Project stopped\")\n"
        );
        let (_, request) = fixture(&script)?;
        assert!(matches!(
            run_configure(request.process_policy(ProcessPolicy::Deny)),
            Err(ApiError::ConfigureFailed { diagnostics, .. })
                if diagnostics.iter().any(|diagnostic| diagnostic.body == b"Project stopped")
        ));
        Ok(())
    }

    #[test]
    fn cancellation_stops_the_next_command_and_resolution() -> Result<(), ApiError> {
        let script = format!(
            "{PROJECT}\nexecute_process(COMMAND cancel)\nfile(WRITE \"${{CMAKE_BINARY_DIR}}/late\" late)\n"
        );
        let (host, request) = fixture(&script)?;
        let cancel = CancelToken::default();
        let token = cancel.clone();
        host.set_command_handler(move |_, _, _| {
            token.cancel();
            Ok(RunOutput::default())
        })?;
        let result = run_configure(request.cancel(cancel))?;
        assert!(matches!(
            result.completion,
            CompletionState::Partial {
                cancelled: true,
                ..
            }
        ));
        assert!(result.generation.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(matches!(
            host.metadata(&host.initial_cwd().join("build/late"), Follow::No),
            Err(error) if error.kind == ErrorKind::NotFound
        ));
        Ok(())
    }

    #[test]
    fn step_budget_stops_a_loop_and_reports_the_limit() -> Result<(), ApiError> {
        let script = format!(
            "{PROJECT}\nforeach(item RANGE 100)\n  set(most_recent \"${{item}}\")\nendforeach()\n\
             file(WRITE \"${{CMAKE_BINARY_DIR}}/late\" late)\n"
        );
        let (host, request) = fixture(&script)?;
        let result = run_configure(request.step_budget(60))?;
        assert!(matches!(
            result.completion,
            CompletionState::Partial {
                cancelled: false,
                ..
            }
        ));
        assert!(result.generation.is_none());
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.body == b"Configure reached its limit of 60 commands."
        }));
        assert!(matches!(
            host.metadata(&host.initial_cwd().join("build/late"), Follow::No),
            Err(error) if error.kind == ErrorKind::NotFound
        ));
        Ok(())
    }
}
