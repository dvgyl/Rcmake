//! Tests for setup spans, setup checkpoints, and the setup record codec.

use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, PureHost, path};
use rcmake_model::{CacheEntry, CacheType, DefinedProperty, SourceGroupDef, Target, TargetKind};

use super::super::activate::Regions;
use super::*;
use crate::journal::ConfigureSnapshot;
use crate::{
    CmdError, CmdFlags, CmdResult, Flow, IncludeOptions, InterpConfig, Registry, RunMode,
    SubdirOptions,
};

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

/// `cache(NAME VALUE)`: write an internal cache entry.
fn cache(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, value] = args else {
        return Err(CmdError::status("Expected a name and a value."));
    };
    cx.project().cache.insert(
        name.value.clone(),
        CacheEntry {
            value: value.value.clone(),
            ty: CacheType::Internal,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
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
    let target = Target::new(
        name.value.clone(),
        TargetKind::StaticLibrary,
        rcmake_model::TargetOrigin::Normal,
        cx.current_dir_id(),
        cx.backtrace(),
        *cx.policies_mut(),
    );
    cx.project()
        .add_target(target)
        .map_err(|error| CmdError::status(error.to_string()))?;
    Ok(Flow::Normal)
}

/// A small `project()`: run `${TOOLCHAIN}` in a setup span. Without the saved
/// system file, it also runs a determination that leaves a reserved variable, like
/// the stock compiler modules in a cold configure.
fn project(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    cx.begin_replay_setup();
    let system = PathBuf::from(if cfg!(windows) {
        "C:/work/build/system"
    } else {
        "/work/build/system"
    });
    if cx
        .host()
        .metadata(&system, rcmake_host::Follow::Yes)
        .is_err()
    {
        cx.note_replay_determination();
        cx.set_var(b"CMAKE_C_COMPILER_ID_TEST_FLAGS", "-c".into());
        let _ = cx
            .host()
            .write(&system, b"", rcmake_host::WriteMode::Truncate);
    }
    let result = match cx.var(b"TOOLCHAIN") {
        Some(file) => cx.include_file(&path::to_native(&file), IncludeOptions::default()),
        None => Ok(Flow::Normal),
    };
    cx.end_replay_project(result.is_ok());
    result
}

fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: true };
    registry.add("set", set, flags);
    registry.add("cache", cache, flags);
    registry.add("add_subdirectory", subdirectory, flags);
    registry.add("add_library", add_library, flags);
    registry.add("project", project, flags);
    registry.mark_replay_commands();
}

struct Fixture {
    host: Arc<PureHost>,
    root: PathBuf,
}

impl Fixture {
    fn new(lists: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
        host.seed_file(&root.join("rcmake"), b"executable identity")?;
        let fixture = Self { host, root };
        fixture.file("src/CMakeLists.txt", lists)?;
        for leaf in ["early", "late"] {
            fixture.file(
                &format!("src/{leaf}/CMakeLists.txt"),
                &format!("add_library({leaf} STATIC {leaf}.c)\n"),
            )?;
        }
        fixture.file("tc.cmake", "# Toolchain.\nset(CMAKE_TC_FLAG 1)\n")?;
        Ok(fixture)
    }

    fn file(&self, name: &str, text: &str) -> TestResult {
        self.host
            .seed_file(&self.root.join(name), text.as_bytes())?;
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

    fn run_with(&self, interp: &mut Interp, manifest: Option<&[u8]>) -> Option<Vec<u8>> {
        let (outcome, record) = interp.configure_incremental(
            &self.root.join("src"),
            &self.root.join("build"),
            &[],
            manifest,
        );
        assert_eq!(outcome.exit_code, 0);
        record
    }

    /// Configure once. Return the new manifest, the hit count, and the setup miss.
    fn run(&self, manifest: Option<&[u8]>) -> (Option<Vec<u8>>, u64, Option<String>) {
        let mut interp = self.interp();
        let record = self.run_with(&mut interp, manifest);
        let miss = interp.replay_setup_miss().map(str::to_owned);
        (record, interp.replay_hits(), miss)
    }
}

const LISTS: &str = "set(TOOLCHAIN /work/tc.cmake)\nadd_subdirectory(early)\nproject(P)\n\
                     add_subdirectory(late)\n";

fn warm(fixture: &Fixture) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (cold, hits, _) = fixture.run(None);
    assert_eq!(hits, 0);
    let (warm, hits, miss) = fixture.run(cold.as_deref());
    assert_eq!((hits, miss), (1, None));
    Ok(warm.ok_or("missing warm manifest")?)
}

#[test]
fn hits_wait_for_the_first_project_and_stay_off_inside_setup() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let warm = warm(&fixture)?;
    let mut interp = fixture.interp();
    fixture.run_with(&mut interp, Some(&warm));
    // The leaf before `project()` and the toolchain in the setup span run live.
    let replayed: Vec<_> = interp.replayed_listfiles().iter().cloned().collect();
    assert_eq!(replayed, [fixture.root.join("src/late/CMakeLists.txt")]);
    Ok(())
}

#[test]
fn cold_only_reserved_variables_do_not_block_the_first_warm_configure() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let (cold, _, _) = fixture.run(None);
    let cold = cold.ok_or("missing cold manifest")?;
    let record = |bytes: &[u8]| -> Result<SetupRecord, Box<dyn std::error::Error>> {
        Ok(ConfigureSnapshot::<Regions>::decode(bytes)?
            .model
            .setup
            .ok_or("missing setup record")?)
    };
    assert_eq!(record(&cold)?.determined, [true]);
    let (warm, hits, miss) = fixture.run(Some(&cold));
    assert_eq!((hits, miss), (1, None));
    let warm = warm.ok_or("missing warm manifest")?;
    assert_eq!(record(&warm)?.determined, [false]);
    // Two warm checkpoints compare their variables too.
    fixture.file("tc.cmake", "set(CMAKE_TC_FLAG 2)\n")?;
    let (_, hits, miss) = fixture.run(Some(&warm));
    assert_eq!((hits, miss.as_deref()), (0, Some("checkpoint 0 variables")));
    // Other domains compare between a cold and a warm checkpoint as well.
    fixture
        .host
        .remove_file(&fixture.root.join("build/system"))?;
    fixture.file("tc.cmake", "cache(CMAKE_TC_FLAG 1)\n")?;
    let (_, hits, miss) = fixture.run(Some(&warm));
    assert_eq!(hits, 0);
    assert!(miss.is_some_and(|miss| miss.starts_with("cache")));
    Ok(())
}

#[test]
fn a_toolchain_value_change_misses_and_a_comment_edit_hits() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let warm = warm(&fixture)?;
    fixture.file("tc.cmake", "# Edited comment.\nset(CMAKE_TC_FLAG 1)\n")?;
    let (comment, hits, miss) = fixture.run(Some(&warm));
    assert_eq!((hits, miss), (1, None));
    fixture.file("tc.cmake", "# Edited comment.\nset(CMAKE_TC_FLAG 2)\n")?;
    let (changed, hits, miss) = fixture.run(comment.as_deref());
    assert_eq!(hits, 0);
    assert_eq!(miss.as_deref(), Some("checkpoint 0 variables"));
    // The configure that missed recorded again, so the next one replays.
    let (_, hits, miss) = fixture.run(changed.as_deref());
    assert_eq!((hits, miss), (1, None));
    Ok(())
}

#[test]
fn setup_inputs_and_setup_environment_reads_gate_the_configure() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    fixture.file("tc.cmake", "set(CMAKE_TC_FLAG $ENV{TC_ENV})\n")?;
    fixture.host.env_set(b"TC_ENV", Some(b"one"));
    let run = |manifest: Option<&[u8]>, preset: &str| {
        let mut interp = fixture.interp();
        let mut inputs = ReplaySetupInputs::new();
        inputs.field("preset_environment", preset);
        inputs.file("preload", fixture.root.join("init.cmake"));
        interp.add_session_data(inputs);
        let record = fixture.run_with(&mut interp, manifest);
        let miss = interp.replay_setup_miss().map(str::to_owned);
        (record, interp.replay_hits(), miss)
    };
    let (cold, _, _) = run(None, "A=1");
    let (warm, hits, miss) = run(cold.as_deref(), "A=1");
    assert_eq!((hits, miss), (1, None));
    let (_, hits, miss) = run(warm.as_deref(), "A=2");
    assert_eq!(
        (hits, miss.as_deref()),
        (0, Some("input preset_environment"))
    );
    fixture.file("init.cmake", "set(X 1 CACHE STRING \"\")\n")?;
    let (_, hits, miss) = run(warm.as_deref(), "A=1");
    assert_eq!((hits, miss.as_deref()), (0, Some("input file:preload")));
    fixture.host.remove_file(&fixture.root.join("init.cmake"))?;
    fixture.host.env_set(b"TC_ENV", Some(b"two"));
    let (_, hits, miss) = run(warm.as_deref(), "A=1");
    assert_eq!((hits, miss.as_deref()), (0, Some("environment TC_ENV")));
    Ok(())
}

#[test]
fn the_cache_ledger_accepts_a_warm_cache_and_rejects_changed_entries() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    fixture.file("tc.cmake", "cache(TC_CACHE 1)\n")?;
    let entry = |value: &str| CacheEntry {
        value: value.into(),
        ty: CacheType::Internal,
        help: BString::default(),
        advanced: false,
        props: Default::default(),
    };
    // `loaded` is the saved cache. `edits` are command-line edits after the load.
    let run = |manifest: Option<&[u8]>, loaded: &[(&str, &str)], edits: &[(&str, &str)]| {
        let mut interp = fixture.interp();
        let mut saved = rcmake_model::Cache::default();
        for (name, value) in loaded {
            saved.insert((*name).into(), entry(value));
        }
        let mut inputs = ReplaySetupInputs::new();
        inputs.loaded_cache(&saved);
        interp.add_session_data(inputs);
        interp.project.cache = saved;
        for (name, value) in edits {
            interp.project.cache.insert((*name).into(), entry(value));
        }
        let record = fixture.run_with(&mut interp, manifest);
        let miss = interp.replay_setup_miss().map(str::to_owned);
        (record, interp.replay_hits(), miss)
    };
    let (cold, _, _) = run(None, &[], &[("USER", "1")]);
    // The warm configure loads the saved cache and the same edit again.
    let (warm, hits, miss) = run(
        cold.as_deref(),
        &[("TC_CACHE", "1"), ("USER", "1")],
        &[("USER", "1")],
    );
    assert_eq!((hits, miss), (1, None));
    // A regenerate without the edit loads the same cache.
    let saved = [("TC_CACHE", "1"), ("USER", "1")];
    let (_, hits, miss) = run(warm.as_deref(), &saved, &[]);
    assert_eq!((hits, miss), (1, None));
    let (_, hits, miss) = run(warm.as_deref(), &saved, &[("USER", "2")]);
    assert_eq!((hits, miss.as_deref()), (0, Some("cache USER")));
    let (_, hits, miss) = run(warm.as_deref(), &saved, &[("NEW", "1")]);
    assert_eq!((hits, miss.as_deref()), (0, Some("cache NEW")));
    let (_, hits, miss) = run(warm.as_deref(), &[("TC_CACHE", "0"), ("USER", "1")], &[]);
    // Setup writes the entry again, but the saved value differs from the ledger.
    assert_eq!(hits, 1, "{miss:?}");
    fixture.file("tc.cmake", "cache(TC_CACHE 2)\n")?;
    let (_, hits, miss) = run(warm.as_deref(), &saved, &[]);
    assert_eq!((hits, miss.as_deref()), (0, Some("cache TC_CACHE")));
    Ok(())
}

#[test]
fn a_manifest_without_a_setup_record_records_once() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let warm = warm(&fixture)?;
    let mut manifest = ConfigureSnapshot::<Regions>::decode(&warm)?;
    manifest.model.setup = None;
    let old = manifest.encode()?;
    let (again, hits, miss) = fixture.run(Some(&old));
    assert_eq!((hits, miss.as_deref()), (0, Some("no setup record")));
    let (_, hits, miss) = fixture.run(again.as_deref());
    assert_eq!((hits, miss), (1, None));
    Ok(())
}

#[test]
fn corrupt_manifests_and_setup_records_never_panic() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let warm = warm(&fixture)?;
    let manifest = ConfigureSnapshot::<Regions>::decode(&warm)?;
    let setup = manifest.model.setup.clone().ok_or("missing setup record")?;
    assert!(!setup.checkpoints.is_empty() && !setup.cache.is_empty());
    assert_eq!(super::super::codec::round_trip(&setup)?, setup);
    let mut text = Vec::new();
    super::super::frame::write_json(&setup, &mut text, 1 << 20).ok_or("encode failed")?;
    let text = String::from_utf8(text)?;
    let options = rcmake_json::ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    };
    let newer = text.replace("\"version\":1", "\"version\":2");
    assert!(SetupRecord::from_json(&rcmake_json::parse(newer.as_bytes(), &options)?).is_err());

    // Flip, truncate, and replace bytes. Decoding returns a manifest or an error.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..3000 {
        let mut bytes = warm.clone();
        let len = bytes.len() as u64;
        let at = usize::try_from(next() % len)?;
        match round % 3 {
            0 => bytes.truncate(at),
            1 => {
                if let Some(byte) = bytes.get_mut(at) {
                    *byte ^= 1 << (next() % 8);
                }
            }
            _ => {
                if let Some(byte) = bytes.get_mut(at) {
                    *byte = b"{}[]\",:0n"[usize::try_from(next() % 9)?];
                }
            }
        }
        if let Ok(decoded) = ConfigureSnapshot::<Regions>::decode(&bytes) {
            let _ = decoded.model.setup;
        }
        if round % 300 == 0 {
            let _ = fixture.run(Some(&bytes));
        }
    }
    Ok(())
}

#[test]
fn setup_decode_rejects_records_beyond_the_writer_limits() -> TestResult {
    let decode = |record: &SetupRecord| -> Result<SetupRecord, String> {
        let mut text = Vec::new();
        super::super::frame::write_json(record, &mut text, 1 << 24).ok_or("encode failed")?;
        let options = rcmake_json::ReadOptions {
            allow_comments: false,
            dup_keys_last_wins: false,
        };
        let value = rcmake_json::parse(&text, &options).map_err(|err| err.to_string())?;
        SetupRecord::from_json(&value).map_err(|err| err.to_string())
    };
    let digest = Some(JournalDigest::hash(b"value"));
    let ledger = |len: usize| -> Digests {
        (0..len)
            .map(|i| (format!("NAME_{i}").into_bytes(), digest))
            .collect()
    };
    let full = SetupRecord {
        cache: ledger(MAX_LEDGER),
        env: ledger(MAX_LEDGER),
        checkpoints: vec![Vec::new(), Vec::new()],
        determined: vec![true, false],
        ..SetupRecord::default()
    };
    assert_eq!(decode(&full)?, full);
    let long_name = vec![(vec![b'N'; MAX_ENV_NAME + 1], digest)];
    for (field, record) in [
        (
            "cache",
            SetupRecord {
                cache: ledger(MAX_LEDGER + 1),
                ..full.clone()
            },
        ),
        (
            "env",
            SetupRecord {
                env: ledger(MAX_LEDGER + 1),
                ..full.clone()
            },
        ),
        (
            "env",
            SetupRecord {
                env: long_name,
                ..full.clone()
            },
        ),
        (
            "checkpoints",
            SetupRecord {
                determined: vec![true],
                ..full.clone()
            },
        ),
        (
            "checkpoints",
            SetupRecord {
                determined: vec![true; 3],
                ..full.clone()
            },
        ),
    ] {
        let error = decode(&record).err().ok_or(field)?;
        assert!(error.contains(field), "{field}: {error}");
    }
    Ok(())
}

/// Each class of interpreter state that a listfile can observe after setup changes
/// exactly one domain digest. A new class of state must add a row here.
#[test]
fn each_state_class_changes_exactly_its_domain() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(LISTS)?;
    let include = |interp: &mut Interp, file: &str| {
        let file = fixture.root.join(file);
        let _ = interp.cx().include_file(&file, IncludeOptions::default());
    };
    type Mutation<'a> = (&'a str, Box<dyn Fn(&mut Interp) + 'a>);
    let rows: Vec<Mutation<'_>> = vec![
        (
            "variables",
            Box::new(|i| i.cx().set_var(b"CMAKE_X", "1".into())),
        ),
        (
            "variables",
            Box::new(|i| i.cx().set_var(b"_CMAKE_X", "1".into())),
        ),
        (
            "variables",
            Box::new(|i| {
                i.cx()
                    .set_var(b"CMAKE_Fortran_MODULE_DIRECTORY", "m".into())
            }),
        ),
        (
            "policies",
            Box::new(|i| {
                let _ = i.cx().policies_mut().set(
                    rcmake_diag::PolicyId::new(77),
                    rcmake_diag::PolicyStatus::New,
                );
            }),
        ),
        (
            "diagnostics",
            Box::new(|i| i.log_level = Some(crate::LogLevel::Debug)),
        ),
        (
            "global",
            Box::new(|i| i.project.global.props.set("G".into(), "1".into())),
        ),
        (
            "global",
            Box::new(|i| {
                i.project.global.defined_props.push(DefinedProperty {
                    scope: "TARGET".into(),
                    name: "P".into(),
                    inherited: false,
                    brief_docs: BString::default(),
                    full_docs: BString::default(),
                    initialize_from_variable: Some("V".into()),
                });
            }),
        ),
        (
            "directories",
            Box::new(|i| {
                i.project
                    .dir_mut(DirId::ROOT)
                    .props
                    .set("D".into(), "1".into());
            }),
        ),
        (
            "directories",
            Box::new(|i| i.project.dir_mut(DirId::ROOT).define_flags = "-DX".into()),
        ),
        (
            "directories",
            Box::new(|i| {
                i.project
                    .dir_mut(DirId::ROOT)
                    .source_groups
                    .push(SourceGroupDef::default());
            }),
        ),
        (
            "directories",
            Box::new(|i| {
                i.project
                    .dir_mut(DirId::ROOT)
                    .source_props
                    .entry("/work/a.c".into())
                    .or_default();
            }),
        ),
        (
            "directories",
            Box::new(|i| {
                i.project
                    .dir_mut(DirId::ROOT)
                    .generated_sources
                    .insert("/work/g.c".into());
            }),
        ),
        (
            "directories",
            Box::new(|i| {
                i.project
                    .dir_mut(DirId::ROOT)
                    .usage
                    .fields
                    .insert(rcmake_model::UsageField::CompileOptions, Vec::new());
            }),
        ),
        (
            "targets",
            Box::new(|i| {
                let _ = i.project.add_target(Target::new(
                    "t".into(),
                    TargetKind::InterfaceLibrary,
                    rcmake_model::TargetOrigin::Normal,
                    DirId::ROOT,
                    Default::default(),
                    Default::default(),
                ));
            }),
        ),
        (
            "hooks",
            Box::new(|i| {
                i.hooks.watches.insert("W".into(), Vec::new());
            }),
        ),
        ("hooks", Box::new(|i| i.hooks.next_defer_id += 1)),
    ];
    let base = fixture.interp().setup_domains();
    // A name that CMake does not reserve is not part of the checkpoint. The
    // listfile verifier records each read of it.
    let mut local = fixture.interp();
    for name in [
        b"_uname_result".as_slice(),
        b"val",
        b"_INCLUDED_TOOLCHAIN_FILE",
        b"MY_FLAG",
    ] {
        local.cx().set_var(name, "0".into());
    }
    assert_eq!(local.setup_domains(), base);
    let names: Vec<_> = base.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "variables",
            "policies",
            "diagnostics",
            "global",
            "directories",
            "targets",
            "commands",
            "hooks"
        ]
    );
    let changed = |interp: &Interp, base: &[Domain]| -> Vec<String> {
        interp
            .setup_domains()
            .into_iter()
            .zip(base)
            .filter(|(now, before)| now != *before)
            .map(|(now, _)| now.0)
            .collect()
    };
    for (domain, mutate) in &rows {
        let mut interp = fixture.interp();
        mutate(&mut interp);
        assert_eq!(changed(&interp, &base), [*domain], "row for {domain}");
    }
    // An include sets variables that name the file, so each variant uses one file
    // name. A new definition, or one with another formal parameter, body, or
    // captured policy, changes only the command domain.
    let defined = |text: &str, policy: bool| -> Result<Vec<Domain>, Box<dyn std::error::Error>> {
        fixture.file("defs.cmake", text)?;
        let mut interp = fixture.interp();
        if policy {
            let _ = interp.cx().policies_mut().set(
                rcmake_diag::PolicyId::new(77),
                rcmake_diag::PolicyStatus::New,
            );
        }
        include(&mut interp, "defs.cmake");
        Ok(interp.setup_domains())
    };
    let only_commands = |a: &[Domain], b: &[Domain]| -> Vec<String> {
        a.iter()
            .zip(b)
            .filter(|(x, y)| x != y)
            .map(|(x, _)| x.0.clone())
            .collect()
    };
    let empty = defined("", false)?;
    let function = defined("function(f a)\nendfunction()\n", false)?;
    assert_eq!(only_commands(&function, &empty), ["commands"]);
    for variant in [
        "function(f b)\nendfunction()\n",
        "function(f a)\nset(x 1)\nendfunction()\n",
    ] {
        assert_eq!(
            only_commands(&defined(variant, false)?, &function),
            ["commands"],
            "{variant}"
        );
    }
    // A macro also appends its name to the `MACROS` directory property, which only
    // mirrors the command domain.
    let macro_domains = defined("macro(f a)\nendmacro()\n", false)?;
    assert_eq!(only_commands(&macro_domains, &function), ["commands"]);
    let policy_empty = defined("", true)?;
    let policy_function = defined("function(f a)\nendfunction()\n", true)?;
    assert_eq!(only_commands(&policy_function, &policy_empty), ["commands"]);
    assert_ne!(
        policy_function.iter().find(|(name, _)| name == "commands"),
        function.iter().find(|(name, _)| name == "commands")
    );
    // A stock module definition differs between a cold and a warm configure, and a
    // replayed listfile never calls a scripted command.
    fixture.file("share/Modules/defs.cmake", "")?;
    let mut stock_empty = fixture.interp();
    include(&mut stock_empty, "share/Modules/defs.cmake");
    fixture.file(
        "share/Modules/defs.cmake",
        "function(stock a)\nendfunction()\n",
    )?;
    let mut stock = fixture.interp();
    include(&mut stock, "share/Modules/defs.cmake");
    assert!(changed(&stock, &stock_empty.setup_domains()).is_empty());
    Ok(())
}

/// A stock module definition is not part of the command domain. A leaf that queries
/// a command must therefore run live after a same-path setup edit defines it.
#[test]
fn a_command_query_in_a_leaf_sees_a_new_stock_definition() -> TestResult {
    if cfg!(windows) {
        return Ok(());
    }
    let fixture = Fixture::new(
        "set(TOOLCHAIN /work/share/Modules/PC.cmake)\nproject(P)\nadd_subdirectory(late)\n",
    )?;
    fixture.file("share/Modules/PC.cmake", "")?;
    let mut stale = Vec::new();
    // Each leaf creates `pc` only while the command is not defined. A recorded leaf
    // must create every target that it names.
    for condition in [
        "COMMAND ProcessorCount)\nelse(",
        "NOT COMMAND ProcessorCount",
        "0)\nelseif(NOT COMMAND ProcessorCount",
    ] {
        fixture.file(
            "src/late/CMakeLists.txt",
            &format!("if({condition})\nadd_library(pc STATIC pc.c)\nendif()\n"),
        )?;
        fixture.file("share/Modules/PC.cmake", "")?;
        let (cold, _, _) = fixture.run(None);
        let (warm, _, _) = fixture.run(cold.as_deref());
        fixture.file(
            "share/Modules/PC.cmake",
            "function(ProcessorCount var)\nendfunction()\n",
        )?;
        let mut interp = fixture.interp();
        fixture.run_with(&mut interp, warm.as_deref());
        let replayed = interp.project.targets().any(|(_, t)| t.name == "pc");
        let mut live = fixture.interp();
        fixture.run_with(&mut live, None);
        let expected = live.project.targets().any(|(_, t)| t.name == "pc");
        if replayed != expected || !interp.replayed_listfiles().is_empty() {
            stale.push(condition);
        }
    }
    assert!(stale.is_empty(), "{stale:?}");
    // A leaf with another condition still replays in this fixture.
    fixture.file(
        "src/late/CMakeLists.txt",
        "add_library(late STATIC late.c)\nif(1)\nendif()\n",
    )?;
    let (cold, _, _) = fixture.run(None);
    let (warm, _, _) = fixture.run(cold.as_deref());
    let (_, hits, miss) = fixture.run(warm.as_deref());
    assert_eq!((hits, miss), (1, None));
    Ok(())
}
