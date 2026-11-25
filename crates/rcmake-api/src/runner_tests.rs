//! Tests for configure evaluation and resolution.

#![forbid(unsafe_code)]

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
    let script = format!("{PROJECT}\nexecute_process(COMMAND skipped RESULT_VARIABLE result)\n");
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
                && denied_processes.first().and_then(|denial| denial.pipeline.first())
                    .is_some_and(|cmd| cmd.program == b"skipped")
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
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.body == b"Configure reached its limit of 60 commands."
        })
    );
    assert!(matches!(
        host.metadata(&host.initial_cwd().join("build/late"), Follow::No),
        Err(error) if error.kind == ErrorKind::NotFound
    ));
    Ok(())
}

#[test]
fn empty_foreach_uses_the_step_budget() -> Result<(), ApiError> {
    let script =
        format!("{PROJECT}\nforeach(item RANGE 1000)\nendforeach()\nadd_library(late INTERFACE)\n");
    let (_, request) = fixture(&script)?;
    let result = run_configure(request.step_budget(20))?;
    assert_eq!(result.model.targets().count(), 3);
    assert!(result.generation.is_none());
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.body == b"Configure reached its limit of 20 commands."
        })
    );
    Ok(())
}

#[test]
fn variable_history_tracks_scope_and_cache_writes() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_eval::VarOp;

    let script = format!(
        "{PROJECT}\n{}",
        r#"
set(TRACKED original)
function(write_value value)
  set(TRACKED local)
  set(TRACKED "${value}" PARENT_SCOPE)
endfunction()
write_value(first)
write_value(second)
set(TRACKED cache CACHE STRING "Test value" FORCE)
unset(TRACKED CACHE)
"#
    );
    let (_, request) = fixture(&script)?;
    let result = run_configure(request)?;
    let ledger = result
        .var_ledger
        .as_ref()
        .ok_or("Missing variable history.")?;
    let events: Vec<_> = ledger.history(b"TRACKED").collect();
    let [
        root,
        local_first,
        parent_first,
        local_second,
        parent_second,
        cache_set,
        cache_unset,
    ] = events.as_slice()
    else {
        return Err(format!("Unexpected variable history: {events:?}").into());
    };
    assert_ne!(local_first.scope_id, local_second.scope_id);
    assert_eq!(local_first.scope_depth, local_second.scope_depth);
    assert_eq!(parent_first.scope_id, root.scope_id);
    assert_eq!(parent_second.scope_id, root.scope_id);
    assert_eq!(parent_second.op, VarOp::ParentScope);
    assert_eq!(
        parent_second
            .old_value
            .as_ref()
            .map(|value| value.prefix.as_slice()),
        Some(b"first".as_slice())
    );
    assert_eq!(cache_set.op, VarOp::CacheSet);
    assert_eq!(cache_unset.op, VarOp::CacheUnset);
    assert_eq!(
        cache_unset
            .old_value
            .as_ref()
            .map(|value| value.prefix.as_slice()),
        Some(b"cache".as_slice())
    );
    assert!(
        events
            .windows(2)
            .all(|pair| { matches!(pair, [previous, next] if previous.seq < next.seq) })
    );
    Ok(())
}

#[test]
fn reports_aliases_callables_and_usage_writes() -> Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "{PROJECT}\n{}",
        r#"
function(set_flag item)
  target_compile_definitions(util INTERFACE "ITEM=${item}")
endfunction()
set_flag(first)
set_flag(second)
add_library(demo::util ALIAS util)
set(CHOICE first CACHE STRING "Select an item" FORCE)
mark_as_advanced(CHOICE)
set_property(CACHE CHOICE PROPERTY STRINGS first second)
"#,
    );
    let (_, request) = fixture(&script)?;
    let result = run_configure(request)?;
    let index = result.index.as_ref().ok_or("Missing index.")?;
    let alias = index
        .lookup_target(DirId::ROOT, b"demo::util")
        .ok_or("Missing alias.")?;
    let location = alias
        .location
        .as_ref()
        .ok_or("Missing alias declaration.")?;
    assert!(location.source_id.is_some() && location.range.is_some());
    let callable = index
        .lookup_callable(b"SET_FLAG")
        .ok_or("Missing function definition.")?;
    assert_eq!(callable.parameters, [rcmake_base::BString::from("item")]);
    let ledger = result
        .model
        .prop_ledger()
        .ok_or("Missing property history.")?;
    let (_, _, writes) = ledger
        .iter()
        .find(|(scope, name, _)| {
            **scope == rcmake_model::PropScope::Target(alias.id)
                && name.as_slice() == b"INTERFACE_COMPILE_DEFINITIONS"
        })
        .ok_or("Missing usage property writes.")?;
    assert_eq!(writes.len(), 2);
    assert_eq!(
        result.facts.cache_force.get(b"CHOICE".as_slice()),
        Some(&true)
    );
    let choice = result
        .model
        .cache()
        .get(b"CHOICE")
        .ok_or("Missing cache entry.")?;
    assert!(choice.advanced);
    assert_eq!(choice.help, "Select an item");
    Ok(())
}

#[test]
fn fatal_denial_in_preload_returns_one_process_request() -> Result<(), ApiError> {
    let (host, request) = fixture(PROJECT)?;
    host.seed_file(
        &host.initial_cwd().join("src/PreLoad.cmake"),
        b"execute_process(COMMAND skipped COMMAND_ERROR_IS_FATAL ANY)\n",
    )?;
    host.set_command_handler(|host, _, _| {
        host.env_set(b"RAN_PROCESS", Some(b"YES"));
        Ok(RunOutput::default())
    })?;
    assert!(matches!(
        run_configure(request.process_policy(ProcessPolicy::Deny)),
        Err(ApiError::DeniedProcess { commands }) if commands.len() == 1
    ));
    assert_eq!(host.env_get(b"RAN_PROCESS"), None);
    Ok(())
}
