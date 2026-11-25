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
    CommandSpec, Follow, Host, ProcessResult, RunOutput, RunRequest, path::to_native,
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
        return plan.submodules(cx);
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
            repository, tag, ..
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
            mode,
            offline,
        };
        plan.find_pin(cx)?;
        Ok(Some(plan))
    }

    fn find_pin(&mut self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        let lock = lock_mutex(&self.runtime.lock);
        let session = match lock.as_ref() {
            Some(Ok(lock)) => lock,
            _ if matches!(self.mode, FetchLockMode::Update | FetchLockMode::Frozen) => {
                return Err(stop(cx, "Could not read the fetch lockfile."));
            }
            _ => return Ok(()),
        };
        let locked = if self.mode == FetchLockMode::Frozen {
            match session.git_pin(
                &self.population.dependency,
                &self.repository,
                self.tag.as_deref(),
            ) {
                Ok(pin) => pin,
                Err(_) => {
                    return required(
                        cx,
                        &self.runtime,
                        &self.population,
                        BlockReason::LockMismatch,
                    );
                }
            }
        } else if self.offline && self.pin.is_none() {
            match session.snapshot().get(&self.population.dependency) {
                Some(LockEntry::Git(entry))
                    if GitEntry::same_repository(&entry.repository, &self.repository)
                        && entry.tag == self.tag =>
                {
                    Some(entry.commit.as_str())
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(locked) = locked {
            if self.pin.as_ref().is_some_and(|pin| pin != locked) {
                return required(
                    cx,
                    &self.runtime,
                    &self.population,
                    BlockReason::LockMismatch,
                );
            }
            self.pin = Some(locked.into());
        }
        if self.offline && self.pin.is_none() {
            required(cx, &self.runtime, &self.population, BlockReason::Unpinned)?;
        }
        Ok(())
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

    fn submodules(&self, cx: &mut Cx<'_>) -> Result<ProcessAction, CmdError> {
        let modules = self.population.source.join(".gitmodules");
        if cx
            .host()
            .metadata(&modules, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
        {
            return Ok(if self.enforce() {
                success(Vec::new())
            } else {
                ProcessAction::default()
            });
        }
        if self.enforce() {
            required(
                cx,
                &self.runtime,
                &self.population,
                BlockReason::GitSubmodules,
            )?;
        } else {
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
        let source = url.strip_prefix("file://").unwrap_or(url);
        if source.contains("://") {
            return None;
        }
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
    let host = cx.host();
    host.current_exe()
        .is_ok_and(|exe| same_file(host, &exe, &to_native(&command.program)))
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
}
