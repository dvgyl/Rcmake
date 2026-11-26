use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{InterpConfig, RunMode};
use rcmake_host::{Follow, HostResult, PureHost};

use super::*;

fn path(value: &str) -> PathBuf {
    if cfg!(windows) {
        format!("C:{value}").into()
    } else {
        value.into()
    }
}

#[test]
fn native_runtime_shares_parsed_sources_with_all_speed_options_off() {
    let source = path("/project");
    let build = path("/build");
    let executable = path("/tools/rcmake");
    let host = Arc::new(PureHost::new(source.clone(), executable.clone()));
    let runtime = ConfigureRuntime::new(host, &source, &build, &Services::default(), true);
    assert!(runtime.native.is_none());
    let mut interp = Interp::new(
        InterpConfig::new(
            runtime.host(),
            RunMode::Configure,
            source.join("data"),
            executable,
            source,
        ),
        crate::register_all,
    );
    runtime.attach(&mut interp);
    let first = interp
        .cx()
        .parse_cache()
        .cloned()
        .expect("native parse cache");
    runtime.attach(&mut interp);
    assert!(
        interp
            .cx()
            .parse_cache()
            .is_some_and(|cache| Arc::ptr_eq(cache, &first))
    );
}

fn configure(
    host: Arc<PureHost>,
    source: &Path,
    build: &Path,
    services: &Services,
    allow_persistent: bool,
    complete: bool,
) -> (RunOutcome, u64) {
    let mut runtime = ConfigureRuntime::new(host, source, build, services, allow_persistent);
    let mut interp = Interp::new(
        InterpConfig {
            host: runtime.host(),
            services: Arc::new(services.clone()),
            mode: RunMode::Configure,
            cmake_root: source.join("data"),
            cmake_command: path("/tools/rcmake"),
            cwd: source.into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    let outcome = runtime.run(&mut interp, &[]);
    assert!(runtime.finish(complete).is_none());
    assert!(
        interp
            .cx()
            .project()
            .find_target(rcmake_model::DirId::ROOT, b"core")
            .is_some()
    );
    (outcome, interp.replay_hits())
}

#[test]
fn configure_publishes_only_after_completion() -> HostResult<()> {
    let source = path("/project");
    let build = path("/build");
    let command = path("/tools/rcmake");
    let host = Arc::new(PureHost::new(source.clone(), command.clone()));
    host.seed_file(&command, b"executable fixture")?;
    host.seed_file(
        &source.join("CMakeLists.txt"),
        b"set(VALUE 1)\nadd_library(core STATIC core.c)\n",
    )?;
    host.seed_file(&source.join("core.c"), b"int core(void) { return 0; }\n")?;
    let record = build.join("CMakeFiles/rcmake/configure.v1.bin");
    let mut services = Services::default();
    services.speed.incremental = true;

    let (outcome, hits) = configure(Arc::clone(&host), &source, &build, &services, false, true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(hits, 0);
    assert!(host.metadata(&record, Follow::No).is_err());

    let (outcome, hits) = configure(Arc::clone(&host), &source, &build, &services, true, false);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(hits, 0);
    assert!(host.metadata(&record, Follow::No).is_err());

    let (outcome, hits) = configure(Arc::clone(&host), &source, &build, &services, true, true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(hits, 0);
    assert!(host.metadata(&record, Follow::No).is_ok());

    let (outcome, hits) = configure(host, &source, &build, &services, true, true);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(hits, 1);
    Ok(())
}

#[cfg(all(feature = "native", any(unix, windows)))]
#[test]
fn platform_query_opt_out_installs_no_query_state() {
    use rcmake_eval::journal::ReplayReceipts;

    use crate::configure::platform_queries::PlatformQueries;

    let attach = |opt_out: Option<&[u8]>| {
        let source = path("/project");
        let build = path("/build");
        let executable = path("/tools/rcmake");
        let host = Arc::new(PureHost::new(source.clone(), executable.clone()));
        host.env_set(b"RCMAKE_NO_PLATFORM_QUERIES", opt_out);
        let mut services = Services::default();
        services.speed.incremental = true;
        let runtime = ConfigureRuntime::new(host, &source, &build, &services, true);
        let mut interp = Interp::new(
            InterpConfig::new(
                runtime.host(),
                RunMode::Configure,
                source.join("data"),
                executable,
                source,
            ),
            crate::register_all,
        );
        runtime.attach(&mut interp);
        let cx = interp.cx();
        (
            cx.session_data::<Arc<PlatformQueries>>().is_some(),
            cx.session_data::<Arc<ReplayReceipts>>().is_some(),
        )
    };
    assert_eq!(attach(Some(b"1")), (false, false));
    #[cfg(target_os = "macos")]
    assert!(attach(None).0, "the default installs the query memo");
    // Only the exact value `1` turns the queries off.
    assert_eq!(attach(Some(b"yes")), attach(None));
    assert!(super::native::platform_queries_disabled(&{
        let host = PureHost::default();
        host.env_set(b"RCMAKE_NO_PLATFORM_QUERIES", Some(b"1"));
        host
    }));
}
