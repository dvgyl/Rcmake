//! Command execution engine: frames, dispatch, scopes, files, and diagnostics.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
    fold_command_name,
};
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, DiagState, Diagnostic, Effect, Frame, FrameKind, Location,
    MessageKind, PolicyId, PolicyMap, PolicyStatus, source::SourceKind,
};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, ReadRange, Stream};
use rcmake_model::{CacheEntry, CacheType, DirId};
use rcmake_regex::Captures;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation, ParseError};

use crate::code::Code;
use crate::hooks::{WatchAccess, display_line};
use crate::util::{closest, collapse_full_path, filename_path};
use crate::{
    CmdError, CmdResult, Command, Cx, DirFrame, FLOW_CONTROL, Flow, HARD_RECURSION_CEILING,
    IncludeOptions, Interp, LogLevel, RunMode, RunOutcome, Snapshot, SubdirOptions, UserCommand,
    UserCommandKind,
};

/// `CMP0198`: `CMAKE_PARENT_LIST_FILE` is not defined in `CMakeLists.txt`.
pub(crate) const CMP0198: PolicyId = PolicyId::new(198);

/// How a command sequence reacts to failed commands and control-flow transfers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyMode {
    /// Listfile: continue after failures; stop at `return()`; ignore `break()`/`continue()`.
    File,
    /// Function body: stop at the first failure; stop at `return()`.
    Function,
    /// Macro body: stop at the first failure; forward every transfer.
    Macro,
    /// `if`/`foreach`/`while`/`block` body: continue after failures; forward transfers.
    Nested,
}

/// Saved list-file variables (`FileScopeBase`).
pub(crate) struct ListFileVars {
    old_parent: Option<BString>,
    old_current: BString,
}

/// Error text of commands that only close or continue a block.
fn unexpected_text(lower: &[u8]) -> Option<&'static str> {
    Some(match lower {
        b"else" => {
            "An ELSE command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
        }
        b"elseif" => "An ELSEIF command was found outside of a proper IF ENDIF structure.",
        b"endforeach" => {
            "An ENDFOREACH command was found outside of a proper FOREACH ENDFOREACH structure. Or its arguments did not match the opening FOREACH command."
        }
        b"endfunction" => {
            "An ENDFUNCTION command was found outside of a proper FUNCTION ENDFUNCTION structure. Or its arguments did not match the opening FUNCTION command."
        }
        b"endif" => {
            "An ENDIF command was found outside of a proper IF ENDIF structure. Or its arguments did not match the opening IF command."
        }
        b"endmacro" => {
            "An ENDMACRO command was found outside of a proper MACRO ENDMACRO structure. Or its arguments did not match the opening MACRO command."
        }
        b"endwhile" => {
            "An ENDWHILE command was found outside of a proper WHILE ENDWHILE structure. Or its arguments did not match the opening WHILE command."
        }
        b"endblock" => {
            "An ENDBLOCK command was found outside of a proper BLOCK ENDBLOCK structure."
        }
        _ => return None,
    })
}

/// Concatenate byte slices.
pub(crate) fn cat(parts: &[&[u8]]) -> BString {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    BString::from(out)
}

/// Map a warning kind to the matching error kind.
fn error_kind(kind: MessageKind) -> MessageKind {
    match kind {
        MessageKind::Warning | MessageKind::Log => MessageKind::Error,
        MessageKind::AuthorWarning => MessageKind::AuthorError,
        MessageKind::DeprecationWarning => MessageKind::DeprecationError,
        other => other,
    }
}

/// Map an error kind to the matching warning kind.
fn warning_kind(kind: MessageKind) -> MessageKind {
    match kind {
        MessageKind::Error | MessageKind::InternalError => MessageKind::Warning,
        MessageKind::AuthorError => MessageKind::AuthorWarning,
        MessageKind::DeprecationError => MessageKind::DeprecationWarning,
        other => other,
    }
}

/// Host system name as CMake reports it (`cmSystemTools::GetSystemName`).
fn host_system_name(sysname: &[u8]) -> BString {
    let lower = sysname.to_ascii_lowercase();
    if lower.starts_with(b"windows") {
        return BString::from("Windows");
    }
    for (prefix, name) in [
        (&b"cygwin"[..], "CYGWIN"),
        (b"msys", "MSYS"),
        (b"mingw", "MINGW"),
    ] {
        if lower.starts_with(prefix) {
            return BString::from(name);
        }
    }
    BString::from(sysname)
}

impl Interp {
    // ----------------------------------------------------------------------------------
    // Initialization and top-level entry points.
    // ----------------------------------------------------------------------------------

    /// Define the variables that CMake defines in every new directory/script session
    /// (`cmStateSnapshot::SetDefaultDefinitions` plus script-mode paths).
    pub(crate) fn init_default_vars(&mut self) {
        let sys = self.config.host.system();
        let name = host_system_name(&sys.sysname);
        let mut defs: Vec<(&[u8], BString)> = Vec::new();
        defs.push((b"CMAKE_HOST_SYSTEM_NAME", name.clone()));
        if name.as_slice() == b"Windows" {
            defs.push((b"WIN32", "1".into()));
            defs.push((b"CMAKE_HOST_WIN32", "1".into()));
            defs.push((b"CMAKE_HOST_EXECUTABLE_SUFFIX", ".exe".into()));
        } else {
            defs.push((b"UNIX", "1".into()));
            defs.push((b"CMAKE_HOST_UNIX", "1".into()));
            defs.push((b"CMAKE_HOST_EXECUTABLE_SUFFIX", BString::default()));
        }
        match name.as_slice() {
            b"Darwin" => {
                defs.push((b"APPLE", "1".into()));
                defs.push((b"CMAKE_HOST_APPLE", "1".into()));
            }
            b"Linux" => {
                defs.push((b"LINUX", "1".into()));
                defs.push((b"CMAKE_HOST_LINUX", "1".into()));
            }
            b"SunOS" => defs.push((b"CMAKE_HOST_SOLARIS", "1".into())),
            b"AIX" => {
                defs.push((b"AIX", "1".into()));
                defs.push((b"CMAKE_HOST_AIX", "1".into()));
            }
            b"OpenBSD" | b"FreeBSD" | b"NetBSD" => {
                defs.push((b"BSD", name.clone()));
                defs.push((b"CMAKE_HOST_BSD", name.clone()));
            }
            b"DragonFly" => {
                defs.push((b"BSD", "DragonFlyBSD".into()));
                defs.push((b"CMAKE_HOST_BSD", "DragonFlyBSD".into()));
            }
            _ => {}
        }
        defs.push((
            b"CMAKE_MAJOR_VERSION",
            CMAKE_VERSION_MAJOR.to_string().into(),
        ));
        defs.push((
            b"CMAKE_MINOR_VERSION",
            CMAKE_VERSION_MINOR.to_string().into(),
        ));
        defs.push((
            b"CMAKE_PATCH_VERSION",
            CMAKE_VERSION_PATCH.to_string().into(),
        ));
        defs.push((b"CMAKE_TWEAK_VERSION", "0".into()));
        defs.push((b"CMAKE_VERSION", CMAKE_VERSION_STR.into()));
        defs.push((b"CMAKE_FILES_DIRECTORY", "/CMakeFiles".into()));

        let to_b = rcmake_host::path::from_native;
        let command = to_b(&self.config.cmake_command);
        let bin_dir = filename_path(&command);
        defs.push((b"CMAKE_COMMAND", command));
        defs.push((
            b"CMAKE_CTEST_COMMAND",
            crate::exec::cat(&[&bin_dir, b"/ctest"]),
        ));
        defs.push((
            b"CMAKE_CPACK_COMMAND",
            crate::exec::cat(&[&bin_dir, b"/cpack"]),
        ));
        defs.push((b"CMAKE_ROOT", to_b(&self.config.cmake_root)));
        let cwd = to_b(&self.config.cwd);
        for var in [
            &b"CMAKE_SOURCE_DIR"[..],
            b"CMAKE_BINARY_DIR",
            b"CMAKE_CURRENT_SOURCE_DIR",
            b"CMAKE_CURRENT_BINARY_DIR",
        ] {
            defs.push((var, cwd.clone()));
        }
        for (k, v) in defs {
            self.vars.set(k, v);
        }
    }

    pub(crate) fn run_script_impl(&mut self, path: &Path) -> RunOutcome {
        self.init_policy_minimum();
        let cwd = rcmake_host::path::from_native(&self.config.cwd);
        let abs = collapse_full_path(&rcmake_host::path::from_native(path), &cwd);

        self.vars.set(b"CMAKE_SCRIPT_MODE_FILE", abs.clone());
        // The script-mode root snapshot has no listfile.
        self.listfiles.push(Arc::from(&b""[..]));
        let argc = self.config.script_args.len();
        self.vars.set(b"CMAKE_ARGC", argc.to_string().into());
        for (i, arg) in self.config.script_args.clone().into_iter().enumerate() {
            self.vars.set(format!("CMAKE_ARGV{i}").as_bytes(), arg);
        }

        if !self.is_regular_file(&abs) {
            self.error_no_location(cat(&[b"Not a file: ", &abs]));
        }
        if !self.read_list_file(&abs) {
            self.error_no_location(cat(&[b"Error processing file: ", &abs]));
        }
        RunOutcome {
            exit_code: self.exit_code(),
        }
    }

    /// `cmMakefile::ReadListFile`: run a file without a policy scope. Returns `false`
    /// when the file can not be read or parsed.
    pub(crate) fn read_list_file(&mut self, abs: &[u8]) -> bool {
        self.read_list_file_impl(abs, false)
    }

    /// Run a listfile. A directory listfile (`CMakeLists.txt` in configure mode) also
    /// applies `CMP0198` and runs the deferred calls that it queues
    /// (`cmMakefile::Configure` and `RunListFile`).
    fn read_list_file_impl(&mut self, abs: &[u8], directory: bool) -> bool {
        let saved_bt = self.push_file_frame(abs);
        let vars = self.push_list_file_vars(abs);
        if directory {
            self.begin_directory_list_file();
        }
        self.record_list_file(abs);
        self.push_snapshot();
        let saved_defer = if directory {
            self.dir_frame_mut().defer.replace(Vec::new())
        } else {
            None
        };
        let read = match self.parse_file(abs) {
            Some(code) => {
                let r = self.run_code(&code, 0, code.len(), BodyMode::File);
                if let Ok(Flow::Return {
                    propagate: Some(names),
                }) = &r
                {
                    self.raise_vars(names);
                }
                if directory {
                    self.run_deferred(abs);
                }
                true
            }
            None => false,
        };
        if directory {
            self.dir_frame_mut().defer = saved_defer;
            let policies = *self.policies.top();
            let id = self.current_dir_id();
            self.project.dir_mut(id).final_policies = Some(policies);
        }
        let report = !self.fatal;
        self.pop_list_file_vars(vars);
        self.pop_snapshot(report);
        self.pop_file_frame(saved_bt);
        read
    }

    /// Start of a directory listfile: use `CMAKE_POLICY_DEFAULT_CMP0198` when the
    /// policy is not set, then set `CMAKE_PARENT_LIST_FILE` for `CMP0198`.
    fn begin_directory_list_file(&mut self) {
        if self.policy_status(CMP0198) == PolicyStatus::Warn {
            let default = self.read_var(b"CMAKE_POLICY_DEFAULT_CMP0198");
            let status = match default.as_ref().map(|v| v.as_slice()) {
                Some(b"NEW") => Some(PolicyStatus::New),
                Some(b"OLD") => Some(PolicyStatus::Old),
                _ => None,
            };
            if let Some(status) = status {
                let _ = self.policies.try_update(|map| map.set(CMP0198, status));
            }
        }
        self.update_parent_list_file_var();
    }

    /// `cmMakefile::UpdateParentListFileVariable` (configure mode only): with `CMP0198`
    /// `NEW`, `CMAKE_PARENT_LIST_FILE` is undefined. Otherwise it names the
    /// `CMakeLists.txt` of the current directory.
    pub(crate) fn update_parent_list_file_var(&mut self) {
        if self.config.mode != RunMode::Configure {
            return;
        }
        if self.policy_status(CMP0198) == PolicyStatus::New {
            self.undefine(b"CMAKE_PARENT_LIST_FILE");
        } else {
            let lists = cat(&[&self.current_source_dir_bytes(), b"/CMakeLists.txt"]);
            self.define(b"CMAKE_PARENT_LIST_FILE", lists);
        }
    }

    // ----------------------------------------------------------------------------------
    // Variables.
    // ----------------------------------------------------------------------------------

    /// Normal binding value, else cache value (`GetDefinition`).
    pub(crate) fn lookup_var(&self, name: &[u8]) -> Option<&BString> {
        self.vars
            .get(name)
            .or_else(|| self.project.cache.get(name).map(|e| &e.value))
    }

    pub(crate) fn run_configure_impl(
        &mut self,
        source_dir: &Path,
        binary_dir: &Path,
        preload_scripts: &[PathBuf],
    ) -> RunOutcome {
        let cwd = rcmake_host::path::from_native(&self.config.cwd);
        let src_abs = collapse_full_path(&rcmake_host::path::from_native(source_dir), &cwd);
        let bin_abs = collapse_full_path(&rcmake_host::path::from_native(binary_dir), &cwd);
        let src_native = rcmake_host::path::to_native(&src_abs);
        let bin_native = rcmake_host::path::to_native(&bin_abs);

        self.init_policy_minimum();
        self.project.set_root_dirs(src_abs.clone(), bin_abs.clone());
        self.render.relative_base = Some(src_abs.clone());

        for var in [b"CMAKE_SOURCE_DIR".as_slice(), b"CMAKE_CURRENT_SOURCE_DIR"] {
            self.vars.set(var, src_abs.clone());
        }
        for var in [b"CMAKE_BINARY_DIR".as_slice(), b"CMAKE_CURRENT_BINARY_DIR"] {
            self.vars.set(var, bin_abs.clone());
        }

        let _ = self.config.host.create_dir_all(&bin_native);
        for preload in preload_scripts {
            self.run_preload_script_impl(preload);
            if self.fatal {
                return RunOutcome {
                    exit_code: self.exit_code(),
                };
            }
        }

        let lists_path = src_native.join("CMakeLists.txt");
        let lists_bytes = rcmake_host::path::from_native(&lists_path);
        if !self.is_regular_file(&lists_bytes) {
            self.error_no_location(cat(&[
                b"The source directory \"",
                &src_abs,
                b"\" does not appear to contain CMakeLists.txt.\nSpecify --help for usage, or press the help button on the CMake GUI.",
            ]));
            return RunOutcome {
                exit_code: self.exit_code(),
            };
        }

        let _ = self.read_list_file_impl(&lists_bytes, true);
        self.write_configure_log(&bin_native);
        self.snapshot_dir_vars(DirId::ROOT);
        if self.exit_code() != 0 && self.exit_override.is_none() {
            self.config.host.write_out(
                Stream::Stdout,
                b"-- Configuring incomplete, errors occurred!\n",
            );
        }
        RunOutcome {
            exit_code: self.exit_code(),
        }
    }

    pub(crate) fn run_preload_script_impl(&mut self, path: &Path) -> bool {
        self.init_policy_minimum();
        let cwd = rcmake_host::path::from_native(&self.config.cwd);
        let abs = collapse_full_path(&rcmake_host::path::from_native(path), &cwd);
        let display = path.to_string_lossy();
        self.config.host.write_out(
            Stream::Stdout,
            format!("loading initial cache file {display}\n").as_bytes(),
        );
        if !self.is_regular_file(&abs) {
            self.error_no_location(cat(&[b"Error processing file: ", &abs]));
            return false;
        }
        let base = self.lookup_var(b"CMAKE_SOURCE_DIR").cloned();
        let saved = core::mem::replace(&mut self.render.relative_base, base);
        let read = self.read_list_file(&abs);
        self.render.relative_base = saved;
        read
    }

    fn init_policy_minimum(&mut self) {
        const NAME: &[u8] = b"CMAKE_POLICY_VERSION_MINIMUM";
        if !self.project.cache.contains(NAME)
            && let Some(value) = self
                .config
                .host
                .env_get(NAME)
                .filter(|value| !value.is_empty())
        {
            self.project.cache.insert(
                NAME.into(),
                CacheEntry {
                    value,
                    ty: CacheType::String,
                    help: "Minimum policy version".into(),
                    advanced: false,
                    props: Default::default(),
                },
            );
        }
    }

    /// The directory being processed.
    pub(crate) fn current_dir_id(&self) -> DirId {
        self.dir_frame().id
    }

    /// Innermost directory frame.
    pub(crate) fn dir_frame(&self) -> &DirFrame {
        self.dir_stack.last().unwrap_or(&self.root_frame)
    }

    /// Mutable innermost directory frame.
    pub(crate) fn dir_frame_mut(&mut self) -> &mut DirFrame {
        match self.dir_stack.last_mut() {
            Some(frame) => frame,
            None => &mut self.root_frame,
        }
    }

    pub(crate) fn current_source_dir(&self) -> PathBuf {
        let dir = self.project.dir(self.current_dir_id());
        rcmake_host::path::to_native(&dir.source_dir)
    }

    pub(crate) fn current_binary_dir(&self) -> PathBuf {
        let dir = self.project.dir(self.current_dir_id());
        rcmake_host::path::to_native(&dir.binary_dir)
    }

    /// Record a listfile read in the current directory (`cmMakefile::ListFiles`).
    fn record_list_file(&mut self, abs: &[u8]) {
        let id = self.current_dir_id();
        let files = &mut self.project.dir_mut(id).listfiles;
        if !files.iter().any(|f| f.as_slice() == abs) {
            files.push(BString::from(abs));
        }
    }

    /// `cmMakefile::AddSubDirectory` + `ConfigureSubDirectory` (see
    /// [`crate::Cx::run_subdirectory`]).
    pub(crate) fn run_subdirectory(
        &mut self,
        src: &Path,
        bin: &Path,
        options: SubdirOptions,
    ) -> Result<DirId, CmdError> {
        if self.running_deferred {
            self.error_here("Subdirectories may not be created during deferred execution.".into());
            return Err(CmdError::Reported);
        }
        let src_abs = collapse_full_path(
            &rcmake_host::path::from_native(src),
            &self.current_source_dir_bytes(),
        );
        let bin_base = rcmake_host::path::from_native(&self.current_binary_dir());
        let bin_abs = collapse_full_path(&rcmake_host::path::from_native(bin), &bin_base);
        let lists = cat(&[&src_abs, b"/CMakeLists.txt"]);
        if !self.is_regular_file(&lists) {
            // CMake reports this from the new directory, so processing continues.
            self.error_here(cat(&[
                b"The source directory\n  ",
                &src_abs,
                b"\ndoes not contain a CMakeLists.txt file.",
            ]));
            return Err(CmdError::Reported);
        }
        let parent = self.current_dir_id();
        let bt = self.bt.clone();
        let Ok(id) = self
            .project
            .add_directory(parent, src_abs.clone(), bin_abs.clone(), bt)
        else {
            self.fatal_here(b"Too many directories.".into());
            return Err(CmdError::Reported);
        };
        let dir = self.project.dir_mut(id);
        dir.exclude_from_all |= options.exclude_from_all;
        dir.system |= options.system;
        if options.exclude_from_all {
            dir.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
        }
        if options.system {
            dir.props.set("SYSTEM".into(), "TRUE".into());
        }
        let _ = self
            .config
            .host
            .create_dir_all(&rcmake_host::path::to_native(&bin_abs));

        self.depth = self.depth.saturating_add(1);
        let limit = self.recursion_limit();
        if self.depth > limit {
            self.depth = self.depth.saturating_sub(1);
            self.fatal_here(format!("Maximum recursion depth of {limit} exceeded").into());
            return Err(CmdError::Reported);
        }
        self.vars.push();
        self.loop_blocks.push(0);
        self.policies.push_copy();
        self.diags.push_copy();
        let policy_base = self.policies.len() - 1;
        let diag_base = self.diags.len() - 1;
        self.dir_stack.push(DirFrame { id, defer: None });
        self.define(b"CMAKE_CURRENT_SOURCE_DIR", src_abs);
        self.define(b"CMAKE_CURRENT_BINARY_DIR", bin_abs);

        let _ = self.read_list_file_impl(&lists, true);
        self.snapshot_dir_vars(id);

        self.dir_stack.pop();
        self.diags.pop_above(diag_base);
        self.policies.pop_above(policy_base);
        self.loop_blocks.pop();
        self.vars.pop();
        self.depth = self.depth.saturating_sub(1);
        Ok(id)
    }

    /// Capture all visible normal variable definitions onto `Directory::vars` before a
    /// directory scope exits so generate-time resolution and lowering observe normal
    /// compiler rules, flags, and platform variables.
    fn snapshot_dir_vars(&mut self, id: DirId) {
        let names = self.vars.visible_names();
        let mut snap = indexmap::IndexMap::with_capacity(names.len());
        for name in names {
            if let Some(val) = self.vars.get(&name) {
                snap.insert(name, val.clone());
            }
        }
        self.project.dir_mut(id).vars = snap;
    }

    fn current_source_dir_bytes(&self) -> BString {
        rcmake_host::path::from_native(&self.current_source_dir())
    }

    /// `PARENT_SCOPE` for one variable (`cmMakefile::RaiseScope(var, def)`).
    pub(crate) fn raise_var(&mut self, name: &[u8], value: Option<BString>) {
        if name.is_empty() {
            return;
        }
        let watched = self.watches.contains_key(name).then(|| value.clone());
        if !self.vars.set_parent(name, value) {
            self.issue_here(
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Author),
                cat(&[b"Cannot set \"", name, b"\": current scope has no parent."]),
                true,
            );
            return;
        }
        if let Some(value) = watched {
            self.fire_watches(
                name,
                WatchAccess::Modified,
                value.as_ref().map(|v| v.as_slice()),
            );
        }
    }

    /// `cmMakefile::RaiseScope(vector)`.
    pub(crate) fn raise_vars(&mut self, names: &[BString]) {
        for name in names {
            let value = self.vars.get(name).cloned();
            self.raise_var(name, value);
        }
    }

    /// Push a file frame (`cmListFileContext::FromListFilePath`) and a `LISTFILE_STACK`
    /// entry. Returns the previous backtrace for [`Self::pop_file_frame`].
    pub(crate) fn push_file_frame(&mut self, abs: &[u8]) -> Backtrace {
        let saved = self.bt.clone();
        let file: Arc<[u8]> = Arc::from(abs);
        self.listfiles.push(Arc::clone(&file));
        self.file_locks.push(Default::default());
        self.bt = self.bt.push(Frame {
            file,
            line: 0,
            name: None,
            kind: FrameKind::ModuleInclude,
        });
        saved
    }

    /// Undo [`Self::push_file_frame`].
    pub(crate) fn pop_file_frame(&mut self, saved: Backtrace) {
        self.file_locks.pop();
        self.listfiles.pop();
        self.bt = saved;
    }

    pub(crate) fn push_list_file_vars(&mut self, new_current: &[u8]) -> ListFileVars {
        let old_parent = self.read_var(b"CMAKE_PARENT_LIST_FILE");
        let old_current = self.read_var(b"CMAKE_CURRENT_LIST_FILE");
        if let Some(c) = &old_current {
            self.define(b"CMAKE_PARENT_LIST_FILE", c.clone());
        }
        self.define(b"CMAKE_CURRENT_LIST_FILE", BString::from(new_current));
        self.define(b"CMAKE_CURRENT_LIST_DIR", filename_path(new_current));
        ListFileVars {
            old_parent,
            old_current: old_current.unwrap_or_default(),
        }
    }

    pub(crate) fn pop_list_file_vars(&mut self, saved: ListFileVars) {
        match saved.old_parent {
            Some(p) => self.define(b"CMAKE_PARENT_LIST_FILE", p),
            None => self.undefine(b"CMAKE_PARENT_LIST_FILE"),
        }
        let dir = filename_path(&saved.old_current);
        self.define(b"CMAKE_CURRENT_LIST_FILE", saved.old_current);
        self.define(b"CMAKE_CURRENT_LIST_DIR", dir);
    }

    /// `ClearMatches` followed by `StoreMatches` when `caps` is `Some`.
    pub(crate) fn store_matches(&mut self, caps: Option<(&Captures, &[u8])>) {
        if let Some(count) = self.lookup_var(b"CMAKE_MATCH_COUNT").cloned() {
            let n = crate::util::stoi(&count).unwrap_or(0).clamp(0, 9);
            for i in 0..=n {
                let name = format!("CMAKE_MATCH_{i}");
                if self
                    .lookup_var(name.as_bytes())
                    .is_some_and(|v| !v.is_empty())
                {
                    self.define(name.as_bytes(), BString::default());
                }
            }
            self.define(b"CMAKE_MATCH_COUNT", "0".into());
        }
        let Some((caps, hay)) = caps else { return };
        let (values, highest) = caps.match_vars(hay);
        for (i, value) in values.into_iter().enumerate() {
            if !value.is_empty() {
                self.define(format!("CMAKE_MATCH_{i}").as_bytes(), value);
            }
        }
        let count = highest.map(|h| h.to_string()).unwrap_or_default();
        self.define(b"CMAKE_MATCH_COUNT", count.into());
    }

    // ----------------------------------------------------------------------------------
    // Policies and diagnostics.
    // ----------------------------------------------------------------------------------

    pub(crate) fn policy_status(&self, id: PolicyId) -> PolicyStatus {
        if id.info().is_some_and(|i| i.removed_old) {
            return PolicyStatus::New;
        }
        self.policies.top().get(id)
    }

    pub(crate) fn policies_top_mut(&mut self) -> &mut PolicyMap {
        self.policies.top_mut()
    }

    pub(crate) fn diag_top(&self) -> &DiagState {
        self.diags.top()
    }

    pub(crate) fn diag_top_mut(&mut self) -> &mut DiagState {
        self.diags.top_mut()
    }

    fn snapshot_bases(&self) -> Snapshot {
        self.snapshots.last().copied().unwrap_or(Snapshot {
            policy_base: 1,
            diag_base: 1,
        })
    }

    pub(crate) fn push_snapshot(&mut self) {
        self.snapshots.push(Snapshot {
            policy_base: self.policies.len(),
            diag_base: self.diags.len(),
        });
    }

    /// `cmMakefile::PopSnapshot`: reject still-open nested policy/diagnostic scopes.
    pub(crate) fn pop_snapshot(&mut self, mut report: bool) {
        let Some(snap) = self.snapshots.pop() else {
            return;
        };
        loop {
            if self.policies.len() > snap.policy_base {
                if report {
                    self.error_here(b"cmake_policy PUSH without matching POP".into());
                    report = false;
                }
                self.policies.pop_above(snap.policy_base);
            } else if self.diags.len() > snap.diag_base {
                if report {
                    self.error_here(b"cmake_diagnostic PUSH without matching POP".into());
                    report = false;
                }
                self.diags.pop_above(snap.diag_base);
            } else {
                break;
            }
        }
    }

    pub(crate) fn pop_policy(&mut self) -> Result<(), CmdError> {
        let base = self.snapshot_bases().policy_base;
        if self.policies.pop_above(base) {
            Ok(())
        } else {
            self.error_here(b"cmake_policy POP without matching PUSH".into());
            Err(CmdError::Reported)
        }
    }

    pub(crate) fn pop_diag(&mut self) -> Result<(), CmdError> {
        let base = self.snapshot_bases().diag_base;
        if self.diags.pop_above(base) {
            Ok(())
        } else {
            self.error_here(b"cmake_diagnostic POP without matching PUSH".into());
            Err(CmdError::Reported)
        }
    }

    /// Write a rendered diagnostic to stderr through the [`rcmake_diag::DiagEngine`].
    pub(crate) fn emit(&mut self, diag: &Diagnostic, state: &DiagState) {
        let shown;
        let diag = if let Location::Backtrace(bt) = &diag.location {
            shown = Diagnostic {
                location: Location::Backtrace(display_backtrace(bt)),
                ..diag.clone()
            };
            &shown
        } else {
            diag
        };
        if let Some(bytes) = self.engine.issue(diag, state, &self.render) {
            self.config.host.write_out(Stream::Stderr, &bytes);
        }
    }

    /// Issue a diagnostic at the current backtrace.
    ///
    /// `mark_failed` marks the current command as failed when the effective severity is
    /// an error (`cmMakefile::IssueMessage` / `IssueDiagnostic`). `message()` passes
    /// `false` because it displays through the messenger directly.
    pub(crate) fn issue_here(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        body: BString,
        mark_failed: bool,
    ) {
        let location = Location::Backtrace(self.bt.clone());
        self.issue_at(kind, effect, category, None, location, body, mark_failed);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_at(
        &mut self,
        kind: MessageKind,
        effect: Effect,
        category: Option<DiagCategory>,
        policy: Option<PolicyId>,
        location: Location,
        body: BString,
        mark_failed: bool,
    ) {
        let state = *self.diag_top();
        let (kind, effect) = match category {
            Some(cat) => match state.get(cat) {
                DiagAction::Ignore => return,
                DiagAction::Warn => (warning_kind(kind), Effect::None),
                DiagAction::SendError => (error_kind(kind), Effect::SetError),
                DiagAction::FatalError => (error_kind(kind), Effect::Fatal),
            },
            None => (kind, effect),
        };
        match effect {
            Effect::None => {}
            Effect::SetError => {
                if mark_failed {
                    self.cmd_failed = true;
                }
            }
            Effect::Fatal => {
                self.fatal = true;
                if mark_failed {
                    self.cmd_failed = true;
                }
            }
        }
        let diag = Diagnostic {
            kind,
            effect,
            category,
            policy,
            location,
            body,
        };
        self.emit(&diag, &state);
    }

    /// `IssueMessage(FATAL_ERROR, text)`: an error that fails the current command.
    pub(crate) fn error_here(&mut self, body: BString) {
        self.issue_here(MessageKind::Error, Effect::SetError, None, body, true);
    }

    /// `IssueMessage(FATAL_ERROR, text)` plus `SetFatalErrorOccurred`.
    pub(crate) fn fatal_here(&mut self, body: BString) {
        self.issue_here(MessageKind::Error, Effect::Fatal, None, body, true);
    }

    /// `cmSystemTools::Error(text)`: `CMake Error: <text>`.
    pub(crate) fn error_no_location(&mut self, body: BString) {
        let state = *self.diag_top();
        let diag = Diagnostic {
            kind: MessageKind::Error,
            effect: Effect::SetError,
            category: None,
            policy: None,
            location: Location::None,
            body,
        };
        self.emit(&diag, &state);
    }

    /// `cmPolicies::GetPolicyWarning`.
    pub(crate) fn policy_warning_text(id: PolicyId) -> BString {
        let summary = id
            .info()
            .map(|i| crate::util::replace_all(i.summary.as_bytes(), b"``", b""))
            .unwrap_or_default();
        let name = id.to_string();
        cat(&[
            b"Policy ",
            name.as_bytes(),
            b" is not set: ",
            &summary,
            b"  Run \"cmake --help-policy ",
            name.as_bytes(),
            b"\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.",
        ])
    }

    pub(crate) fn issue_policy_warning(&mut self, id: PolicyId, preface: &[u8], postface: &[u8]) {
        let warning = Self::policy_warning_text(id);
        let mut body = Vec::new();
        if !preface.is_empty() {
            body.extend_from_slice(preface);
            body.push(b'\n');
        }
        body.extend_from_slice(&warning);
        if !postface.is_empty() {
            body.push(b'\n');
            body.extend_from_slice(postface);
        }
        let location = Location::Backtrace(self.bt.clone());
        self.issue_at(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Policy),
            Some(id),
            location,
            BString::from(body),
            true,
        );
    }

    pub(crate) fn log_level(&self) -> LogLevel {
        if let Some(level) = self.log_level {
            return level;
        }
        self.lookup_var(b"CMAKE_MESSAGE_LOG_LEVEL")
            .and_then(|v| LogLevel::parse(v))
            .unwrap_or(LogLevel::Status)
    }

    // ----------------------------------------------------------------------------------
    // Commands.
    // ----------------------------------------------------------------------------------

    pub(crate) fn command_exists(&self, name: &[u8]) -> bool {
        let lower = fold_command_name(name);
        FLOW_CONTROL.contains(&lower.as_slice())
            || self.scripted.contains_key(&lower)
            || self.registry.get_lower(&lower).is_some()
    }

    pub(crate) fn lookup_command(&self, lower: &[u8]) -> Option<Command> {
        if let Some(cmd) = self.scripted.get(lower) {
            return Some(cmd.clone());
        }
        self.registry
            .get_lower(lower)
            .map(|b| Command::Builtin(b.func, b.flags, b.raw))
    }

    /// Sorted names of all known commands (`cmState::GetCommandNames`).
    fn command_names(&self) -> Vec<BString> {
        let mut names: Vec<BString> = FLOW_CONTROL.iter().map(|n| BString::from(*n)).collect();
        names.extend(self.registry.names().cloned());
        names.extend(self.scripted.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    /// `cmState::AddScriptedCommand`.
    pub(crate) fn add_scripted_command(&mut self, cmd: Arc<UserCommand>) -> bool {
        let lower = fold_command_name(&cmd.name);
        if FLOW_CONTROL.contains(&lower.as_slice()) {
            self.fatal_here(cat(&[
                b"Built-in flow control command \"",
                &lower,
                b"\" cannot be overridden.",
            ]));
            return false;
        }
        if let Some(old) = self.lookup_command(&lower) {
            self.scripted.insert(cat(&[b"_", &lower]), old);
        }
        self.scripted.insert(lower, Command::User(cmd));
        true
    }

    fn recursion_limit(&self) -> u32 {
        let configured = self
            .lookup_var(b"CMAKE_MAXIMUM_RECURSION_DEPTH")
            .filter(|v| !v.is_empty() && v.iter().all(u8::is_ascii_digit))
            .and_then(|v| core::str::from_utf8(v).ok()?.parse::<u64>().ok())
            .map_or(self.config.limits.max_recursion, |v| {
                u32::try_from(v).unwrap_or(u32::MAX)
            });
        configured.min(HARD_RECURSION_CEILING)
    }

    /// Get the current backtrace with a new command frame.
    pub(crate) fn command_backtrace(&self, file: &Arc<[u8]>, line: u32, name: &[u8]) -> Backtrace {
        self.bt.push(Frame {
            file: Arc::clone(file),
            line,
            name: Some(Arc::from(name)),
            kind: FrameKind::Command,
        })
    }

    /// Run `f` with a call frame for `name` at `file:line` (`CallScope`), enforce the
    /// recursion limit, and convert command errors into diagnostics.
    pub(crate) fn with_command_frame(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: &[u8],
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        self.with_frame(file, line, name, None, f)
    }

    /// [`Self::with_command_frame`] with an optional deferred-call identifier
    /// (`cmListFileContext::DeferId`).
    fn with_frame(
        &mut self,
        file: &Arc<[u8]>,
        line: u32,
        name: &[u8],
        defer_id: Option<BString>,
        f: impl FnOnce(&mut Self) -> CmdResult,
    ) -> CmdResult {
        let bt = self.command_backtrace(file, line, name);
        let saved_bt = core::mem::replace(&mut self.bt, bt);
        let saved_defer = core::mem::replace(&mut self.top_defer_id, defer_id);
        self.depth = self.depth.saturating_add(1);
        let saved_line = core::mem::replace(&mut self.cur_line, line);
        let saved_failed = core::mem::replace(&mut self.cmd_failed, false);

        let limit = self.recursion_limit();
        let result = if self.depth > limit {
            self.fatal_here(format!("Maximum recursion depth of {limit} exceeded").into());
            Err(CmdError::Reported)
        } else {
            f(self)
        };
        let result = self.finish_command(name, result);

        self.cmd_failed = saved_failed;
        self.cur_line = saved_line;
        self.depth = self.depth.saturating_sub(1);
        self.top_defer_id = saved_defer;
        self.bt = saved_bt;
        result
    }

    fn finish_command(&mut self, name: &[u8], result: CmdResult) -> CmdResult {
        let failed = match result {
            Err(CmdError::Status(text)) => {
                self.error_here(cat(&[name, b" ", &text]));
                true
            }
            Err(CmdError::Reported) => true,
            Ok(flow) => {
                if !self.cmd_failed {
                    return Ok(flow);
                }
                true
            }
        };
        if failed && self.config.mode == RunMode::Script {
            self.fatal = true;
        }
        Err(CmdError::Reported)
    }

    /// Expand arguments with `rcmake_expr::expand_args`, one argument at a time
    /// (`cmMakefile::ExpandArguments`). An expansion error is fatal and reads
    /// `Syntax error in cmake code at <file>:<line> when parsing string <arg> <error>`.
    pub(crate) fn expand(&mut self, args: &[Argument]) -> Result<Vec<ExpandedArg>, CmdError> {
        let mut out = Vec::new();
        let expand = self.frontend.expand;
        let mut failed = false;
        for arg in args {
            // Short literal arguments need no variable lookups, escape processing,
            // or list splitting. Keep their original kind and line number.
            let text = arg.text.as_slice();
            if text.len() <= 64
                && !text.iter().any(|&b| {
                    matches!(b, b'$' | b'\\') || (b == b';' && arg.kind == ArgKind::Unquoted)
                })
            {
                if !text.is_empty() || arg.kind != ArgKind::Unquoted {
                    out.push(ExpandedArg {
                        value: arg.text.clone(),
                        kind: arg.kind,
                        line: arg.line,
                    });
                }
                continue;
            }
            if let Err(e) = expand(core::slice::from_ref(arg), self, &mut out) {
                let body = self.syntax_error_text(arg, e.message);
                self.fatal_here(body);
                failed = true;
            }
        }
        if failed {
            Err(CmdError::Reported)
        } else {
            Ok(out)
        }
    }

    /// `ExpandVariablesInStringImpl` error text for an argument of the command on top of
    /// the backtrace.
    fn syntax_error_text(&self, arg: &Argument, message: BString) -> BString {
        const PREFIX: &[u8] = b"Syntax error in cmake code ";
        if message.starts_with(PREFIX) {
            return message;
        }
        let mut out = PREFIX.to_vec();
        if let Some(top) = self.bt.top() {
            out.extend_from_slice(b"at\n  ");
            out.extend_from_slice(&top.file);
            out.push(b':');
            out.extend_from_slice(display_line(arg.line).as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"when parsing string\n  ");
        out.extend_from_slice(&arg.text);
        out.push(b'\n');
        out.extend_from_slice(&message);
        BString::from(out)
    }

    /// Execute the ordinary command at index `i` of `code`.
    pub(crate) fn exec_command(&mut self, code: &Code, i: usize) -> CmdResult {
        let Some(cmd) = code.cmds.get(i) else {
            return Ok(Flow::Normal);
        };
        let saved = self.cur_cmd.replace((Arc::clone(&code.cmds), i));
        let result = self.with_command_frame(&code.file, cmd.line, &cmd.name, |this| {
            this.dispatch(cmd, &code.file)
        });
        self.cur_cmd = saved;
        result
    }

    /// Execute the deferred call in `code` (one command) with identifier `id`.
    pub(crate) fn exec_deferred(&mut self, code: &Code, id: BString) {
        let Some(cmd) = code.cmds.first() else {
            return;
        };
        let saved = self.cur_cmd.replace((Arc::clone(&code.cmds), 0));
        let vars = self.push_list_file_vars(&code.file);
        let _ = self.with_frame(&code.file, cmd.line, &cmd.name, Some(id), |this| {
            this.dispatch(cmd, &code.file)
        });
        self.pop_list_file_vars(vars);
        self.cur_cmd = saved;
    }

    fn dispatch(&mut self, cmd: &CommandInvocation, file: &[u8]) -> CmdResult {
        if self.fatal {
            return Ok(Flow::Normal);
        }
        let lower = cmd.lower.as_slice();
        let command = self.lookup_command(lower);
        if self.trace_enabled() && (command.is_some() || unexpected_text(lower).is_some()) {
            self.trace_command(cmd, file, false);
        }
        if let Some(text) = unexpected_text(lower) {
            return Err(CmdError::Status(text.into()));
        }
        let Some(command) = command else {
            self.unknown_command(&cmd.name);
            return Err(CmdError::Reported);
        };
        if matches!(command, Command::Builtin(_, _, true)) {
            return self.invoke(command, &[]);
        }
        let args = self.expand(&cmd.args)?;
        if self.running_deferred
            && lower == b"enable_language"
            && matches!(command, Command::Builtin(..))
        {
            self.error_here("Languages may not be enabled during deferred execution.".into());
            return Err(CmdError::Reported);
        }
        self.invoke(command, &args)
    }

    fn unknown_command(&mut self, name: &[u8]) {
        let mut msg = cat(&[b"Unknown CMake command \"", name, b"\"."]);
        let names = self.command_names();
        if let Some(s) = closest(name, &names) {
            msg.extend_from_slice(b"  Did you mean: \"");
            msg.extend_from_slice(s);
            msg.extend_from_slice(b"\"?");
        }
        self.fatal_here(msg);
    }

    fn invoke(&mut self, command: Command, args: &[ExpandedArg]) -> CmdResult {
        match command {
            Command::Builtin(f, flags, _) => {
                if self.config.mode == RunMode::Script && !flags.scriptable {
                    return Err(CmdError::Status("command is not scriptable".into()));
                }
                // Install rules use this setting at the call site.
                let first_install = self.project.installs.len();
                let result = f(&mut Cx { interp: self }, args);
                if first_install < self.project.installs.len()
                    && self
                        .bt
                        .top()
                        .and_then(|frame| frame.name.as_deref())
                        .is_some_and(|name| name.eq_ignore_ascii_case(b"install"))
                    && let Some(level) = self.lookup_var(b"CMAKE_INSTALL_MESSAGE").cloned()
                {
                    for index in first_install..self.project.installs.len() {
                        self.project.global.props.set(
                            format!("_RCMAKE_INSTALL_{index}_MESSAGE").into(),
                            level.clone(),
                        );
                    }
                }
                result
            }
            Command::User(u) => match u.kind {
                UserCommandKind::Function => self.call_function(&u, args),
                UserCommandKind::Macro => self.call_macro(&u, args),
            },
        }
    }

    /// `Cx::call_command`: run a command with pre-expanded arguments under a frame.
    pub(crate) fn call_command(&mut self, name: &[u8], args: Vec<ExpandedArg>) -> CmdResult {
        let lower = fold_command_name(name);
        if crate::code::opener_end(&lower).is_some() || unexpected_text(&lower).is_some() {
            return Err(CmdError::Status(cat(&[
                b"invalid command specified: ",
                name,
            ])));
        }
        let file = self
            .bt
            .top()
            .map_or_else(|| Arc::from(&b""[..]), |f| Arc::clone(&f.file));
        let line = self.cur_line;
        let Some(command) = self.lookup_command(&lower) else {
            let saved = core::mem::take(&mut self.bt);
            let result = self.with_command_frame(&file, line, name, |this| {
                this.unknown_command(name);
                Err(CmdError::Reported)
            });
            self.bt = saved;
            return result;
        };
        // A command that expands its own arguments reads them from `cur_cmd`. The
        // arguments are expanded already, so pass them as bracket arguments.
        let raw = matches!(command, Command::Builtin(_, _, true)).then(|| {
            let args = args
                .iter()
                .map(|a| Argument {
                    kind: ArgKind::Bracket,
                    text: a.value.clone(),
                    line,
                    span: 0..0,
                })
                .collect();
            let cmd = CommandInvocation {
                name: BString::from(name),
                lower: lower.clone(),
                line,
                close_line: line,
                span: 0..0,
                args,
            };
            (Arc::from(vec![cmd]), 0)
        });
        let args = if raw.is_some() { Vec::new() } else { args };
        let saved = core::mem::replace(&mut self.cur_cmd, raw);
        let result = self.with_command_frame(&file, line, name, |this| this.invoke(command, &args));
        self.cur_cmd = saved;
        result
    }

    /// Run commands `start..end` of `code`.
    pub(crate) fn run_code(
        &mut self,
        code: &Code,
        start: usize,
        end: usize,
        mode: BodyMode,
    ) -> CmdResult {
        let mut i = start;
        while i < end {
            if self.fatal || self.over_budget() {
                return Err(CmdError::Reported);
            }
            let (result, next) = self.exec_at(code, i);
            i = next;
            if self.fatal {
                return Err(CmdError::Reported);
            }
            match result {
                Ok(Flow::Normal) => {}
                Ok(flow @ Flow::Return { .. }) => return Ok(flow),
                Ok(flow) => {
                    if mode != BodyMode::File {
                        return Ok(flow);
                    }
                }
                Err(e) => {
                    if matches!(mode, BodyMode::Function | BodyMode::Macro) {
                        return Err(e);
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    /// Stop execution if the optional wall-clock budget has expired.
    pub(crate) fn over_budget(&mut self) -> bool {
        let Some(budget) = self.config.limits.wall_budget else {
            return false;
        };
        if self.config.host.monotonic().saturating_sub(self.started) <= budget {
            return false;
        }
        self.fatal_here("Execution exceeded the configured wall-clock budget.".into());
        true
    }

    /// Execute the command at `i` and return the index of the next command.
    fn exec_at(&mut self, code: &Code, i: usize) -> (CmdResult, usize) {
        let Some(cmd) = code.cmds.get(i) else {
            return (Ok(Flow::Normal), i + 1);
        };
        let result = match cmd.lower.as_slice() {
            b"if" => self.exec_if(code, i),
            b"foreach" => self.exec_foreach(code, i),
            b"while" => self.exec_while(code, i),
            b"function" => self.exec_define(code, i, UserCommandKind::Function),
            b"macro" => self.exec_define(code, i, UserCommandKind::Macro),
            b"block" => self.exec_block(code, i),
            _ => return (self.exec_command(code, i), i + 1),
        };
        match result {
            Ok(result) => (result, code.end_of(i).max(i) + 1),
            // The opener failed and did not open a block: run its body as ordinary
            // commands, so that the end command reports its own error.
            Err(e) => (Err(e), i + 1),
        }
    }

    // ----------------------------------------------------------------------------------
    // Files.
    // ----------------------------------------------------------------------------------

    pub(crate) fn is_regular_file(&self, abs: &[u8]) -> bool {
        let native = rcmake_host::path::to_native(abs);
        self.config
            .host
            .metadata(&native, Follow::Yes)
            .is_ok_and(|m| m.kind != FileKind::Dir)
    }

    pub(crate) fn exists(&self, abs: &[u8]) -> bool {
        let native = rcmake_host::path::to_native(abs);
        self.config.host.metadata(&native, Follow::Yes).is_ok()
    }

    pub(crate) fn is_dir(&self, abs: &[u8]) -> bool {
        let native = rcmake_host::path::to_native(abs);
        self.config
            .host
            .metadata(&native, Follow::Yes)
            .is_ok_and(|m| m.kind == FileKind::Dir)
    }

    /// Read and parse a listfile. Reports read and parse errors. Returns `None` when the
    /// file is missing, is a directory, or does not parse.
    pub(crate) fn parse_file(&mut self, abs: &[u8]) -> Option<Code> {
        if !self.is_regular_file(abs) {
            return None;
        }
        let native = rcmake_host::path::to_native(abs);
        let limit = self.config.limits.max_file_read;
        let Ok(data) = self.config.host.read(&native, ReadRange::All { limit }) else {
            self.issue_here(
                MessageKind::Error,
                Effect::SetError,
                None,
                "cmListFileCache: error can not open file.".into(),
                false,
            );
            return None;
        };
        self.parse_source(abs, &data, SourceKind::File)
    }

    fn parse_source(&mut self, path: &[u8], data: &[u8], kind: SourceKind) -> Option<Code> {
        let text: Arc<[u8]> = Arc::from(data);
        let id = self.sources.add(BString::from(path), kind, text)?;
        let file: Arc<[u8]> = Arc::from(path);
        let (parsed, warnings) =
            match rcmake_syntax::parse_list_file_with_warnings(data, id, kind, &self.config.limits)
            {
                Ok(mut list) => {
                    let warnings = std::mem::take(&mut list.warnings);
                    (Ok(list), warnings)
                }
                Err((err, warnings)) => (Err(err), warnings),
            };
        for mut warning in warnings {
            let line = match &warning.location {
                Location::FileLine { line, .. } => *line,
                _ => 0,
            };
            warning.location = Location::Backtrace(self.bt.push(Frame {
                file: Arc::clone(&file),
                line,
                name: None,
                kind: FrameKind::Command,
            }));
            let state = *self.diag_top();
            self.emit(&warning, &state);
        }
        match parsed {
            Ok(list) => Some(Code::new(list.commands, file)),
            Err(err) => {
                self.report_parse_error(&file, &err);
                None
            }
        }
    }

    /// Report a listfile parse error.
    ///
    /// - A file-level error (no column and no command: a non-UTF-8 Byte-Order-Mark or an
    ///   input over the size limit) is located `in <file>:` and does not stop processing
    ///   (`IssueFileOpenError`).
    /// - Other syntax and nesting errors are located at their line. They stop processing
    ///   (`IssueError`, `SetFatalErrorOccurred`), except a missing `)` at the end of
    ///   the file and an argument that is not separated by whitespace.
    fn report_parse_error(&mut self, file: &Arc<[u8]>, err: &ParseError) {
        const NOT_FATAL: [&[u8]; 2] = [
            b"Parse error.  Function missing ending \")\".  End of file reached.",
            b"Syntax Error in cmake code at column ",
        ];
        let file_level = err.line == 0 || (err.column == 0 && err.command.is_none());
        let fatal = !file_level && !NOT_FATAL.iter().any(|m| err.message.starts_with(m));
        let bt = if file_level {
            self.bt.clone()
        } else {
            self.bt.push(Frame {
                file: Arc::clone(file),
                line: err.line,
                name: err.command.as_ref().map(|c| Arc::from(c.as_slice())),
                kind: FrameKind::Command,
            })
        };
        let effect = if fatal {
            Effect::Fatal
        } else {
            Effect::SetError
        };
        self.issue_at(
            MessageKind::Error,
            effect,
            None,
            None,
            Location::Backtrace(bt),
            err.message.clone(),
            true,
        );
    }

    /// `cmMakefile::ReadDependentFile` (`IncludeScope`). Returns `false` when the file
    /// can not be read or parsed.
    pub(crate) fn read_dependent_file(&mut self, abs: &[u8], options: IncludeOptions) -> bool {
        self.record_list_file(abs);
        let saved_bt = self.push_file_frame(abs);
        self.push_snapshot();
        if !options.no_policy_scope {
            self.policies.push_copy();
        }
        if !options.no_diagnostic_scope {
            self.diags.push_copy();
        }
        let vars = self.push_list_file_vars(abs);
        let read = match self.parse_file(abs) {
            Some(code) => {
                let r = self.run_code(&code, 0, code.len(), BodyMode::File);
                if let Ok(Flow::Return {
                    propagate: Some(names),
                }) = &r
                {
                    self.raise_vars(names);
                }
                true
            }
            None => false,
        };
        let report = !self.fatal;
        self.pop_list_file_vars(vars);
        let base = self.snapshot_bases();
        if !options.no_diagnostic_scope {
            self.diags.pop_above(base.diag_base);
        }
        if !options.no_policy_scope {
            self.policies.pop_above(base.policy_base);
        }
        self.pop_snapshot(report);
        self.pop_file_frame(saved_bt);
        read
    }

    /// `Cx::include_file`.
    pub(crate) fn include_file(&mut self, path: &Path, options: IncludeOptions) -> CmdResult {
        let base = self.current_source_dir_bytes();
        let shown = rcmake_host::path::from_native(path);
        let abs = collapse_full_path(&shown, &base);
        if !self.exists(&abs) || self.is_dir(&abs) {
            if options.optional {
                return Ok(Flow::Normal);
            }
            let what: &[u8] = if self.exists(&abs) {
                b"requested file is a directory:\n  "
            } else {
                b"could not find requested file:\n  "
            };
            return Err(CmdError::Status(cat(&[what, &shown])));
        }
        if self.read_dependent_file(&abs, options) {
            return Ok(Flow::Normal);
        }
        if options.optional {
            return Ok(Flow::Normal);
        }
        if self.fatal {
            return Err(CmdError::Reported);
        }
        Err(CmdError::Status(cat(&[
            b"could not load requested file:\n  ",
            &shown,
        ])))
    }

    /// `cmake_language(EVAL CODE ...)`: parse `src` as a virtual listfile named
    /// `<file>:<line>:EVAL` and run it in the current scope.
    pub(crate) fn eval_code(&mut self, src: &[u8]) -> CmdResult {
        let current = self
            .lookup_var(b"CMAKE_CURRENT_LIST_FILE")
            .cloned()
            .unwrap_or_default();
        let name = cat(&[
            &current,
            b":",
            self.cur_line.to_string().as_bytes(),
            b":EVAL",
        ]);
        let base = self.current_source_dir_bytes();
        let abs = collapse_full_path(&name, &base);
        let saved_bt = self.push_file_frame(&abs);
        let vars = self.push_list_file_vars(&abs);
        self.push_snapshot();
        let result = match self.parse_source(&abs, src, SourceKind::VirtualEval) {
            Some(code) => {
                let r = self.run_code(&code, 0, code.len(), BodyMode::File);
                if let Ok(Flow::Return {
                    propagate: Some(names),
                }) = &r
                {
                    self.raise_vars(names);
                }
                Ok(Flow::Normal)
            }
            None => Err(CmdError::Reported),
        };
        let report = !self.fatal;
        self.pop_list_file_vars(vars);
        self.pop_snapshot(report);
        self.pop_file_frame(saved_bt);
        result
    }

    // ----------------------------------------------------------------------------------
    // Function and macro calls.
    // ----------------------------------------------------------------------------------

    fn push_call_scope(&mut self, u: &UserCommand, function: bool) {
        self.push_snapshot();
        if function {
            self.vars.push();
            self.loop_blocks.push(0);
            self.listfiles.push(Arc::clone(&u.body.file));
            self.function_locks.push(Default::default());
        }
        self.policies.push_weak(u.policies);
        self.diags.push_weak(u.diags);
    }

    fn pop_call_scope(&mut self, function: bool, report: bool) {
        let base = self.snapshot_bases();
        self.diags.pop_above(base.diag_base);
        self.policies.pop_above(base.policy_base);
        self.pop_snapshot(report);
        if function {
            self.vars.pop();
            self.loop_blocks.pop();
            self.function_locks.pop();
            self.listfiles.pop();
        }
    }

    fn call_function(&mut self, u: &Arc<UserCommand>, args: &[ExpandedArg]) -> CmdResult {
        let values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        if values.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Function invoked with incorrect arguments for function named: ",
                &u.name,
            ])));
        }
        self.push_call_scope(u, true);

        self.define(b"ARGC", values.len().to_string().into());
        for (i, v) in values.iter().enumerate() {
            self.define(format!("ARGV{i}").as_bytes(), v.clone());
        }
        for (param, v) in u.params.iter().zip(values.iter()) {
            self.define(param, v.clone());
        }
        let argn = values.get(u.params.len()..).unwrap_or_default();
        self.define(b"ARGV", rcmake_base::list::join(values.iter()));
        self.define(b"ARGN", rcmake_base::list::join(argn.iter()));
        self.define(b"ARGNC", argn.len().to_string().into());
        self.define(b"CMAKE_CURRENT_FUNCTION", u.name.clone());
        let file = BString::from(&u.body.file[..]);
        self.define(b"CMAKE_CURRENT_FUNCTION_LIST_DIR", filename_path(&file));
        self.define(b"CMAKE_CURRENT_FUNCTION_LIST_FILE", file);
        self.define(
            b"CMAKE_CURRENT_FUNCTION_LIST_LINE",
            u.line.to_string().into(),
        );

        let result = self.run_code(&u.body, 0, u.body.len(), BodyMode::Function);
        if let Ok(Flow::Return {
            propagate: Some(names),
        }) = &result
        {
            self.raise_vars(names);
        }
        let report = result.is_ok() && !self.fatal;
        self.pop_call_scope(true, report);
        result.map(|_| Flow::Normal)
    }

    /// `cmMakefile::CheckCMP0219`.
    pub(crate) fn check_cmp0219(&mut self, callee: &BString, values: &[BString]) -> PolicyStatus {
        let status = self.policy_status(PolicyId::new(219));
        let has_backslash = values.iter().any(|v| v.contains(&b'\\'));
        if status == PolicyStatus::Warn
            && has_backslash
            && self.warned_cmp0219.insert(callee.clone())
        {
            return PolicyStatus::Warn;
        }
        if status == PolicyStatus::New {
            PolicyStatus::New
        } else {
            PolicyStatus::Old
        }
    }

    pub(crate) fn issue_cmp0219_warning(&mut self, callee: &[u8], values: &[BString]) {
        let mut old_args = Vec::new();
        for v in values.iter().filter(|v| v.contains(&b'\\')) {
            if !old_args.is_empty() {
                old_args.push(b'\n');
            }
            old_args.extend_from_slice(b" \"");
            old_args.extend_from_slice(v);
            old_args.push(b'"');
        }
        let new_args = crate::util::replace_all(&old_args, b"\\", b"\\\\");
        let postface = cat(&[
            b"Command \"",
            callee,
            b"\" called with arguments containing backslashes.\nSince the policy is not set, backslashes in the arguments:\n",
            &old_args,
            b"\nwill be interpreted as escape sequences for compatibility.\nSet the policy to NEW to instead pass\n",
            &new_args,
            b"\nso that argument parsing will preserve the original values.",
        ]);
        self.issue_policy_warning(PolicyId::new(219), b"", &postface);
    }

    fn call_macro(&mut self, u: &Arc<UserCommand>, args: &[ExpandedArg]) -> CmdResult {
        let mut values: Vec<BString> = args.iter().map(|a| a.value.clone()).collect();
        if values.len() < u.params.len() {
            return Err(CmdError::Status(cat(&[
                b"Macro invoked with incorrect arguments for macro named: ",
                &u.name,
            ])));
        }
        let argc = values.len().to_string();
        let mut argn =
            rcmake_base::list::join(values.get(u.params.len()..).unwrap_or_default().iter());
        let mut argv = rcmake_base::list::join(values.iter());
        match self.check_cmp0219(&u.name, &values) {
            PolicyStatus::New => {
                for v in &mut values {
                    *v = crate::util::escape_backslashes(v);
                }
                argn = crate::util::escape_backslashes(&argn);
                argv = crate::util::escape_backslashes(&argv);
            }
            PolicyStatus::Warn => self.issue_cmp0219_warning(&u.name, &values),
            PolicyStatus::Old => {}
        }

        let formals: Vec<BString> = u.params.iter().map(|p| cat(&[b"${", p, b"}"])).collect();
        let argvs: Vec<BString> = (0..values.len())
            .map(|i| BString::from(format!("${{ARGV{i}}}")))
            .collect();
        let substitute = |text: &[u8]| -> BString {
            let mut s = text.to_vec();
            for (formal, value) in formals.iter().zip(values.iter()) {
                s = crate::util::replace_all(&s, formal, value);
            }
            s = crate::util::replace_all(&s, b"${ARGC}", argc.as_bytes());
            s = crate::util::replace_all(&s, b"${ARGN}", &argn);
            s = crate::util::replace_all(&s, b"${ARGV}", &argv);
            if contains_seq(&s, b"${ARGV") {
                for (pat, value) in argvs.iter().zip(values.iter()) {
                    s = crate::util::replace_all(&s, pat, value);
                }
            }
            BString::from(s)
        };
        let cmds: Vec<CommandInvocation> = u
            .body
            .cmds
            .iter()
            .map(|c| {
                let mut c = c.clone();
                for arg in &mut c.args {
                    if arg.kind != ArgKind::Bracket {
                        arg.text = substitute(&arg.text);
                    }
                }
                c
            })
            .collect();
        let body = u.body.with_cmds(cmds);

        self.push_call_scope(u, false);
        let result = self.run_code(&body, 0, body.len(), BodyMode::Macro);
        let report = result.is_ok() && !self.fatal;
        self.pop_call_scope(false, report);
        result
    }
}

fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Backtrace as CMake prints it: the top frame, then only command frames. File frames
/// (`FrameKind::ModuleInclude`) locate file-level diagnostics (`in <file>:`) but do not
/// appear in the `Call Stack` list.
pub(crate) fn display_backtrace(bt: &Backtrace) -> Backtrace {
    let mut frames = bt.frames();
    let Some(top) = frames.next() else {
        return Backtrace::default();
    };
    let callers: Vec<&Frame> = frames
        .filter(|f| f.kind != FrameKind::ModuleInclude)
        .collect();
    if callers.len() + 1 == bt.frames().count() {
        return bt.clone();
    }
    let mut out = Backtrace::default();
    for frame in callers.into_iter().rev() {
        out = out.push(frame.clone());
    }
    out.push(top.clone())
}
