use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{InterpConfig, RunMode};
use rcmake_host::{Follow, Host, HostResult, PureHost};

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

fn session_interp(runtime: &ConfigureRuntime, source: &Path, services: &Services) -> Interp {
    Interp::new(
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
    )
}

/// Configure once. Return the outcome, the hit count, and the interpreter.
fn configure_interp(
    host: Arc<PureHost>,
    source: &Path,
    build: &Path,
    services: &Services,
    allow_persistent: bool,
    complete: bool,
) -> (RunOutcome, u64, Interp) {
    let mut runtime = ConfigureRuntime::new(host, source, build, services, allow_persistent);
    let mut interp = session_interp(&runtime, source, services);
    let outcome = runtime.run(&mut interp, &[]);
    assert!(runtime.finish(complete).is_none());
    let hits = interp.replay_hits();
    (outcome, hits, interp)
}

fn configure(
    host: Arc<PureHost>,
    source: &Path,
    build: &Path,
    services: &Services,
    allow_persistent: bool,
    complete: bool,
) -> (RunOutcome, u64) {
    let (outcome, hits, mut interp) =
        configure_interp(host, source, build, services, allow_persistent, complete);
    let core = interp
        .cx()
        .project()
        .dirs()
        .find(|(_, dir)| dir.source_dir.ends_with(b"/core"))
        .map(|(id, _)| id);
    assert!(
        core.and_then(|dir| interp.cx().project().find_target(dir, b"core"))
            .is_some()
    );
    (outcome, hits)
}

fn seed_project(host: &PureHost, source: &Path, leaf: &[u8]) -> HostResult<()> {
    host.seed_file(&path("/tools/rcmake"), b"executable fixture")?;
    // The first `project()` ends the configure setup. Replay can restore only the
    // listfiles that run after it.
    host.seed_file(
        &source.join("CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 3.10)\nproject(demo NONE)\nadd_subdirectory(core)\n",
    )?;
    host.seed_file(&source.join("core/CMakeLists.txt"), leaf)?;
    for name in ["a.c", "a", "b.c"] {
        host.seed_file(
            &source.join("core").join(name),
            b"int f(void) { return 0; }\n",
        )?;
    }
    Ok(())
}

#[test]
fn configure_publishes_only_after_completion() -> HostResult<()> {
    let source = path("/project");
    let build = path("/build");
    let command = path("/tools/rcmake");
    let host = Arc::new(PureHost::new(source.clone(), command.clone()));
    seed_project(
        &host,
        &source,
        b"set(VALUE 1)\nadd_library(core STATIC core.c)\n",
    )?;
    host.seed_file(
        &source.join("core/core.c"),
        b"int core(void) { return 0; }\n",
    )?;
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

/// The directory sources and the resolved sources of `core`, in a stable text form.
fn source_state(interp: &mut Interp, host: &dyn Host) -> String {
    let mut cx = interp.cx();
    let model: &rcmake_model::ProjectModel = cx.project();
    let Some((dir, directory)) = model
        .dirs()
        .find(|(_, dir)| dir.source_dir.ends_with(b"/core"))
    else {
        return "no core directory".into();
    };
    let order = directory
        .props
        .get(rcmake_model::replay::SOURCE_ORDER_PROPERTY)
        .cloned();
    let target = model
        .find_target(dir, b"core")
        .and_then(|id| model.target(id))
        .map(|target| format!("{:?}", target.sources));
    let resolved = rcmake_resolve::resolve(model, host, &[]).map(|result| {
        result
            .targets
            .iter()
            .map(|target| format!("{:?}", target.compile_units))
            .collect::<Vec<_>>()
    });
    format!(
        "order={order:?}\nprops={:?}\nsources={:?}\ngenerated={:?}\ntarget={target:?}\nresolved={resolved:?}",
        directory.props, directory.source_props, directory.generated_sources,
    )
}

/// An admitted `add_library()` registers its sources in the directory: relative
/// records, absolute records, and source property entries. The resolver uses the
/// record order for ambiguous source names (`CMP0115` `OLD`). A replayed leaf must
/// leave the same directory state as a live configure.
#[test]
fn replayed_leaf_restores_directory_source_registrations() -> HostResult<()> {
    let source = path("/project");
    let build = path("/build");
    let host = Arc::new(PureHost::new(source.clone(), path("/tools/rcmake")));
    let leaf = "add_library(core STATIC a /project/core/b.c a.c)\n";
    seed_project(&host, &source, leaf.as_bytes())?;
    let mut services = Services::default();
    services.speed.incremental = true;
    let (outcome, hits, _) =
        configure_interp(Arc::clone(&host), &source, &build, &services, true, true);
    assert_eq!((outcome.exit_code, hits), (0, 0));
    let (outcome, hits, mut replayed) =
        configure_interp(Arc::clone(&host), &source, &build, &services, true, true);
    assert_eq!((outcome.exit_code, hits), (0, 1));
    let live_services = Services::default();
    let (outcome, hits, mut live) = configure_interp(
        Arc::clone(&host),
        &source,
        &build,
        &live_services,
        true,
        true,
    );
    assert_eq!((outcome.exit_code, hits), (0, 0));
    let replayed = source_state(&mut replayed, host.as_ref());
    assert!(replayed.contains("order=Some("), "{replayed}");
    assert_eq!(replayed, source_state(&mut live, host.as_ref()));
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
