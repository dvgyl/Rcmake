// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx, FetchLockMode};
use rcmake_fetch::{GitClone, GitEntry, GitMirror, LockEntry};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, path::to_native,
};

use super::runtime::{
    Population, Runtime, Script, blocked, check_unmatched, lock_mutex, module_is, required,
    runtime, stop,
};
use super::{BlockReason, DeclaredSource, GitTransfer, portable_source, relative_repo};

#[derive(Default)]
pub(crate) struct ProcessAction {
    pub(crate) result: Option<RunOutput>,
    record: Option<GitRecord>,
}

struct GitRecord {
    runtime: Arc<Runtime>,
    dependency: String,
    repository: String,
    tag: Option<String>,
    program: BString,
    source: PathBuf,
}

struct GitPlan {
    runtime: Arc<Runtime>,
    population: Population,
    repository: String,
    tag: Option<String>,
    pin: Option<String>,
    default_submodules: bool,
    mode: FetchLockMode,
    offline: bool,
}

pub(crate) fn before_process(
    cx: &mut Cx<'_>,
    request: &RunRequest,
) -> Result<ProcessAction, CmdError> {
    if !cx.host().supports_process_execution() {
        return Ok(ProcessAction::default());
    }
    let active = runtime(cx).and_then(|runtime| runtime.population(cx).map(|pop| (runtime, pop)));
    let Some((runtime, pop)) = active else {
        let executable = cx.var_or_empty(b"GIT_EXECUTABLE");
        if request.pipeline.iter().any(|command| {
            let program = to_native(&command.program);
            let is_git = (!executable.is_empty() && command.program == executable)
                || program
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("git"));
            is_git && may_transfer(&command.args)
        }) {
            check_unmatched(cx)?;
        }
        return Ok(ProcessAction::default());
    };
    if pop.subbuild {
        required(cx, &runtime, &pop, BlockReason::SubBuild)?;
        return Ok(ProcessAction::default());
    }
    let [command] = request.pipeline.as_slice() else {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(ProcessAction::default());
    };
    let script = runtime.script(cx, &pop);
    if local_cmake(cx, command, script)
        || local_git_version(cx, command)
        || script == Some(Script::Step) && local_directory_copy(cx, &runtime, &pop, command)?
    {
        return Ok(ProcessAction::default());
    }
    let executable = cx.var_or_empty(b"GIT_EXECUTABLE");
    if command.program != executable || executable.is_empty() {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(ProcessAction::default());
    }
    if !matches!(script, Some(Script::GitClone | Script::GitUpdate)) {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(ProcessAction::default());
    }
    let Some(plan) = GitPlan::new(cx, Arc::clone(&runtime), pop.clone())? else {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(ProcessAction::default());
    };
    let operation = git_operation(&command.args);
    if operation == b"submodule" && request.cwd == pop.source {
        return plan.submodules(cx, request, command, script);
    }
    if script == Some(Script::GitUpdate) && request.cwd == pop.source {
        return plan.update(cx, request, command, operation);
    }
    let transfer = match runtime.with_fetch(&pop, |fetch| {
        fetch.match_git(pop.site(), &command.program, &executable, &command.args)
    }) {
        Ok(transfer) => transfer,
        Err(error) => {
            blocked(cx, Some(error))?;
            return Ok(ProcessAction::default());
        }
    };
    match transfer {
        Some(GitTransfer::Clone(clone)) if request.cwd.join(clone.directory) == pop.source => {
            plan.clone_source(cx, request, command, &clone)
        }
        Some(GitTransfer::Checkout) if request.cwd == pop.source => {
            plan.checkout(cx, request, command)
        }
        _ => {
            required(cx, &runtime, &pop, BlockReason::Unsupported)?;
            if matches!(operation, b"clone" | b"fetch" | b"submodule") {
                crate::hermetic::check_fetch_network(cx, &plan.repository, false, &pop.dependency)?;
            }
            Ok(ProcessAction::default())
        }
    }
}

pub(crate) fn after_process(
    cx: &mut Cx<'_>,
    action: ProcessAction,
    run: &RunOutput,
) -> Result<(), CmdError> {
    let Some(record) = action.record else {
        return Ok(());
    };
    if run.results.as_slice() != [ProcessResult::Exited(0)] {
        return Ok(());
    }
    let root = cache_root(cx, &record.runtime);
    let _cache = crate::hermetic::allow_fetch_cache(cx, &root);
    let Ok(mirror) = GitMirror::new(cx.host(), &root) else {
        return Err(stop(cx, "Could not open the Git fetch cache."));
    };
    let mirror = mirror
        .with_program(record.program)
        .with_timeout(std::time::Duration::from_secs(30));
    let Ok(commit) = mirror.head(&record.source) else {
        return Err(stop(
            cx,
            "Could not read the Git revision to update the fetch lockfile.",
        ));
    };
    if record.runtime.config.root.is_absolute() {
        let _ = mirror.seed_checkout(&record.repository, &record.source, &commit);
    }
    let mut lock = lock_mutex(&record.runtime.lock);
    if let Some(Ok(lock)) = lock.as_mut() {
        lock.record_git_commit(
            &record.dependency,
            &record.repository,
            record.tag.as_deref(),
            &commit,
        )
        .map_err(|_| {
            stop(
                cx,
                "Could not record the Git revision in the fetch lockfile.",
            )
        })?;
    }
    Ok(())
}

pub(super) fn frozen_pin(
    cx: &mut Cx<'_>,
    runtime: Arc<Runtime>,
    pop: Population,
) -> Result<Option<String>, CmdError> {
    Ok(GitPlan::new(cx, runtime, pop)?.and_then(|plan| plan.pin))
}

impl GitPlan {
    fn new(
        cx: &mut Cx<'_>,
        runtime: Arc<Runtime>,
        population: Population,
    ) -> Result<Option<Self>, CmdError> {
        let declaration = runtime.with_fetch(&population, |fetch| {
            fetch
                .get(&population.dependency)
                .filter(|d| d.supported())
                .cloned()
                .map(|d| (d, fetch.mode, fetch.offline))
        });
        let Some((declaration, mode, offline)) = declaration else {
            return Ok(None);
        };
        let pin = declaration.git_commit().map(str::to_ascii_lowercase);
        let DeclaredSource::Git {
            repository,
            tag,
            submodules,
            ..
        } = declaration.source
        else {
            return Ok(None);
        };
        if !portable_source(&repository) || relative_repo(&repository) {
            return Ok(None);
        }
        let mut plan = Self {
            runtime,
            population,
            repository,
            tag,
            pin,
            default_submodules: submodules.is_none(),
            mode,
            offline,
        };
        plan.find_pin(cx)?;
        Ok(Some(plan))
    }

    fn find_pin(&mut self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        let selection = self.locked_revision();
        let locked = match selection {
            Ok(pin) => pin,
            Err(Some(error)) => return Err(stop(cx, error)),
            Err(None) => {
                return required(
                    cx,
                    &self.runtime,
                    &self.population,
                    BlockReason::LockMismatch,
                );
            }
        };
        if let Some(locked) = locked {
            if self.pin.as_ref().is_some_and(|pin| *pin != locked) {
                return required(
                    cx,
                    &self.runtime,
                    &self.population,
                    BlockReason::LockMismatch,
                );
            }
            self.pin = Some(locked);
        }
        if self.offline && self.pin.is_none() {
            required(cx, &self.runtime, &self.population, BlockReason::Unpinned)?;
        }
        Ok(())
    }

    fn locked_revision(&self) -> Result<Option<String>, Option<String>> {
        let lock = lock_mutex(&self.runtime.lock);
        let session = match lock.as_ref() {
            Some(Ok(lock)) => lock,
            Some(Err(error))
                if matches!(self.mode, FetchLockMode::Update | FetchLockMode::Frozen) =>
            {
                return Err(Some(error.to_string()));
            }
            _ => return Ok(None),
        };
        if self.mode == FetchLockMode::Frozen {
            return session
                .git_pin(
                    &self.population.dependency,
                    &self.repository,
                    self.tag.as_deref(),
                )
                .map(|pin| pin.map(str::to_ascii_lowercase))
                .map_err(|_| None);
        }
        if self.offline
            && self.pin.is_none()
            && let Some(LockEntry::Git(entry)) = session.snapshot().get(&self.population.dependency)
            && GitEntry::same_repository(&entry.repository, &self.repository)
            && entry.tag == self.tag
        {
            return Ok(Some(entry.commit.to_ascii_lowercase()));
        }
        Ok(None)
    }

    fn enforce(&self) -> bool {
        self.offline || self.mode == FetchLockMode::Frozen
    }

    fn with_mirror<T>(
        &self,
        cx: &Cx<'_>,
        request: &RunRequest,
        program: &BString,
        operation: impl FnOnce(GitMirror<'_>) -> T,
    ) -> Option<T> {
        let root = cache_root(cx, &self.runtime);
        let _cache = crate::hermetic::allow_fetch_cache(cx, &root);
        let mut mirror = GitMirror::new(cx.host(), &root)
            .ok()?
            .with_program(program.clone());
        if self.offline {
            mirror = mirror.offline_only();
        }
        let timeout = match (request.timeout, cx.limits().wall_budget) {
            (Some(request), Some(wall)) => Some(request.min(wall)),
            (request, wall) => request.or(wall),
        };
        let mirror = mirror.with_timeout(timeout.unwrap_or(std::time::Duration::from_secs(30)));
        Some(operation(mirror))
    }

    fn clone_source(
        &self,
        cx: &mut Cx<'_>,
        request: &RunRequest,
        command: &CommandSpec,
        clone: &GitClone<'_>,
    ) -> Result<ProcessAction, CmdError> {
        let Some(pin) = &self.pin else {
            self.check_network(cx, false)?;
            return Ok(ProcessAction::default());
        };
        self.runtime.wait_for_git(&self.repository, pin);
        let use_mirror = self.runtime.config.root.is_absolute()
            || (self.mode == FetchLockMode::Frozen && !self.offline);
        let result = if use_mirror {
            let try_cache = self
                .with_mirror(cx, request, &command.program, |mirror| {
                    mirror
                        .offline_only()
                        .clone_template(clone, &request.cwd, Some(pin))
                        .is_ok()
                })
                .unwrap_or(false);
            if try_cache || self.offline {
                try_cache
            } else {
                self.check_network(cx, true)?;
                self.with_mirror(cx, request, &command.program, |mirror| {
                    mirror
                        .clone_template(clone, &request.cwd, Some(pin))
                        .is_ok()
                })
                .unwrap_or(false)
            }
        } else {
            if !self.offline {
                self.check_network(cx, true)?;
            }
            false
        };
        if result {
            Ok(success(Vec::new()))
        } else if self.enforce() {
            required(cx, &self.runtime, &self.population, BlockReason::CacheMiss)?;
            Ok(ProcessAction::default())
        } else {
            Ok(ProcessAction::default())
        }
    }

    fn checkout(
        &self,
        cx: &mut Cx<'_>,
        request: &RunRequest,
        command: &CommandSpec,
    ) -> Result<ProcessAction, CmdError> {
        if self.enforce() {
            let Some(pin) = &self.pin else {
                required(cx, &self.runtime, &self.population, BlockReason::Unpinned)?;
                return Ok(ProcessAction::default());
            };
            let success_at_pin = self
                .with_mirror(cx, request, &command.program, |mirror| {
                    mirror.checkout_commit(&request.cwd, pin).is_ok()
                })
                .unwrap_or(false);
            if !success_at_pin {
                return Err(stop(cx, "Could not check out the locked Git commit."));
            }
            let mut action = success(Vec::new());
            action.record = self.record(command);
            return Ok(action);
        }
        Ok(ProcessAction {
            result: None,
            record: self.record(command),
        })
    }

    fn update(
        &self,
        cx: &mut Cx<'_>,
        request: &RunRequest,
        command: &CommandSpec,
        operation: &[u8],
    ) -> Result<ProcessAction, CmdError> {
        if !self.enforce() {
            if operation == b"fetch" {
                self.check_network(cx, false)?;
            }
            return Ok(ProcessAction {
                result: None,
                record: matches!(operation, b"checkout" | b"rebase")
                    .then(|| self.record(command))
                    .flatten(),
            });
        }
        let Some(pin) = &self.pin else {
            required(cx, &self.runtime, &self.population, BlockReason::Unpinned)?;
            return Ok(ProcessAction::default());
        };
        // The bundled update script returns once its requested ref resolves to HEAD.
        match command.args.as_slice() {
            [gitdir, op, rev]
                if gitdir == "--git-dir=.git" && op == "rev-parse" && rev == "HEAD^0" =>
            {
                let verified = self
                    .with_mirror(cx, request, &command.program, |mirror| {
                        mirror.head(&request.cwd).is_ok_and(|head| head == *pin)
                    })
                    .unwrap_or(false);
                if !verified {
                    return Err(stop(
                        cx,
                        "The Git checkout does not contain the locked revision at HEAD.",
                    ));
                }
                Ok(success(format!("{pin}\n").into_bytes()))
            }
            [gitdir, op, tag]
                if gitdir == "--git-dir=.git"
                    && op == "show-ref"
                    && tag == self.requested_tag() =>
            {
                Ok(success(Vec::new()))
            }
            [gitdir, op, rev]
                if gitdir == "--git-dir=.git"
                    && op == "rev-parse"
                    && rev.as_slice() == format!("{}^0", self.requested_tag()).as_bytes() =>
            {
                Ok(success(format!("{pin}\n").into_bytes()))
            }
            _ => {
                required(cx, &self.runtime, &self.population, BlockReason::GitUpdate)?;
                Ok(ProcessAction::default())
            }
        }
    }

    fn check_network(&self, cx: &Cx<'_>, pinned: bool) -> Result<(), CmdError> {
        crate::hermetic::check_fetch_network(
            cx,
            &self.repository,
            pinned,
            &self.population.dependency,
        )
    }

    fn requested_tag(&self) -> &str {
        self.tag.as_deref().unwrap_or("master")
    }

    fn submodules(
        &self,
        cx: &mut Cx<'_>,
        request: &RunRequest,
        command: &CommandSpec,
        script: Option<Script>,
    ) -> Result<ProcessAction, CmdError> {
        let modules = self.population.source.join(".gitmodules");
        let no_modules_file = cx
            .host()
            .metadata(&modules, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound);
        if no_modules_file && self.enforce() {
            return Ok(success(Vec::new()));
        }
        if no_modules_file
            && script == Some(Script::GitClone)
            && self.default_submodules
            && command.args == ["submodule", "update", "--recursive", "--init"]
            && let Some(pin) = &self.pin
            && no_gitlinks(cx.host(), request, command, pin)
        {
            return Ok(success(Vec::new()));
        }
        if self.enforce() {
            required(
                cx,
                &self.runtime,
                &self.population,
                BlockReason::GitSubmodules,
            )?;
        } else if !no_modules_file {
            self.check_network(cx, false)?;
        }
        Ok(ProcessAction::default())
    }

    fn record(&self, command: &CommandSpec) -> Option<GitRecord> {
        (self.mode == FetchLockMode::Update).then(|| GitRecord {
            runtime: Arc::clone(&self.runtime),
            dependency: self.population.dependency.clone(),
            repository: self.repository.clone(),
            tag: self.tag.clone(),
            program: command.program.clone(),
            source: self.population.source.clone(),
        })
    }
}

fn no_gitlinks(host: &dyn Host, request: &RunRequest, command: &CommandSpec, pin: &str) -> bool {
    // These variables move the repository or the configuration, or change where
    // Git finds its helper commands. With `GIT_EXEC_PATH`, `git submodule` can
    // fail even when the index has no gitlinks, so run the real command.
    const REDIRECTS: &[&[u8]] = &[
        b"GIT_EXEC_PATH",
        b"GIT_DIR",
        b"GIT_COMMON_DIR",
        b"GIT_WORK_TREE",
        b"GIT_INDEX_FILE",
        b"GIT_CONFIG",
        b"GIT_CONFIG_COUNT",
        b"GIT_CONFIG_PARAMETERS",
    ];
    let git = request.cwd.join(".git");
    if request
        .env
        .iter()
        .any(|(key, _)| REDIRECTS.iter().any(|name| key.eq_ignore_ascii_case(name)))
        || !host
            .metadata(&git, Follow::No)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
        || !host
            .metadata(&git.join("index"), Follow::No)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        || !host
            .read(&git.join("HEAD"), ReadRange::All { limit: 64 })
            .is_ok_and(|head| head.trim_ascii_end() == pin.as_bytes())
    {
        return false;
    }
    let query = |args: &[&str]| {
        let inspect = RunRequest {
            pipeline: vec![CommandSpec {
                program: command.program.clone(),
                args: args.iter().copied().map(BString::from).collect(),
            }],
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            tee: false,
            ..request.clone()
        };
        host.run_bounded(&inspect, 1024 * 1024)
            .ok()
            .filter(|run| run.results == [ProcessResult::Exited(0)] && run.stderr.is_empty())
            .map(|run| run.stdout)
    };
    // `git submodule` needs a work tree. Configuration such as `core.bare` or
    // `core.worktree` can remove it while the index stays readable, so require
    // the top level of a work tree at the checkout directory.
    let top_level = query(&["rev-parse", "--is-inside-work-tree", "--show-cdup"])
        .is_some_and(|stdout| stdout == b"true\n\n");
    top_level
        && query(&["ls-files", "--stage", "-z"]).is_some_and(|stdout| {
            stdout.is_empty()
                || stdout.strip_suffix(b"\0").is_some_and(|entries| {
                    entries.split(|&byte| byte == 0).all(|entry| {
                        matches!(
                            entry.split(|&byte| byte == b' ').next(),
                            Some(b"100644" | b"100755" | b"120000")
                        )
                    })
                })
        })
}

fn git_operation(mut args: &[BString]) -> &[u8] {
    loop {
        match args {
            [flag, rest @ ..] if flag == "--git-dir=.git" => args = rest,
            [flag, _, rest @ ..] if flag == "-c" => args = rest,
            [op, ..] => return op,
            [] => return b"",
        }
    }
}

fn may_transfer(mut args: &[BString]) -> bool {
    loop {
        match args {
            [flag, _, rest @ ..]
                if matches!(
                    flag.as_slice(),
                    b"-c"
                        | b"-C"
                        | b"--config-env"
                        | b"--git-dir"
                        | b"--work-tree"
                        | b"--namespace"
                        | b"--super-prefix"
                ) =>
            {
                args = rest;
            }
            [flag, rest @ ..] if flag.starts_with(b"-") => args = rest,
            [operation, ..] => {
                return matches!(
                    operation.as_slice(),
                    b"clone" | b"fetch" | b"ls-remote" | b"submodule"
                );
            }
            [] => return false,
        }
    }
}

fn success(stdout: Vec<u8>) -> ProcessAction {
    ProcessAction {
        result: Some(RunOutput {
            stdout,
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        }),
        record: None,
    }
}

fn local_git_version(cx: &Cx<'_>, command: &CommandSpec) -> bool {
    let trace = cx.backtrace();
    trace
        .top()
        .is_some_and(|frame| module_is(cx, &frame.file, "Modules/FindGit.cmake"))
        && matches!(command.args.as_slice(), [version] if version == "--version")
        && cx.var_or_empty(b"GIT_EXECUTABLE") == command.program
}

fn local_cmake(cx: &Cx<'_>, command: &CommandSpec, script: Option<Script>) -> bool {
    if !matches!(
        script,
        Some(Script::Archive | Script::GitClone | Script::Extract)
    ) {
        return false;
    }
    let [flag, op, ..] = command.args.as_slice() else {
        return false;
    };
    if flag != "-E" || !matches!(op.as_slice(), b"rm" | b"copy" | b"sleep" | b"tar") {
        return false;
    }
    bundled_executable(cx, command)
}

fn local_directory_copy(
    cx: &Cx<'_>,
    runtime: &Runtime,
    pop: &Population,
    command: &CommandSpec,
) -> Result<bool, CmdError> {
    if !bundled_executable(cx, command) {
        return Ok(false);
    }
    let origin = runtime.with_fetch(pop, |fetch| {
        let info = fetch.get(&pop.dependency).filter(|d| d.supported())?;
        let DeclaredSource::Archive { urls, .. } = &info.source else {
            return None;
        };
        let [url] = urls.as_slice() else { return None };
        let source = super::archive::local_url_path(url)?;
        let source = cx.resolve_path(source.as_bytes());
        cx.host()
            .metadata(&source, Follow::Yes)
            .ok()
            .filter(|meta| meta.kind == rcmake_host::FileKind::Dir)?;
        Some(source)
    });
    let Some(origin) = origin else {
        return Ok(false);
    };
    match command.args.as_slice() {
        [helper, op, flags, target]
            if helper == "-E"
                && op == "rm"
                && flags == "-rf"
                && to_native(target) == pop.source =>
        {
            Ok(true)
        }
        [helper, op, source, target]
            if helper == "-E"
                && op == "copy_directory"
                && to_native(source) == origin
                && to_native(target) == pop.source =>
        {
            crate::hermetic::check_fetch_network(
                cx,
                &origin.to_string_lossy(),
                false,
                &pop.dependency,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn bundled_executable(cx: &Cx<'_>, command: &CommandSpec) -> bool {
    bundled_program(cx.host(), &to_native(&command.program))
}

/// Return whether `program` is the running executable or its staged `cmake` sibling.
///
/// A staged install (for example the Windows release layout) places `cmake.exe` next to the
/// `rcmake.exe` entry point. Both names start the same engine, so both may use the
/// in-process fast path.
fn bundled_program(host: &dyn Host, program: &Path) -> bool {
    let Ok(exe) = host.current_exe() else {
        return false;
    };
    if same_file(host, &exe, program) {
        return true;
    }
    let (Ok(exe), Ok(target)) = (host.canonicalize(&exe), host.canonicalize(program)) else {
        return false;
    };
    let is_file = host
        .metadata(&target, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::File);
    let sibling = exe.parent().is_some() && target.parent() == exe.parent();
    let named_cmake = target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ["cmake", "cmake.exe"].iter().any(|expected| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case(expected)
                } else {
                    name == *expected
                }
            })
        });
    is_file && sibling && named_cmake
}

fn cache_root(cx: &Cx<'_>, runtime: &Runtime) -> PathBuf {
    if runtime.config.root.is_absolute() {
        runtime.config.root.clone()
    } else {
        cx.current_binary_dir().join("CMakeFiles/rcmake")
    }
}

fn same_file(host: &dyn Host, left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let (Ok(left), Ok(right)) = (host.canonicalize(left), host.canonicalize(right)) else {
        return false;
    };
    left == right
}

#[cfg(test)]
mod tests {
    use rcmake_host::{HostResult, PureHost};

    use super::*;

    #[test]
    fn index_inspection_rejects_gitlinks_and_failed_queries() -> HostResult<()> {
        let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(root.into(), root.join("rcmake"));
        let pin = "0123456789012345678901234567890123456789";
        host.seed_file(&root.join(".git/HEAD"), format!("{pin}\n").as_bytes())?;
        host.seed_file(&root.join(".git/index"), b"index")?;
        let command = CommandSpec {
            program: "git".into(),
            args: vec!["submodule".into(), "update".into()],
        };
        let request = RunRequest {
            pipeline: vec![command.clone()],
            cwd: root.into(),
            env: Vec::new(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        // A bare repository (`core.bare=true`) and a checkout below another work
        // tree (`core.worktree`) give other `rev-parse` answers.
        for (worktree, mode, status, expected) in [
            ("true\n\n", "100644", 0, true),
            ("true\n\n", "160000", 0, false),
            ("true\n\n", "100644", 1, false),
            ("false\n", "100644", 0, false),
            ("true\n../\n", "100644", 0, false),
        ] {
            host.set_command_handler(move |_, request, _| {
                let args = request
                    .pipeline
                    .first()
                    .map(|command| command.args.as_slice())
                    .unwrap_or_default();
                let (stdout, status) = if args
                    == ["rev-parse", "--is-inside-work-tree", "--show-cdup"].map(BString::from)
                {
                    (worktree.to_string(), 0)
                } else {
                    assert_eq!(args, ["ls-files", "--stage", "-z"].map(BString::from));
                    (format!("{mode} {pin} 0\ttracked\nname\0"), status)
                };
                Ok(RunOutput {
                    stdout: stdout.into_bytes(),
                    results: vec![ProcessResult::Exited(status)],
                    ..RunOutput::default()
                })
            })?;
            assert_eq!(
                no_gitlinks(&host, &request, &command, pin),
                expected,
                "{worktree:?} {mode} {status}"
            );
        }
        Ok(())
    }

    #[test]
    fn helper_lookup_environment_runs_the_real_submodule_command() -> HostResult<()> {
        let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(root.into(), root.join("rcmake"));
        let pin = "0123456789012345678901234567890123456789";
        host.seed_file(&root.join(".git/HEAD"), format!("{pin}\n").as_bytes())?;
        host.seed_file(&root.join(".git/index"), b"index")?;
        let command = CommandSpec {
            program: "git".into(),
            args: ["submodule", "update", "--recursive", "--init"]
                .map(BString::from)
                .into(),
        };
        let queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&queries);
        host.set_command_handler(move |_, request, _| {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let worktree = request
                .pipeline
                .first()
                .is_some_and(|command| command.args.first().is_some_and(|arg| arg == "rev-parse"));
            let stdout = if worktree {
                "true\n\n".to_string()
            } else {
                format!("100644 {pin} 0\ttracked\0")
            };
            Ok(RunOutput {
                stdout: stdout.into_bytes(),
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let request = |env: Vec<(BString, BString)>| RunRequest {
            pipeline: vec![command.clone()],
            cwd: root.into(),
            env,
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        assert!(no_gitlinks(&host, &request(Vec::new()), &command, pin));
        assert_eq!(queries.load(std::sync::atomic::Ordering::SeqCst), 2);
        for key in ["GIT_EXEC_PATH", "git_exec_path"] {
            let env = vec![(BString::from(key), BString::from("/empty"))];
            assert!(!no_gitlinks(&host, &request(env), &command, pin));
        }
        assert_eq!(queries.load(std::sync::atomic::Ordering::SeqCst), 2);
        Ok(())
    }

    #[test]
    fn matches_the_running_executable_through_a_path_alias() -> HostResult<()> {
        let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let executable = root.join("real/rcmake");
        let other = root.join("real/other");
        let alias = root.join("alias");
        let host = PureHost::new(root.into(), alias.join("rcmake"));
        host.seed_file(&executable, b"rcmake")?;
        host.seed_file(&other, b"other")?;
        host.symlink(&root.join("real"), &alias)?;

        let invoked = host.current_exe()?;
        assert!(same_file(&host, &invoked, &executable));
        assert!(!same_file(&host, &invoked, &other));
        Ok(())
    }

    #[test]
    fn accepts_the_staged_cmake_sibling_of_the_running_executable() -> HostResult<()> {
        let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let bin = root.join("bin");
        let host = PureHost::new(root.into(), bin.join("rcmake.exe"));
        host.seed_file(&bin.join("rcmake.exe"), b"rcmake")?;
        host.seed_file(&bin.join("cmake.exe"), b"cmake")?;
        host.seed_file(&bin.join("cmake"), b"cmake")?;
        host.seed_file(&bin.join("ctest.exe"), b"ctest")?;
        host.seed_file(&root.join("other/cmake.exe"), b"cmake")?;

        assert!(bundled_program(&host, &bin.join("rcmake.exe")));
        assert!(bundled_program(&host, &bin.join("cmake.exe")));
        assert!(bundled_program(&host, &bin.join("cmake")));
        assert!(!bundled_program(&host, &bin.join("ctest.exe")));
        assert!(!bundled_program(&host, &root.join("other/cmake.exe")));
        assert!(!bundled_program(&host, &bin.join("missing/cmake.exe")));

        let host = PureHost::new(root.into(), bin.join("rcmake"));
        host.seed_file(&bin.join("rcmake"), b"rcmake")?;
        host.create_dir_all(&bin.join("cmake"))?;
        assert!(!bundled_program(&host, &bin.join("cmake")));
        Ok(())
    }
}
