use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, list};
use rcmake_eval::{
    CmdError, CmdFlags, CmdResult, Cx, Flow, IncludeOptions, Interp, InterpConfig, LogLevel,
    Registry, RunMode,
};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{CommandSpec, Follow, Host};

use crate::CtestError;

const MAX_RECURSION: u32 = 128;
const TEST_FILE: &str = "CTestTestfile.cmake";
const SCRIPTABLE: CmdFlags = CmdFlags { scriptable: true };

pub(crate) struct Test {
    pub(crate) name: BString,
    pub(crate) command: CommandSpec,
    pub(crate) directory: PathBuf,
    pub(crate) properties: BTreeMap<BString, BString>,
}

impl Test {
    pub(crate) fn property(&self, key: &[u8]) -> &[u8] {
        self.properties
            .get(key)
            .map_or(b"", |value| value.as_slice())
    }

    pub(crate) fn list(&self, key: &[u8]) -> Vec<BString> {
        list::split(self.property(key), list::Empty::Drop)
    }
}

#[derive(Default)]
struct Discovery {
    tests: Vec<Test>,
    names: BTreeMap<BString, Vec<usize>>,
}

thread_local! {
    // Interpreter command callbacks are function pointers. Keep one collector
    // per discovery call on the current thread.
    static COLLECTOR: RefCell<Discovery> = RefCell::new(Discovery::default());
}

struct DiscoveryScope(Discovery);

impl DiscoveryScope {
    fn enter() -> Self {
        Self(COLLECTOR.take())
    }
}

impl Drop for DiscoveryScope {
    fn drop(&mut self) {
        COLLECTOR.replace(std::mem::take(&mut self.0));
    }
}

pub(crate) fn load(
    host: Arc<dyn Host>,
    dir: &Path,
    config: Option<&BString>,
) -> Result<Vec<Test>, CtestError> {
    let path = dir.join(TEST_FILE);
    match host.metadata(&path, Follow::Yes) {
        Ok(_) => {}
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(CtestError::Access { path, source }),
    }
    let executable = host.current_exe().unwrap_or_else(|_| dir.join("cmake"));
    let cmake_root = host
        .env_get(b"RCMAKE_VENDORED_DATA_ROOT")
        .map(|root| dir.join(to_native(&root)))
        .or_else(|| Some(executable.parent()?.parent()?.join("share/cmake-4.5")))
        .unwrap_or_default();
    let cmake_command =
        executable.with_file_name(if cfg!(windows) { "cmake.exe" } else { "cmake" });
    let mut interp = Interp::new(
        InterpConfig {
            host,
            mode: RunMode::Script,
            cmake_root,
            cmake_command,
            cwd: dir.to_path_buf(),
            limits: Limits {
                max_recursion: MAX_RECURSION,
                ..Limits::default()
            },
            diag: rcmake_diag::DiagOptions::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    interp.set_log_level(Some(LogLevel::Notice));
    interp.cx().set_var(
        b"CTEST_CONFIGURATION_TYPE",
        config.cloned().unwrap_or_default(),
    );
    let _scope = DiscoveryScope::enter();
    if interp.run_script(&path).exit_code != 0 {
        return Err(CtestError::Load(path));
    }
    Ok(COLLECTOR.take().tests)
}

fn register(registry: &mut Registry) {
    rcmake_commands::register_all(registry);
    registry.add("add_test", add_test, SCRIPTABLE);
    registry.add("set_tests_properties", set_properties, SCRIPTABLE);
    registry.add("subdirs", subdirs, SCRIPTABLE);
    registry.add("add_subdirectory", add_subdirectory, SCRIPTABLE);
    registry.add("discover_tests", crate::discover::run, SCRIPTABLE);
}

fn add_test(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, program, args @ ..] = args else {
        return Err(CmdError::Status("requires a test name and command".into()));
    };
    let test = Test {
        name: name.value.clone(),
        command: CommandSpec {
            program: program.value.clone(),
            args: args.iter().map(|arg| arg.value.clone()).collect(),
        },
        directory: cx.current_binary_dir(),
        properties: BTreeMap::new(),
    };
    register_test(test);
    Ok(Flow::Normal)
}

pub(crate) fn register_test(test: Test) {
    COLLECTOR.with_borrow_mut(|state| {
        state
            .names
            .entry(test.name.clone())
            .or_default()
            .push(state.tests.len());
        state.tests.push(test);
    });
}

fn set_properties(_: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(keyword) = args.iter().position(|arg| arg.value == "PROPERTIES") else {
        return Err(CmdError::Status("requires the PROPERTIES keyword".into()));
    };
    let (names, tail) = args.split_at(keyword);
    let (pairs, remainder) = tail.get(1..).unwrap_or_default().as_chunks::<2>();
    if names.is_empty() || !remainder.is_empty() {
        return Err(CmdError::Status(
            "requires test names and property-value pairs".into(),
        ));
    }
    COLLECTOR.with_borrow_mut(|state| {
        for name in names {
            if let Some(ids) = state.names.get(&name.value) {
                for &id in ids {
                    if let Some(test) = state.tests.get_mut(id) {
                        for [key, value] in pairs {
                            test.properties
                                .insert(key.value.clone(), value.value.clone());
                        }
                    }
                }
            }
        }
    });
    Ok(Flow::Normal)
}

fn add_subdirectory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    match args {
        [directory, ..] => subdirs(cx, std::slice::from_ref(directory)),
        [] => Err(CmdError::Status("requires a directory".into())),
    }
}

fn subdirs(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if cx
        .backtrace()
        .frames()
        .nth(MAX_RECURSION as usize)
        .is_some()
    {
        return Err(CmdError::Status(
            "exceeded the test directory recursion limit".into(),
        ));
    }
    let parent = cx.current_binary_dir();
    let parent_bytes = from_native(&parent);
    for arg in args {
        let dir = parent.join(to_native(&arg.value));
        let file = dir.join(TEST_FILE);
        match cx.host().metadata(&file, Follow::Yes) {
            Ok(_) => {}
            Err(error) if error.kind == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CmdError::Status(
                    format!("cannot access {}: {error}", file.display()).into(),
                ));
            }
        }
        let dir_bytes = from_native(&dir);
        cx.project().set_root_dirs(dir_bytes.clone(), dir_bytes);
        let result = cx.include_file(&file, IncludeOptions::default());
        cx.project()
            .set_root_dirs(parent_bytes.clone(), parent_bytes.clone());
        result?;
    }
    Ok(Flow::Normal)
}
