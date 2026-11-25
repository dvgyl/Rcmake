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
    runtime.finish(complete);
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
