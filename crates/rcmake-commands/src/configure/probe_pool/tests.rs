use super::*;
use rcmake_host::PureHost;

fn work() -> &'static Path {
    Path::new(if cfg!(windows) { "C:/work" } else { "/work" })
}

/// A file under [`work`] with `/` separators only, as the dependency trace records it.
fn work_file(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}/{name}", work().display()))
}

#[test]
fn limits_total_jobs_and_uses_explicit_settings_first() {
    let host = PureHost::default();
    assert_eq!(job_limit(&host, 0, None).effective, 1);
    host.env_set(b"RCMAKE_PROBE_JOBS", Some(b"100"));
    assert_eq!(job_limit(&host, 4 * GIB, None).effective, 2);
    assert_eq!(
        job_limit(&host, 4 * GIB, None).clamp,
        Some(JobClamp::Memory)
    );
    let limit = job_limit(&host, 16 * GIB, NonZeroUsize::new(1));
    assert_eq!(limit.requested, 1);
    assert_eq!(limit.effective, 1);
    assert_eq!(limit.clamp, None);
}

#[test]
fn claims_and_cleans_distinct_scratch_directories() -> HostResult<()> {
    let host = PureHost::default();
    let root = work().join("spec");
    host.create_dir_all(&root)?;
    {
        let first = Scratch::create(&host, &root)?;
        let second = Scratch::create(&host, &root)?;
        assert_ne!(first.path, second.path);
        assert_eq!(host.read_dir(&root)?.len(), 4);
    }
    assert!(host.read_dir(&root)?.is_empty());
    Ok(())
}
struct TestCompiler;

impl ProcessVerifier for TestCompiler {
    fn preflight(&self, request: &RunRequest) -> Option<rcmake_store::Trace> {
        let [command] = request.pipeline.as_slice() else {
            return None;
        };
        (command.program == path::from_native_cmake(&work().join("compiler"))).then(Vec::new)
    }

    fn dependencies(
        &self,
        _: &RunRequest,
        _: &rcmake_host::RunOutput,
    ) -> Option<super::super::try_compile::journal::DependencyClosure> {
        use super::super::try_compile::journal::DependencyClosure;
        Some(DependencyClosure {
            files: [work_file("header.h")].into(),
            searches: Vec::new(),
        })
    }
}

#[test]
fn owns_the_request_and_recovers_after_a_worker_panics() -> HostResult<()> {
    use super::super::try_compile::{
        cache::test_support::recipe,
        plan::{SpeculativeRun, Templated},
    };
    use std::sync::mpsc;

    let host = PureHost::default();
    let root = work().join("spec");
    host.create_dir_all(&root)?;
    host.seed_file(&work_file("header.h"), b"header")?;
    host.seed_file(&work_file("compiler"), b"compiler")?;
    host.sleep(Duration::from_secs(3));
    host.env_set(b"LANG", Some(b"C"));
    let environment = host.env_all();
    let config = StoreConfig::new(work().join("cache"));
    let cache = ProbeCache::new(rcmake_store::StoreMode::ReadWrite);
    let context = WorkerContext {
        host: &host,
        store: &config,
        cache: &cache,
        scratch_root: &root,
        process_verifier: Some(&TestCompiler),
    };
    let mut good = recipe(&host, work())?;
    good.speculative_run = Some(SpeculativeRun {
        program: path::from_native_cmake(&work().join("compiler")),
        args: Vec::new(),
        artifact: None,
        timeout: Duration::from_secs(4),
    });
    let mut panicking = good.clone();
    let arg = Templated::literal(b"panic fixture")
        .ok_or_else(|| HostError::unsupported("Invalid test argument."))?;
    if let Some(run) = &mut panicking.speculative_run {
        run.args.push(arg);
    }
    let (started, jobs) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    host.set_command_handler(move |_, request, _| {
        assert_eq!(request.env, [(BString::from("LANG"), BString::from("C"))]);
        let [program] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Missing test process."));
        };
        let _ = started.send(());
        if !released
            .lock()
            .is_ok_and(|rx| rx.recv_timeout(Duration::from_secs(2)).is_ok())
        {
            return Err(HostError::unsupported("Test process did not resume."));
        }
        assert!(program.args.is_empty(), "Panicking worker fixture.");
        Ok(rcmake_host::RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: path::from_native_cmake(&request.cwd.join("probe.c")).to_vec(),
            stderr: Vec::new(),
        })
    })?;

    ProbePool::scoped(&context, 4 * GIB, None, CancelToken::default(), |pool| {
        assert_eq!(pool.limit().effective, 2);
        let failure_key = compute_probe_key(&panicking);
        assert!(
            pool.queue()
                .submit(panicking.clone(), environment.clone(), None, 0)
        );
        jobs.recv_timeout(Duration::from_secs(2))
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        assert!(!pool.queue().submit(panicking, environment.clone(), None, 0));
        release
            .send(())
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        pool.queue().wait_for(failure_key);
        assert_eq!(pool.stats().failures, 1);

        assert!(!pool.queue().submit(good.clone(), Vec::new(), None, 0));
        assert!(pool.queue().submit(good.clone(), environment, None, 0));
        jobs.recv_timeout(Duration::from_secs(2))
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        release
            .send(())
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        pool.queue().wait_for(compute_probe_key(&good));
        assert_eq!(pool.stats().published, 1);
        assert_eq!(cache.stats(), Default::default());
        let hit = cache
            .lookup(&host, &config, &good)
            .ok_or_else(|| HostError::unsupported("Missing worker result."))?;
        assert_eq!(
            hit.output,
            path::from_native_cmake(&work().join("scratch/probe.c")).as_slice()
        );
        Ok(())
    })?;
    assert!(host.read_dir(&root)?.is_empty());
    Ok(())
}
