use rcmake_host::{
    CommandSpec, Follow, Input, Output, PureHost, RunRequest, WriteMode as HostWrite,
};

use super::*;
use crate::{OverlayBytes, OverlayEntryKind};

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
        denied_processes, cancelled: false, omitted_denials: 0, omitted_denial_bytes: 0
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

#[test]
fn rejects_names_that_the_cache_writer_cannot_persist() {
    for (name, escaped) in [
        ("A\nB", r#""A\nB""#),
        ("A\rB", r#""A\rB""#),
        ("A\"B", r#""A\"B""#),
    ] {
        let (_, host) = workspace();
        let request = ConfigureRequest::new("source", "build", "cmake", host).define(CacheDefine {
            name: name.into(),
            ty: None,
            value: "value".into(),
        });
        assert!(matches!(
            request.prepare(),
            Err(ApiError::InvalidConfig { field: "defines", message })
                if message.contains(escaped)
        ));
    }
}

#[test]
fn describes_overlay_paths_and_bounds_captured_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let (root, base) = workspace();
    let mode = root.join("mode");
    let old = root.join("old");
    let moved = root.join("moved");
    let hard = root.join("hard");
    let link = root.join("link");
    let large = root.join("large");
    let dir = root.join("newdir");
    let target = Path::new("mode");
    base.create_dir_all(&root)?;
    base.write(&mode, b"unchanged", HostWrite::Truncate)?;
    base.write(&old, b"move me", HostWrite::Truncate)?;
    let prepared = ConfigureRequest::new("source", "build", "cmake", base)
        .write_mode(WriteMode::Overlay)
        .prepare()?;
    let host = prepared.host();
    host.set_permissions(&mode, 0o755)?;
    host.rename(&old, &moved)?;
    host.hard_link(&moved, &hard)?;
    host.symlink(target, &link)?;
    host.create_dir_all(&dir)?;
    host.write(&large, &vec![0; 1024 * 1024 + 1], HostWrite::Truncate)?;
    let result = prepared.finish(ConfigureResult::default());
    let diff = result.overlay_diff.ok_or("missing overlay diff")?;

    assert!(diff.changes.get(&dir).is_some_and(|change| {
        change.kind == OverlayEntryKind::Directory && change.bytes == OverlayBytes::NotApplicable
    }));
    assert!(diff.changes.get(&link).is_some_and(|change| {
        change.kind == OverlayEntryKind::Symlink && change.link_target.as_deref() == Some(target)
    }));
    assert!(diff.changes.get(&mode).is_some_and(|change| {
        change.kind == OverlayEntryKind::File
            && change.bytes == OverlayBytes::FromBase
            && change
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.mode == 0o755)
    }));
    assert!(
        diff.changes
            .get(&old)
            .is_some_and(|change| change.kind == OverlayEntryKind::Removed)
    );
    let inode = |path: &Path| {
        diff.changes
            .get(path)?
            .metadata
            .as_ref()
            .map(|metadata| (metadata.dev, metadata.ino))
    };
    assert!(inode(&moved).is_some());
    assert_eq!(inode(&moved), inode(&hard));
    assert_eq!(
        inode(&moved),
        diff.changes.get(&old).and_then(|change| {
            change
                .previous_metadata
                .as_ref()
                .map(|metadata| (metadata.dev, metadata.ino))
        })
    );
    assert_eq!(
        diff.changes.get(&large).map(|change| &change.bytes),
        Some(&OverlayBytes::OmittedByLimit)
    );
    assert_eq!(diff.audit.deleted_paths, 1);
    Ok(())
}
