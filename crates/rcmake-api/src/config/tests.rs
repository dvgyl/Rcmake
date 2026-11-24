use rcmake_host::{
    CommandSpec, Follow, Input, Output, PureHost, RunRequest, WriteMode as HostWrite,
};

use super::*;

fn workspace() -> (PathBuf, Arc<PureHost>) {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" }).join("workspace");
    let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
    (root, host)
}

#[test]
fn uses_request_overrides_and_resolves_relative_paths() -> Result<(), ApiError> {
    let (root, host) = workspace();
    let cancelled = CancelToken::default();
    cancelled.cancel();
    let services = Services {
        cancel: Some(cancelled),
        step_budget: Some(0),
        ..Services::default()
    };
    let request = ConfigureRequest::new("source", "build", "cmake", host)
        .services(services)
        .step_budget(50)
        .collect_index(true);
    assert!(matches!(request.validate(), Err(ApiError::Cancelled)));
    let prepared = request.cancel(CancelToken::default()).prepare()?;
    assert_eq!(prepared.source_dir, root.join("source"));
    assert_eq!(prepared.services.step_budget, Some(50));
    assert!(matches!(
        prepared.services.provenance,
        LedgerLevel::Full { per_name_cap: 16 }
    ));
    Ok(())
}

#[test]
fn captures_overlay_writes_and_process_denials() -> Result<(), ApiError> {
    let (root, host) = workspace();
    let prepared = ConfigureRequest::new(
        "source",
        "build",
        "cmake",
        Arc::clone(&host) as Arc<dyn Host>,
    )
    .write_mode(WriteMode::Overlay)
    .collect_index(true)
    .prepare()?;
    let session = prepared.host();
    let output = prepared.build_dir.join("build.ninja");
    session.create_dir_all(&prepared.build_dir)?;
    session.write(&output, b"overlay data", HostWrite::Truncate)?;
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "cc".into(),
            args: Vec::new(),
        }],
        cwd: root,
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: None,
        tee: false,
    };
    assert!(session.run_bounded(&request, 100).is_err());
    assert!(host.metadata(&output, Follow::No).is_err());
    let result = prepared.finish(ConfigureResult::default());
    assert!(matches!(result.completion, CompletionState::Partial {
        denied_processes, cancelled: false
    } if denied_processes.len() == 1));
    assert_eq!(
        result
            .overlay_diff
            .as_ref()
            .and_then(|overlay| overlay.read_file(&output)),
        Some(b"overlay data".as_slice()),
    );
    assert!(result.index.is_some());
    Ok(())
}
