use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, PureHost};
use rcmake_model::{CacheEntry, CacheType, DirId};

use super::*;
use crate::{CmdError, CmdFlags, Cx, InterpConfig, Registry, SubdirOptions};

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

fn add_library(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, _, ..] = args else {
        return Err(CmdError::status("Missing target name."));
    };
    let mode = cx.var_or_empty(b"MODE");
    let mut target = rcmake_model::Target::new(
        name.value.clone(),
        rcmake_model::TargetKind::StaticLibrary,
        rcmake_model::TargetOrigin::Normal,
        cx.current_dir_id(),
        cx.backtrace(),
        *cx.policies_mut(),
    );
    target.props.set("MODE".into(), mode);
    cx.project()
        .add_target(target)
        .map_err(|error| CmdError::status(error.to_string()))?;
    Ok(Flow::Normal)
}

fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("set", set, flags);
    registry.add("add_subdirectory", subdirectory, flags);
    registry.add("add_library", add_library, flags);
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
            InterpConfig::new(
                self.host.clone(),
                RunMode::Configure,
                self.root.join("share"),
                self.root.join("rcmake"),
                &self.root,
            ),
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
fn a_confirmed_input_is_restored_only_for_the_same_executable() -> TestResult {
    fn check(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
        let digest = JournalDigest::hash(b"verified query input");
        let session = cx
            .session_data::<Arc<ReplayReceipts>>()
            .ok_or_else(|| CmdError::status("Missing query confirmations."))?;
        let reused = session.contains(digest);
        session.record(digest);
        cx.set_var(b"CONFIRMED", if reused { "yes" } else { "no" }.into());
        Ok(Flow::Normal)
    }

    let f = Fixture::new()?;
    f.script("CMakeLists.txt", b"check()\n")?;
    let configured = || {
        let mut interp = f.interp();
        Arc::make_mut(&mut interp.registry).add("check", check, CmdFlags { scriptable: true });
        interp.add_session_data(Arc::new(ReplayReceipts::default()));
        interp
    };
    let mut cold = configured();
    let snapshot = f
        .run(&mut cold, None)
        .ok_or("Missing query confirmations.")?;
    assert_eq!(cold.cx().var(b"CONFIRMED"), Some("no".into()));
    let mut warm = configured();
    f.run(&mut warm, Some(&snapshot));
    assert_eq!(warm.cx().var(b"CONFIRMED"), Some("yes".into()));

    f.host
        .seed_file(&f.root.join("rcmake"), b"new executable")?;
    let mut changed = configured();
    f.run(&mut changed, Some(&snapshot));
    assert_eq!(changed.cx().var(b"CONFIRMED"), Some("no".into()));
    Ok(())
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
fn first_target_leaf_edit_checks_only_the_variables_it_read() -> TestResult {
    let f = Fixture::new()?;
    f.script(
        "CMakeLists.txt",
        b"add_subdirectory(a)\nadd_subdirectory(b)\n",
    )?;
    f.script("a/CMakeLists.txt", b"add_library(a STATIC a.c)\n")?;
    f.script("b/CMakeLists.txt", b"add_library(b STATIC b.c)\n")?;
    let mut cold = f.interp();
    cold.vars.push();
    cold.vars.pop();
    cold.cx()
        .set_var(b"COMPILER_BOOTSTRAP_TEMP", "transient".into());
    cold.track_variable_reads(["ALREADY_USED".into(), "UNUSED".into()]);
    let _ = cold.lookup_var(b"ALREADY_USED");
    let _ = cold.lookup_var(b"UNUSED");
    let bytes = f.run(&mut cold, None).ok_or("Missing leaf records.")?;
    f.script("b/CMakeLists.txt", b"set(EDITED 1)\n")?;

    let mut first_edit = f.interp();
    first_edit.track_variable_reads(["MODE".into(), "UNUSED".into()]);
    first_edit.project.cache.insert(
        "CMAKE_CACHEFILE_DIR".into(),
        CacheEntry {
            value: "build".into(),
            ty: CacheType::Internal,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    f.run(&mut first_edit, Some(&bytes));
    assert_eq!(first_edit.replay_hits(), 1);
    assert_eq!(
        first_edit.unread_variables().collect::<Vec<_>>(),
        [b"UNUSED"]
    );

    let mut changed_input = f.interp();
    changed_input.cx().set_var(b"MODE", "new".into());
    changed_input.track_variable_reads(["MODE".into(), "UNUSED".into()]);
    f.run(&mut changed_input, Some(&bytes));
    assert_eq!(changed_input.replay_hits(), 0);
    let model = &changed_input.project;
    let target = model
        .find_target(DirId::ROOT, b"a")
        .and_then(|id| model.target(id))
        .ok_or("Missing target a.")?;
    assert_eq!(target.props.get(b"MODE"), Some(&BString::from("new")));
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
        name: b"A".to_vec(),
        value: Some(b"leak".to_vec()),
    };
    *effects.last_mut().ok_or("Expected a late effect.")? = JournalEffect::SetVar {
        scope: VariableScope::Current,
        name: b"B".to_vec(),
        value: Some(vec![b'x'; 16]),
    };
    let mut next = f.interp();
    next.config.limits.max_value_bytes = 8;
    f.run(&mut next, Some(&bad.encode()?));
    assert_eq!(next.replay_hits(), 0);
    assert_eq!(next.cx().var(b"A"), Some("expected".into()));
    assert_eq!(next.cx().var(b"B"), Some("second".into()));
    Ok(())
}

#[test]
fn opaque_commands_and_observed_sessions_stay_live() -> TestResult {
    let f = Fixture::new()?;
    f.script("CMakeLists.txt", b"set(A cold)\n")?;
    let bytes = f.run(&mut f.interp(), None).ok_or("Missing root record.")?;
    let mut replaced = f.interp();
    Arc::make_mut(&mut replaced.registry).add("set", set, CmdFlags { scriptable: true });
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
    let system = info.join("CMakeSystem.cmake");
    f.host.seed_file(&compiler, b"version A")?;
    f.host
        .seed_file(&system, b"set(SYSTEM_DECLARATION loaded)\n")?;
    f.host.seed_file(
        &module,
        b"if(INPUT)\nset(INITIALIZED \"${INPUT}\")\nendif()\nset(INPUT replaced)\n",
    )?;
    f.script(
        "CMakeLists.txt",
        format!(
            "include([[{}]])\ninclude([[{}]])\n",
            system.display(),
            module.display()
        )
        .as_bytes(),
    )?;
    let configured = || {
        let mut interp = f.interp();
        interp
            .cx()
            .set_var(b"CMAKE_PLATFORM_INFO_DIR", path::from_native_cmake(&info));
        interp
            .cx()
            .set_var(b"CMAKE_C_COMPILER", path::from_native_cmake(&compiler));
        interp.cx().set_var(b"INPUT", "yes".into());
        interp
    };

    let record = f
        .run(&mut configured(), None)
        .ok_or("Missing compiler bootstrap record.")?;
    let frame = Manifest::decode(&record)?;
    assert!(frame.model.nested.iter().any(|(r, _)| matches!(&r.key.kind,
        RegionKind::Bootstrap {language} if language == b"C")));
    let mut warm = configured();
    warm.cx().set_var(b"UNRELATED", "warm".into());
    warm.track_executed_listfiles();
    f.run(&mut warm, Some(&record));
    assert!(warm.replayed_listfiles().contains(&system));
    assert!(warm.replayed_listfiles().contains(&module));
    assert!(!warm.executed_listfiles().any(|path| path == module));
    assert!(
        warm.executed_listfiles()
            .any(|path| path.ends_with("CMakeLists.txt"))
    );
    assert_eq!(warm.cx().var(b"INITIALIZED"), Some("yes".into()));

    let mut changed = configured();
    changed.cx().set_var(b"INPUT", "changed".into());
    f.run(&mut changed, Some(&record));
    assert!(!changed.replayed_listfiles().contains(&module));
    assert_eq!(changed.cx().var(b"INITIALIZED"), Some("changed".into()));

    f.host
        .env_set(b"CMAKE_MAXIMUM_RECURSION_DEPTH", Some(b"64"));
    let mut changed = configured();
    f.run(&mut changed, Some(&record));
    assert_eq!(changed.replay_hits(), 0);
    f.host.env_set(b"CMAKE_MAXIMUM_RECURSION_DEPTH", None);
    f.host.seed_file(&compiler, b"version B")?;
    let mut changed = configured();
    f.run(&mut changed, Some(&record));
    assert!(!changed.replayed_listfiles().contains(&module));
    Ok(())
}

#[test]
fn replay_checks_expanded_values_environment_and_file_conditions() -> TestResult {
    let f = Fixture::new()?;
    let marker = f.root.join("src/marker");
    f.script(
        "CMakeLists.txt",
        br#"
set(prefix name)
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/marker")
  set(prefix present)
endif()
set(result "${prefix}:$CACHE{SUFFIX}:$ENV{COLOR}")
foreach(part IN ITEMS first second)
  set(result "${result}:${part}")
endforeach()
"#,
    )?;
    f.host.env_set(b"COLOR", Some(b"blue"));
    let configured = || {
        let mut interp = f.interp();
        interp.project.cache.insert(
            "SUFFIX".into(),
            CacheEntry {
                value: "value".into(),
                ty: CacheType::String,
                help: "".into(),
                advanced: false,
                props: Default::default(),
            },
        );
        interp.track_executed_listfiles();
        interp
    };
    let mut cold = configured();
    let record = f.run(&mut cold, None).ok_or("Missing replay record.")?;
    let mut warm = configured();
    f.run(&mut warm, Some(&record));
    assert_eq!(warm.replay_hits(), 1);
    assert_eq!(
        warm.cx().var(b"result"),
        Some("name:value:blue:first:second".into())
    );

    f.host.env_set(b"COLOR", Some(b"green"));
    let mut changed = configured();
    let record = f
        .run(&mut changed, Some(&record))
        .ok_or("Missing updated replay record.")?;
    assert_eq!(changed.replay_hits(), 0);
    assert_eq!(
        changed.cx().var(b"result"),
        Some("name:value:green:first:second".into())
    );

    f.host.seed_file(&marker, b"")?;
    let mut changed = configured();
    f.run(&mut changed, Some(&record));
    assert_eq!(changed.replay_hits(), 0);
    assert_eq!(
        changed.cx().var(b"result"),
        Some("present:value:green:first:second".into())
    );
    assert!(
        changed
            .executed_listfiles()
            .any(|file| file.ends_with("CMakeLists.txt"))
    );
    Ok(())
}

#[test]
fn cold_configure_defers_executable_hashing_until_manifest_save() -> TestResult {
    let f = Fixture::new()?;
    // An unrecorded/unclassified command prevents any replay record from being emitted.
    f.script("CMakeLists.txt", b"set(A 1)\n")?;
    // Remove the executable before a cold run (`cached = None`): if cold start tried to
    // hash the executable eagerly, `ReplaySession::start` would return `None` early.
    // Here we verify a cold run without replayable output never reads the missing exe,
    // and when the exe IS present, lazy hashing stamps it cleanly on manifest save.
    f.host.remove_file(&f.root.join("rcmake"))?;
    let mut cold = f.interp();
    let record = f.run(&mut cold, None);
    assert!(record.is_none());

    // Restore the exe and verify cold save + warm hit works cleanly under configure.v6.
    f.host.seed_file(&f.root.join("rcmake"), b"v6 binary")?;
    let mut cold = f.interp();
    let record = f
        .run(&mut cold, None)
        .ok_or("Missing cold replay record.")?;
    let decoded = ConfigureSnapshot::<Regions>::decode(&record)?;
    assert_eq!(decoded.schema, "configure.v6");
    let mut warm = f.interp();
    f.run(&mut warm, Some(&record));
    assert_eq!(warm.replay_hits(), 1);
    Ok(())
}

#[test]
fn typed_setup_domains_distinguish_none_vs_empty_and_stacked_policies() -> TestResult {
    let f = Fixture::new()?;
    let base = f.interp();
    let base_domains = base.setup_domains();

    // Multi-frame policy stack with different weak flags must change only `policies`.
    let mut pushed = f.interp();
    pushed.policies.push_copy();
    assert_ne!(pushed.setup_domains(), base_domains);
    let mut weak = f.interp();
    weak.policies.push_weak(rcmake_diag::PolicyMap::default());
    assert_ne!(weak.setup_domains(), pushed.setup_domains());

    // `None` vs `Some("")` on `DefinedProperty::initialize_from_variable` must differ.
    let mut prop_none = f.interp();
    prop_none
        .project
        .global
        .defined_props
        .push(rcmake_model::DefinedProperty {
            scope: "TARGET".into(),
            name: "P".into(),
            inherited: false,
            brief_docs: BString::default(),
            full_docs: BString::default(),
            initialize_from_variable: None,
        });
    let mut prop_empty = f.interp();
    prop_empty
        .project
        .global
        .defined_props
        .push(rcmake_model::DefinedProperty {
            scope: "TARGET".into(),
            name: "P".into(),
            inherited: false,
            brief_docs: BString::default(),
            full_docs: BString::default(),
            initialize_from_variable: Some(BString::default()),
        });
    assert_ne!(prop_none.setup_domains(), prop_empty.setup_domains());

    // Multi-frame diagnostic stacks with different weak flags or frame states must differ.
    let mut diag_strong = f.interp();
    diag_strong.diags.push_copy();
    let mut diag_weak = f.interp();
    diag_weak.diags.push_weak(rcmake_diag::DiagState::default());
    assert_ne!(diag_strong.setup_domains(), base_domains);
    assert_ne!(diag_weak.setup_domains(), diag_strong.setup_domains());
    Ok(())
}

#[test]
fn startup_exe_pinning_and_strict_memo_admission() -> TestResult {
    let f = Fixture::new()?;
    let exe = f.root.join("rcmake");

    // Pin startup identity, then replace the binary on disk mid-configure -> must refuse to hash/publish.
    let pinned = capture_startup_exe(f.host.as_ref(), &exe).ok_or("missing startup exe id")?;
    assert!(executable_digest_pinned(f.host.as_ref(), &exe, Some(&pinned)).is_some());
    f.host.seed_file(&exe, b"replaced binary with new size")?;
    assert!(executable_digest_pinned(f.host.as_ref(), &exe, Some(&pinned)).is_none());

    // Verify `ExeMemoKey::from_immutable` mirrors `TraceMemo`'s strict admission rules.
    f.host.set_effective_uid(Some(1000))?;
    f.host.sleep(std::time::Duration::from_secs(10));
    let now = f.host.now();
    let old_ctime = rcmake_host::FileTime {
        secs: now.secs - 5,
        nanos: 123_456,
    };
    let immutable_meta = rcmake_host::Metadata {
        kind: FileKind::File,
        len: 1024,
        mode: 0o755,
        mtime: old_ctime,
        ctime: Some(old_ctime),
        atime: old_ctime,
        dev: 1,
        ino: 42,
        nlink: 1,
        owner: Some(0),
    };
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &immutable_meta).is_some());

    // Missing ctime -> declined.
    let mut no_ctime = immutable_meta.clone();
    no_ctime.ctime = None;
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &no_ctime).is_none());

    // Coarse second-resolution ctime (nanos == 0) -> declined.
    let mut coarse_ctime = immutable_meta.clone();
    coarse_ctime.ctime = Some(rcmake_host::FileTime {
        secs: old_ctime.secs,
        nanos: 0,
    });
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &coarse_ctime).is_none());

    // Recent ctime (within 1s of now) -> declined.
    let mut recent_ctime = immutable_meta.clone();
    recent_ctime.ctime = Some(rcmake_host::FileTime {
        secs: now.secs,
        nanos: 500,
    });
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &recent_ctime).is_none());

    // Zero inode -> declined.
    let mut zero_ino = immutable_meta.clone();
    zero_ino.ino = 0;
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &zero_ino).is_none());

    // Owned by current user -> declined (writable/chmod-able by current user).
    let mut owned_by_self = immutable_meta.clone();
    owned_by_self.owner = Some(1000);
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &owned_by_self).is_none());

    // Group-writable -> declined.
    let mut group_writable = immutable_meta.clone();
    group_writable.mode = 0o775;
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &group_writable).is_none());

    // Root caller (uid == 0) -> declined.
    f.host.set_effective_uid(Some(0))?;
    assert!(ExeMemoKey::from_immutable(f.host.as_ref(), &exe, &immutable_meta).is_none());
    Ok(())
}
