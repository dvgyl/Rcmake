use std::time::Duration;

use rcmake_host::{
    CommandSpec, Follow, Input, Output, ProcessResult, PureHost, ReadRange, RunOutput, RunRequest,
};
use rcmake_store::{InputStamp, StoreConfig, Trace, file_digest};

use super::super::journal::{
    DependencyClosure, ImpureReason, JournalHost, JournalMode, ProcessVerifier, SearchObservation,
    depfile::make_dependencies,
};
use super::super::plan::DiagnosticStream;
use super::test_support::recipe;
use super::*;

/// Return an example successful test probe.
pub(crate) fn success() -> ObservedOutcome {
    ObservedOutcome {
        compiled: true,
        exit_code: 0,
        output: b"/work/scratch/cmTC_1".to_vec(),
        artifact: Some(Artifact {
            bytes: b"test executable".to_vec(),
            relative_path: "bin/cmTC_1".into(),
            mode: 0o755,
        }),
        diagnostics: vec![ProbeDiagnostic {
            stream: DiagnosticStream::Stderr,
            message: b"/work/source/probe.c: warning".to_vec(),
        }],
    }
}

#[test]
fn verifies_inputs_and_materializes_the_copy_file_blob() -> HostResult<()> {
    let host = PureHost::default();
    let store = StoreConfig::new("/cache".into());
    let cache = ProbeCache::new(StoreMode::ReadWrite);
    let scratch = Path::new("/work/scratch");
    let header = Path::new("/work/header.h");
    let compiler = Path::new("/work/compiler");
    host.create_dir_all(scratch)?;
    host.seed_file(header, b"old")?;
    host.seed_file(compiler, b"cc1")?;
    host.env_set(b"LANG", Some(b"C"));
    let mut original = recipe(&host, Path::new("/work"))?;
    original
        .tools
        .insert("cc".into(), file_digest(&host, compiler)?);
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    journal.record_dependency(header)?;
    journal.record_dependency(compiler)?;
    journal.env_get(b"LANG");
    let journal = journal.finish();
    assert!(cache.lookup(&host, &store, &original).is_none());
    assert_eq!(
        cache.publish(&host, &store, &original, &success(), &journal),
        ProbePublishOutcome::Published
    );

    let mut relocated = recipe(&host, Path::new("/other"))?;
    relocated.tools = original.tools.clone();
    let hit = cache
        .lookup(&host, &store, &relocated)
        .ok_or_else(|| HostError::unsupported("Expected a probe cache hit."))?;
    assert_eq!(hit.output, b"/other/scratch/cmTC_1");
    assert_eq!(
        hit.diagnostics.first().map(|diag| diag.message.as_slice()),
        Some(b"/other/source/probe.c: warning".as_slice())
    );
    let artifact = hit
        .artifact
        .ok_or_else(|| HostError::unsupported("Missing probe artifact."))?;
    assert_eq!(artifact.relative_path, Path::new("bin/cmTC_1"));
    let destination = Path::new("/work/copied/test");
    artifact.copy_to(&host, scratch, destination)?;
    assert_eq!(
        host.read(destination, ReadRange::All { limit: 32 })?,
        artifact.bytes
    );
    assert_eq!(host.metadata(destination, Follow::Yes)?.mode & 0o777, 0o755);

    host.env_set(b"LANG", Some(b"fr_FR"));
    assert!(cache.lookup(&host, &store, &original).is_none());
    host.env_set(b"LANG", Some(b"C"));
    host.seed_file(header, b"new")?;
    assert!(cache.lookup(&host, &store, &original).is_none());
    host.seed_file(header, b"old")?;
    host.seed_file(compiler, b"cc2")?;
    assert!(cache.lookup(&host, &store, &original).is_none());
    assert_eq!(
        cache.stats(),
        ProbeCacheStats {
            hits: 1,
            misses: 4,
            ..ProbeCacheStats::default()
        }
    );
    Ok(())
}

#[test]
fn enforces_bypass_purity_and_verify_mode() -> HostResult<()> {
    let host = PureHost::default();
    let store = StoreConfig::new("/cache".into());
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    let mut recipe = recipe(&host, Path::new("/work"))?;
    recipe.scope = ReuseScope::ExactBindings;
    let cache = ProbeCache::new(StoreMode::ReadWrite);
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    journal.env_set(b"LC_ALL", Some(b"C"));
    assert!(matches!(
        cache.publish(&host, &store, &recipe, &success(), &journal.finish()),
        ProbePublishOutcome::Skipped(SkipReason::Inputs(_))
    ));
    assert!(cache.lookup(&host, &store, &recipe).is_none());
    assert_eq!(cache.stats().impure, 1);
    host.env_set(b"LC_ALL", None);

    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?.finish();
    assert_eq!(
        cache.publish(&host, &store, &recipe, &success(), &journal),
        ProbePublishOutcome::Published
    );
    assert_eq!(cache.lookup(&host, &store, &recipe), Some(success()));
    recipe.eligibility = ProbeEligibility::Bypass(super::super::plan::BypassReason::Trace);
    assert!(cache.lookup(&host, &store, &recipe).is_none());
    assert_eq!(cache.stats().bypassed, 1);

    let verify = ProbeCache::for_session(ProbeCacheMode::Verify);
    let mut fresh = success();
    fresh.output.push(b'!');
    let site = VerifyContext {
        variable: b"HAVE_CC",
        file: Path::new("CMakeLists.txt"),
        line: 42,
        key: compute_probe_key(&recipe),
        cached_copy: None,
        fresh_copy: None,
    };
    assert!(verify.verify_mode(site, &success(), &fresh).is_some());
    assert_eq!(verify.stats().verify_mismatches, 1);
    Ok(())
}

#[test]
fn keys_preserve_argument_and_environment_boundaries() -> HostResult<()> {
    let host = PureHost::default();
    let mut original = recipe(&host, Path::new("/work"))?;
    let initial_key = compute_probe_key(&original);
    host.env_set(b"LC_ALL", Some(b""));
    let with_empty_locale = recipe(&host, Path::new("/work"))?;
    assert_ne!(compute_probe_key(&with_empty_locale), initial_key);

    original.flags = vec!["-Idir".into(), "-DSET".into()];
    let ordered_key = compute_probe_key(&original);
    original.flags.reverse();
    assert_ne!(compute_probe_key(&original), ordered_key);
    original.flags = vec!["-Idir-DSET".into()];
    assert_ne!(compute_probe_key(&original), ordered_key);
    Ok(())
}
#[test]
fn rejects_unverified_process_before_it_starts() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    let journal = JournalHost::new(&host, scratch, JournalMode::Speculate, None)?;
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "/work/compiler".into(),
            args: Vec::new(),
        }],
        cwd: scratch.to_owned(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(Duration::from_secs(1)),
        tee: false,
    };
    assert!(journal.run(&request, &mut |_, _| {}).is_err());
    let journal = journal.finish();
    assert_eq!(journal.processes(), &[request]);
    assert_eq!(journal.impure(), Some(ImpureReason::UnverifiedProcess));
    Ok(())
}

struct Compiler<'a> {
    host: &'a PureHost,
    request: &'a RunRequest,
    include: &'a Path,
    empty_trace: bool,
}

impl ProcessVerifier for Compiler<'_> {
    fn preflight(&self, request: &RunRequest) -> Option<Trace> {
        (request == self.request).then(Vec::new)
    }

    fn dependencies(
        &self,
        request: &RunRequest,
        output: &RunOutput,
        _preflight: &[InputStamp],
    ) -> Option<DependencyClosure> {
        if self.empty_trace {
            return Some(DependencyClosure {
                files: Default::default(),
                searches: Vec::new(),
            });
        }
        let missing = output.stderr == b"missing header";
        let relative = "header.h";
        let files = if missing {
            Default::default()
        } else {
            let bytes = self
                .host
                .read(&request.cwd.join("probe.d"), ReadRange::All { limit: 4096 })
                .ok()?;
            make_dependencies(&bytes, &request.cwd)?
        };
        Some(DependencyClosure {
            files,
            searches: vec![SearchObservation {
                root: self.include.to_owned(),
                relative: relative.into(),
                found: !missing,
            }],
        })
    }
}

fn compiler_request(scratch: &Path) -> RunRequest {
    RunRequest {
        pipeline: vec![CommandSpec {
            program: "/work/compiler".into(),
            args: vec!["-MD".into(), "-MF".into(), "probe.d".into()],
        }],
        cwd: scratch.to_owned(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(Duration::from_secs(1)),
        tee: false,
    }
}

fn set_compiler_handler(host: &PureHost) -> HostResult<()> {
    host.set_command_handler(|host, request, _| {
        let header = Path::new("/work/include/header.h");
        let (code, stderr) = match host.read(header, ReadRange::All { limit: 64 }) {
            Ok(bytes) => {
                host.write(
                    &request.cwd.join("probe.d"),
                    b"probe.o: /work/include/header.h\n",
                    WriteMode::Truncate,
                )?;
                (i32::from(bytes != b"good"), Vec::new())
            }
            Err(_) => (1, b"missing header".to_vec()),
        };
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(code)],
            stderr,
            ..RunOutput::default()
        })
    })
}

fn run_compiler(
    host: &PureHost,
    request: &RunRequest,
    verifier: &Compiler<'_>,
) -> HostResult<(ObservedOutcome, Journal)> {
    let journal = JournalHost::new(host, &request.cwd, JournalMode::Speculate, Some(verifier))?;
    let result = journal.run(request, &mut |_, _| {})?;
    let [ProcessResult::Exited(exit_code)] = result.results.as_slice() else {
        return Err(HostError::unsupported("Compiler process did not complete."));
    };
    let outcome = ObservedOutcome {
        compiled: *exit_code == 0,
        exit_code: *exit_code,
        output: result.stderr,
        artifact: None,
        diagnostics: Vec::new(),
    };
    Ok((outcome, journal.finish()))
}

#[test]
fn certifies_the_exact_process_and_rechecks_its_executable() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let compiler = Path::new("/work/compiler");
    let include = Path::new("/work/include");
    host.create_dir_all(scratch)?;
    host.seed_file(compiler, b"compiler-v1")?;
    host.seed_file(&include.join("header.h"), b"good")?;
    host.sleep(Duration::from_secs(3));
    set_compiler_handler(&host)?;
    let request = compiler_request(scratch);
    let mut verifier = Compiler {
        host: &host,
        request: &request,
        include,
        empty_trace: true,
    };
    let (_, journal) = run_compiler(&host, &request, &verifier)?;
    assert_eq!(journal.impure(), Some(ImpureReason::UnverifiedProcess));
    verifier.empty_trace = false;
    let (_, journal) = run_compiler(&host, &request, &verifier)?;
    assert_eq!(journal.processes(), &[request]);
    assert!(journal.verified_inputs(&host).is_some());
    host.seed_file(compiler, b"compiler-v2")?;
    assert!(journal.verified_inputs(&host).is_none());
    Ok(())
}

#[test]
fn header_changes_invalidate_passes_and_failed_searches() -> HostResult<()> {
    for (initial, replacement) in [
        (Some(b"good".as_slice()), b"bad".as_slice()),
        (None, b"good"),
    ] {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        let include = Path::new("/work/include");
        let header = include.join("header.h");
        host.create_dir_all(scratch)?;
        host.create_dir_all(include)?;
        host.seed_file(Path::new("/work/compiler"), b"compiler")?;
        if let Some(bytes) = initial {
            host.seed_file(&header, bytes)?;
        }
        host.sleep(Duration::from_secs(3));
        set_compiler_handler(&host)?;
        let request = compiler_request(scratch);
        let verifier = Compiler {
            host: &host,
            request: &request,
            include,
            empty_trace: false,
        };
        let (first, journal) = run_compiler(&host, &request, &verifier)?;
        let recipe = recipe(&host, Path::new("/work"))?;
        let config = StoreConfig::new("/cache".into());
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        assert_eq!(first.compiled, initial.is_some());
        assert_eq!(
            cache.publish(&host, &config, &recipe, &first, &journal),
            ProbePublishOutcome::Published
        );
        assert_eq!(cache.lookup(&host, &config, &recipe), Some(first));
        host.seed_file(&header, replacement)?;
        assert!(cache.lookup(&host, &config, &recipe).is_none());
        let (fresh, racy_journal) = run_compiler(&host, &request, &verifier)?;
        assert_eq!(fresh.compiled, initial.is_none());
        assert_eq!(racy_journal.impure(), Some(ImpureReason::ChangedInput));
        host.sleep(Duration::from_secs(3));
        let (fresh, stable_journal) = run_compiler(&host, &request, &verifier)?;
        assert_eq!(
            cache.publish(&host, &config, &recipe, &fresh, &stable_journal),
            ProbePublishOutcome::Published
        );
        assert_eq!(cache.lookup(&host, &config, &recipe), Some(fresh));
    }
    Ok(())
}

#[test]
fn replaces_a_result_after_a_supplemental_check_changes() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let link = Path::new("/work/link");
    host.create_dir_all(scratch)?;
    let recipe = recipe(&host, Path::new("/work"))?;
    let config = StoreConfig::new("/cache".into());
    let cache = ProbeCache::new(StoreMode::ReadWrite);
    for target in ["/work/first", "/work/second"] {
        host.symlink(Path::new(target), link)?;
        assert!(cache.lookup(&host, &config, &recipe).is_none());
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        let mut outcome = success();
        outcome.output = path::from_native(&journal.read_link(link)?).to_vec();
        let journal = journal.finish();
        assert!(
            journal
                .verified_inputs(&host)
                .is_some_and(|(trace, _)| trace.is_empty())
        );
        assert_eq!(
            cache.publish(&host, &config, &recipe, &outcome, &journal),
            ProbePublishOutcome::Published
        );
        assert_eq!(cache.lookup(&host, &config, &recipe), Some(outcome));
        host.remove_file(link)?;
    }
    Ok(())
}

#[test]
fn rejects_leaks_and_can_replace_materialized_files() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    let recipe = recipe(&host, Path::new("/work"))?;
    let config = StoreConfig::new("/cache".into());
    let cache = ProbeCache::new(StoreMode::ReadWrite);
    let mut outcome = success();
    let Some(artifact) = &mut outcome.artifact else {
        return Err(HostError::unsupported("Missing artifact."));
    };
    let file = artifact.materialize(&host, scratch)?;
    artifact.bytes = b"embedded /work/scratch/probe.c".to_vec();
    artifact.materialize(&host, scratch)?;
    assert_eq!(
        host.read(&file, ReadRange::All { limit: 128 })?,
        artifact.bytes
    );
    assert!(artifact.copy_to(&host, scratch, scratch).is_err());
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?.finish();
    assert_eq!(
        cache.publish(&host, &config, &recipe, &outcome, &journal),
        ProbePublishOutcome::Skipped(SkipReason::Outcome)
    );
    Ok(())
}

#[test]
fn missing_nested_header_invalidates_when_a_shadow_appears() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let root = Path::new("/work/include");
    let relative = Path::new("sub/header.h");
    host.create_dir_all(scratch)?;
    host.create_dir_all(&root.join("sub"))?;
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    assert!(!journal.record_search(root, relative)?);
    let journal = journal.finish();
    assert!(matches!(
        journal
            .verified_inputs(&host)
            .map(|(trace, _)| trace.as_slice()),
        Some([InputStamp::NegativeLookup { .. }])
    ));
    host.seed_file(&root.join(relative), b"shadow")?;
    assert!(journal.verified_inputs(&host).is_none());
    Ok(())
}

#[test]
fn stable_exact_bindings_ignore_previous_scratch_names() -> HostResult<()> {
    let host = PureHost::default();
    let mut first = recipe(&host, Path::new("/work"))?;
    first.scope = ReuseScope::ExactBindings;
    let mut second = first.clone();
    second.bindings = second
        .bindings
        .with_scratch(Path::new("/work/other-scratch"))
        .and_then(|bindings| bindings.with_target(b"cmTC_other"))
        .ok_or_else(|| HostError::unsupported("Invalid test bindings."))?;
    assert_ne!(compute_probe_key(&first), compute_probe_key(&second));
    let root = Path::new("/work/scratch");
    assert_eq!(
        first.bind_exact_scratch(root),
        second.bind_exact_scratch(root)
    );
    assert_eq!(compute_probe_key(&first), compute_probe_key(&second));
    Ok(())
}

#[test]
fn a_store_error_does_not_change_the_probe_result() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    host.seed_file(Path::new("/cache"), b"This path is a file.")?;
    let config = StoreConfig::new("/cache".into());
    let cache = ProbeCache::new(StoreMode::ReadWrite);
    let recipe = recipe(&host, Path::new("/work"))?;
    let outcome = success();
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?.finish();
    assert!(matches!(
        cache.publish(&host, &config, &recipe, &outcome, &journal),
        ProbePublishOutcome::StoreError(_)
    ));
    assert_eq!(cache.stats().store_errors, 1);
    assert!(outcome.compiled);
    Ok(())
}
