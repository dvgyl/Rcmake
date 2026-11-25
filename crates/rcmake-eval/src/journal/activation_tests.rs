use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, PureHost};
use rcmake_model::{CacheEntry, CacheType, DirId};

use super::*;
use crate::{CmdError, CmdFlags, Cx, InterpConfig, Registry, Services, SubdirOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, values @ ..] = args else {
        return Err(CmdError::status("Missing variable name."));
    };
    cx.set_var(
        &name.value,
        list::join(values.iter().map(|arg| arg.value.as_slice())),
    );
    Ok(Flow::Normal)
}

fn subdirectory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name] = args else {
        return Err(CmdError::status("Missing directory name."));
    };
    let name = path::to_native(&name.value);
    cx.run_subdirectory(&name, &name, SubdirOptions::default())?;
    Ok(Flow::Normal)
}

fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("set", set, flags);
    registry.add("add_subdirectory", subdirectory, flags);
    registry.mark_replay_commands();
}

struct Fixture {
    host: Arc<PureHost>,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
        host.seed_file(&root.join("rcmake"), b"executable identity")?;
        Ok(Self { host, root })
    }

    fn script(&self, name: &str, bytes: &[u8]) -> TestResult {
        self.host
            .seed_file(&self.root.join("src").join(name), bytes)?;
        Ok(())
    }

    fn interp(&self) -> Interp {
        Interp::new(
            InterpConfig {
                host: self.host.clone(),
                cwd: self.root.clone(),
                cmake_command: self.root.join("rcmake"),
                cmake_root: self.root.join("share"),
                mode: RunMode::Configure,
                defines: Vec::new(),
                limits: Default::default(),
                diag: Default::default(),
                services: Arc::new(Services::default()),
                script_args: Vec::new(),
            },
            register,
        )
    }

    fn run(&self, interp: &mut Interp, manifest: Option<&[u8]>) -> Option<Vec<u8>> {
        let (outcome, record) = interp.configure_incremental(
            &self.root.join("src"),
            &self.root.join("build"),
            &[],
            manifest,
        );
        assert_eq!(outcome.exit_code, 0);
        record
    }
}

#[test]
fn whole_replay_checks_the_executable_source_cache_and_scope() -> TestResult {
    let f = Fixture::new()?;
    let file = "CMakeLists.txt";
    f.script(file, b"set(A one)\nset(B two)\n")?;
    let mut first = f.interp();
    first.track_variable_reads(["A".into()]);
    let entry = CacheEntry {
        value: "test".into(),
        ty: CacheType::String,
        help: "old".into(),
        advanced: false,
        props: Default::default(),
    };
    first.project.cache.insert("key".into(), entry.clone());
    let bytes = f.run(&mut first, None).ok_or("Missing root record.")?;
    assert_eq!(first.replay_hits(), 0);

    let same = || {
        let mut i = f.interp();
        i.track_variable_reads(["A".into()]);
        i.project.cache.insert("key".into(), entry.clone());
        i
    };
    let mut hit = same();
    f.run(&mut hit, Some(&bytes));
    assert_eq!(hit.replay_hits(), 1);
    assert_eq!(
        hit.project.dir(DirId::ROOT).vars,
        first.project.dir(DirId::ROOT).vars
    );
    assert_eq!(hit.unread_variables().count(), 0);

    let mut changed = same();
    changed
        .project
        .cache
        .get_mut(b"key")
        .ok_or("Missing cache entry.")?
        .help = "new".into();
    f.run(&mut changed, Some(&bytes));
    assert_eq!(changed.replay_hits(), 0);

    let mut changed = same();
    changed.cx().set_var(b"extra", "scope input".into());
    f.run(&mut changed, Some(&bytes));
    assert_eq!(changed.replay_hits(), 0);

    let mut changed = same();
    f.script(file, b"set(A new)\nset(B two)\n")?;
    f.run(&mut changed, Some(&bytes));
    assert_eq!(changed.replay_hits(), 0);
    assert_eq!(changed.cx().var(b"A"), Some(BString::from("new")));

    f.script(file, b"set(A one)\nset(B two)\n")?;
    f.host
        .seed_file(&f.root.join("rcmake"), b"new binary identity")?;
    let mut changed = same();
    f.run(&mut changed, Some(&bytes));
    assert_eq!(changed.replay_hits(), 0);
    Ok(())
}

#[test]
fn sibling_edit_reuses_only_the_unchanged_leaf() -> TestResult {
    let f = Fixture::new()?;
    f.script(
        "CMakeLists.txt",
        b"add_subdirectory(a)\nadd_subdirectory(b)\n",
    )?;
    f.script("a/CMakeLists.txt", b"set(V first)\n")?;
    f.script("b/CMakeLists.txt", b"set(V second)\n")?;
    let bytes = f
        .run(&mut f.interp(), None)
        .ok_or("Missing leaf records.")?;
    f.script("a/CMakeLists.txt", b"set(V third)\n")?;
    let mut next = f.interp();
    let refreshed = f
        .run(&mut next, Some(&bytes))
        .ok_or("Missing updated frame.")?;
    assert_eq!(next.replay_hits(), 1);
    let vars: Vec<_> = next
        .project
        .dirs()
        .filter_map(|(_, d)| d.vars.get(b"V".as_slice()))
        .collect();
    assert_eq!(vars, [&BString::from("third"), &BString::from("second")]);
    let mut warm = f.interp();
    f.run(&mut warm, Some(&refreshed));
    assert_eq!(warm.replay_hits(), 2);
    Ok(())
}

#[test]
fn invalid_late_effect_does_not_change_earlier_variables() -> TestResult {
    let f = Fixture::new()?;
    f.script("CMakeLists.txt", b"set(A expected)\nset(B second)\n")?;
    let bytes = f.run(&mut f.interp(), None).ok_or("Missing root record.")?;
    let mut bad = Manifest::decode(&bytes)?;
    let RegionPayload::Complete { effects, .. } = &mut bad.region.payload else {
        return Err("Missing effects.".into());
    };
    *effects.first_mut().ok_or("Expected at least one effect.")? = JournalEffect::SetVar {
        scope: VariableScope::Current,
        name: b"WRONG".to_vec(),
        value: Some(b"leak".to_vec()),
    };
    effects.push(JournalEffect::ConfigureLog(b"invalid".to_vec()));
    let mut next = f.interp();
    f.run(&mut next, Some(&bad.encode()?));
    assert_eq!(next.replay_hits(), 0);
    assert!(next.cx().var(b"WRONG").is_none());
    assert_eq!(next.cx().var(b"B"), Some("second".into()));
    Ok(())
}

#[test]
fn opaque_commands_and_observed_sessions_stay_live() -> TestResult {
    let f = Fixture::new()?;
    f.script("CMakeLists.txt", b"set(A cold)\n")?;
    let bytes = f.run(&mut f.interp(), None).ok_or("Missing root record.")?;
    let mut replaced = f.interp();
    replaced
        .registry
        .add("set", set, CmdFlags { scriptable: true });
    assert!(f.run(&mut replaced, Some(&bytes)).is_none());
    assert_eq!(replaced.replay_hits(), 0);
    let mut profiled = f.interp();
    profiled.set_command_profiler(Some(Arc::new(|_| {})));
    assert!(f.run(&mut profiled, Some(&bytes)).is_none());
    assert_eq!(profiled.replay_hits(), 0);
    f.script("CMakeLists.txt", b"message(STATUS live)\n")?;
    let mut ordinary = f.interp();
    assert!(f.run(&mut ordinary, Some(&bytes)).is_none());
    assert_eq!(ordinary.replay_hits(), 0);
    assert!(
        f.host
            .stream_bytes(rcmake_host::Stream::Stdout)
            .ends_with(b"-- live\n")
    );
    Ok(())
}

#[test]
fn bootstrap_rechecks_compiler_contents_and_recursion_environment() -> TestResult {
    let f = Fixture::new()?;
    let info = f.root.join("info");
    let compiler = f.root.join("cc");
    let module = info.join("CMakeCCompiler.cmake");
    f.host.seed_file(&compiler, b"version A")?;
    f.host.seed_file(&module, b"set(INITIALIZED yes)\n")?;
    f.script(
        "CMakeLists.txt",
        format!("include([[{}]])\n", module.display()).as_bytes(),
    )?;
    let configured = || {
        let mut interp = f.interp();
        interp
            .cx()
            .set_var(b"CMAKE_PLATFORM_INFO_DIR", path::from_native_cmake(&info));
        interp
            .cx()
            .set_var(b"CMAKE_C_COMPILER", path::from_native_cmake(&compiler));
        interp
    };

    let record = f
        .run(&mut configured(), None)
        .ok_or("Missing compiler bootstrap record.")?;
    let frame = Manifest::decode(&record)?;
    assert!(frame.model.nested.iter().any(|(r, _)| matches!(&r.key.kind,
        RegionKind::Bootstrap {language} if language == b"C")));
    let mut warm = configured();
    f.run(&mut warm, Some(&record));
    assert!(warm.replayed_listfiles().contains(&module));
    assert_eq!(warm.cx().var(b"INITIALIZED"), Some("yes".into()));

    f.host
        .env_set(b"CMAKE_MAXIMUM_RECURSION_DEPTH", Some(b"64"));
    let mut changed = configured();
    f.run(&mut changed, Some(&record));
    assert_eq!(changed.replay_hits(), 0);
    f.host.env_set(b"CMAKE_MAXIMUM_RECURSION_DEPTH", None);
    f.host.seed_file(&compiler, b"version B")?;
    let mut changed = configured();
    f.run(&mut changed, Some(&record));
    assert_eq!(changed.replay_hits(), 0);
    Ok(())
}
