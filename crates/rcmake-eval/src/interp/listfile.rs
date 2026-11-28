use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_host::{FileKind, Follow, HostResult, Stream, path};
use rcmake_model::{CacheEntry, CacheType, DirId};

use crate::interp::IssueClass;
use crate::util::{cat, collapse_full_path};
use crate::{CmdError, DirFrame, Flow, Interp, RunMode, RunOutcome, SubdirOptions};

/// `CMP0198`: `CMAKE_PARENT_LIST_FILE` is not defined in `CMakeLists.txt`.
pub(crate) const CMP0198: PolicyId = PolicyId::new(198);

impl Interp {
    /// Execute a standalone `-P` script at `path`.
    pub fn run_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path, true, true)
    }

    /// Run a listfile for a caller that reports its own file processing errors.
    pub fn run_embedded_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path, false, true)
    }

    /// Execute the find-package module in a session that permits project commands.
    pub fn run_find_package_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path, true, false)
    }

    fn run_script_impl(
        &mut self,
        path: &Path,
        report_processing_error: bool,
        standalone: bool,
    ) -> RunOutcome {
        self.init_policy_minimum();
        let cwd = rcmake_host::path::from_native_cmake(&self.config.cwd);
        let abs = collapse_full_path(&rcmake_host::path::from_native_cmake(path), &cwd);
        let abs = if self.is_regular_file(&abs) {
            let on_disk = self
                .config
                .host
                .path_on_disk(&rcmake_host::path::to_native(&abs));
            rcmake_host::path::from_native_cmake(&on_disk)
        } else {
            abs
        };

        if standalone {
            self.store_var(b"CMAKE_SCRIPT_MODE_FILE", abs.clone());
            // The script-mode root snapshot has no listfile.
            self.listfiles.push(Arc::from(&b""[..]));
        }
        let argc = self.config.script_args.len();
        self.store_var(b"CMAKE_ARGC", argc.to_string().into());
        for (i, arg) in self.config.script_args.clone().into_iter().enumerate() {
            if !self.store_var(format!("CMAKE_ARGV{i}").as_bytes(), arg) {
                break;
            }
        }

        if !self.is_regular_file(&abs) {
            self.error_no_location(cat(&[b"Not a file: ", &abs]));
        }
        if !self.read_list_file(&abs) && report_processing_error && self.stop_reason().is_none() {
            self.error_no_location(cat(&[b"Error processing file: ", &abs]));
        }
        RunOutcome {
            exit_code: self.exit_code(),
        }
    }

    /// `cmMakefile::ReadListFile`: run a file without a policy scope. Returns `false`
    /// when the file can not be read or parsed.
    pub(crate) fn read_list_file(&mut self, abs: &[u8]) -> bool {
        self.run_list_file(abs, false)
    }

    /// Run a listfile. A directory listfile (`CMakeLists.txt` in configure mode) also
    /// applies `CMP0198` and runs the deferred calls that it queues
    /// (`cmMakefile::Configure` and `RunListFile`).
    fn run_list_file(&mut self, abs: &[u8], directory: bool) -> bool {
        if self.poll_cancel() {
            return false;
        }
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
                if directory {
                    self.inject_project_command(&code, abs);
                }
                let r = self.run_listfile_body(&code, abs, directory);
                if let Ok(Flow::Return {
                    propagate: Some(names),
                }) = &r
                {
                    self.raise_vars(names);
                }
                if directory {
                    self.run_deferred(abs);
                    self.run_legacy_subdirs();
                    self.add_user_configure_depends();
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

    /// `cmMakefile::Configure`: when the top-level `CMakeLists.txt` has no `project()`
    /// call at all, warn and run `project(Project)` before its first command.
    fn inject_project_command(&mut self, code: &crate::code::Code, abs: &[u8]) {
        if !self.inject_project
            || self.current_dir_id() != DirId::ROOT
            || code
                .cmds
                .iter()
                .any(|cmd| cmd.lower.as_slice() == b"project")
        {
            return;
        }
        self.issue_at(
            IssueClass {
                kind: MessageKind::AuthorWarning,
                effect: Effect::None,
                category: Some(DiagCategory::Author),
                policy: None,
            },
            Location::FileLine {
                file: abs.into(),
                line: 0,
            },
            "No project() command is present.  The top-level CMakeLists.txt file must contain a literal, direct call to the project() command.  Add a line of code such as\n  project(ProjectName)\nnear the top of the file, but after cmake_minimum_required().\nCMake is pretending there is a \"project(Project)\" command on the first line.".into(),
            false,
        );
        let injected = b"project(Project __CMAKE_INJECTED_PROJECT_COMMAND__)\n";
        if let Some(injected) =
            self.parse_source(abs, injected, rcmake_diag::source::SourceKind::VirtualEval)
        {
            let _ = self.run_code(&injected, 0, injected.len(), super::BodyMode::File);
        }
    }

    /// Apply the default for `CMP0198` if the policy is unset.
    /// Then set `CMAKE_PARENT_LIST_FILE` for this directory listfile.
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
            let lists = self.directory_list_file(&self.current_source_dir_bytes());
            self.define(b"CMAKE_PARENT_LIST_FILE", lists);
        }
    }

    /// Configure a project with a source directory and a build directory.
    /// Run any `-C` preload scripts before reading `CMakeLists.txt`.
    pub fn run_configure(
        &mut self,
        source_dir: &Path,
        binary_dir: &Path,
        preload_scripts: &[PathBuf],
    ) -> RunOutcome {
        if self.poll_cancel() {
            return RunOutcome {
                exit_code: self.exit_code(),
            };
        }
        let cwd = rcmake_host::path::from_native_cmake(&self.config.cwd);
        let src_abs = collapse_full_path(&rcmake_host::path::from_native_cmake(source_dir), &cwd);
        let bin_abs = collapse_full_path(&rcmake_host::path::from_native_cmake(binary_dir), &cwd);

        let bin_native = rcmake_host::path::to_native(&bin_abs);

        self.init_policy_minimum();
        self.project.set_root_dirs(src_abs.clone(), bin_abs.clone());
        let probe = self
            .project
            .global
            .props
            .get(b"IN_TRY_COMPILE")
            .is_some_and(|value| rcmake_base::truth::is_on(value));
        self.render.relative_base = (!probe).then(|| src_abs.clone());

        for var in [b"CMAKE_SOURCE_DIR".as_slice(), b"CMAKE_CURRENT_SOURCE_DIR"] {
            self.store_var(var, src_abs.clone());
        }
        for var in [b"CMAKE_BINARY_DIR".as_slice(), b"CMAKE_CURRENT_BINARY_DIR"] {
            self.store_var(var, bin_abs.clone());
        }

        let _ = self.config.host.create_dir_all(&bin_native);
        for preload in preload_scripts {
            self.run_preload_script(preload);
            if self.fatal {
                return RunOutcome {
                    exit_code: self.exit_code(),
                };
            }
        }

        let lists_bytes = self.directory_list_file(&src_abs);
        if !self.is_regular_file(&lists_bytes) {
            let name = self.list_file_name.clone();
            self.error_no_location(cat(&[
                b"The source directory \"",
                &src_abs,
                b"\" does not appear to contain ",
                &name,
                b".\nSpecify --help for usage, or press the help button on the CMake GUI.",
            ]));
            return RunOutcome {
                exit_code: self.exit_code(),
            };
        }

        if self.reset_package_redirects(&bin_native).is_err() {
            self.error_no_location(
                format!(
                    "Unable to (re)create the private pkgRedirects directory:\n  {}\nThis may be caused by not having read/write access to the build directory.\nTry specifying a location with read/write access like:\n  cmake -B build\nIf using a CMake presets file, ensure that preset parameter\n'binaryDir' expands to a writable directory.\n",
                    path::from_native_cmake(&bin_native.join("CMakeFiles/pkgRedirects"))
                )
                .into(),
            );
            return RunOutcome {
                exit_code: self.exit_code(),
            };
        }

        let _ = self.run_list_file(&lists_bytes, true);
        self.poll_cancel();
        self.write_configure_log(&bin_native);
        self.snapshot_dir_vars(DirId::ROOT);
        if !probe && self.exit_code() != 0 && self.exit_override.is_none() {
            self.config.host.write_out(
                Stream::Stdout,
                b"-- Configuring incomplete, errors occurred!\n",
            );
        }
        RunOutcome {
            exit_code: self.exit_code(),
        }
    }

    fn reset_package_redirects(&mut self, binary_dir: &Path) -> HostResult<()> {
        let host = self.config.host.as_ref();
        let parent = binary_dir.join("CMakeFiles");
        let redirects = parent.join("pkgRedirects");
        if let Ok(metadata) = host.metadata(&parent, Follow::No)
            && metadata.kind != FileKind::Dir
        {
            return Err(rcmake_host::HostError::from_io(
                std::io::Error::from(ErrorKind::NotADirectory),
                Some(parent),
            ));
        }
        match host.metadata(&redirects, Follow::No) {
            Ok(metadata) if metadata.kind == FileKind::Dir => host.remove_dir_all(&redirects)?,
            Ok(_) => host.remove_file(&redirects)?,
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        host.create_dir_all(&redirects)?;
        self.project.cache.insert(
            "CMAKE_FIND_PACKAGE_REDIRECTS_DIR".into(),
            CacheEntry {
                value: path::from_native_cmake(&redirects),
                ty: CacheType::Static,
                help: "Value Computed by CMake.".into(),
                advanced: false,
                props: Default::default(),
            },
        );
        Ok(())
    }

    /// Execute one `-C` initial cache script in the current session.
    pub fn run_preload_script(&mut self, path: &Path) -> bool {
        if self.poll_cancel() {
            return false;
        }
        self.init_policy_minimum();
        let cwd = rcmake_host::path::from_native_cmake(&self.config.cwd);
        let abs = collapse_full_path(&rcmake_host::path::from_native_cmake(path), &cwd);
        let abs = if self.is_regular_file(&abs) {
            let on_disk = self
                .config
                .host
                .path_on_disk(&rcmake_host::path::to_native(&abs));
            rcmake_host::path::from_native_cmake(&on_disk)
        } else {
            abs
        };
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

    /// Project file of the source directory `dir` (`cmake::GetCMakeListFile`). A
    /// directory without the selected `--project-file` reads `CMakeLists.txt`.
    pub(crate) fn directory_list_file(&self, dir: &[u8]) -> BString {
        let selected = cat(&[dir, b"/", &self.list_file_name]);
        if self.list_file_name.as_slice() == crate::DEFAULT_LIST_FILE_NAME.as_bytes()
            || self.is_regular_file(&selected)
        {
            return selected;
        }
        cat(&[dir, b"/", crate::DEFAULT_LIST_FILE_NAME.as_bytes()])
    }

    /// Record a listfile read in the current directory (`cmMakefile::ListFiles`).
    /// `cmMakefile::AddCMakeDependFilesFromUser`: the `CMAKE_CONFIGURE_DEPENDS` directory
    /// property names more files whose change makes the build system re-run CMake.
    fn add_user_configure_depends(&mut self) {
        let id = self.current_dir_id();
        let Some(value) = self
            .project
            .dir(id)
            .props
            .get(b"CMAKE_CONFIGURE_DEPENDS")
            .cloned()
        else {
            return;
        };
        let base = self.current_source_dir_bytes();
        for dep in rcmake_base::list::split(&value, rcmake_base::list::Empty::Drop) {
            let abs = collapse_full_path(&dep, &base);
            self.record_list_file(&abs);
        }
    }

    /// Run legacy `subdirs()` directories queued during the current directory listfile,
    /// using the parent's end-of-listfile variables, policies, and directory state while
    /// preserving each subdirectory's original `install_position`.
    fn run_legacy_subdirs(&mut self) {
        const LEGACY_SUBDIRS_PROP: &[u8] = b"\0rcmake-legacy-subdirs";
        let id = self.current_dir_id();
        let Some(raw) = self.project.dir(id).props.get(LEGACY_SUBDIRS_PROP).cloned() else {
            return;
        };
        let mut cursor = 0;
        while !self.fatal && self.exit_override.is_none() && !self.over_budget() {
            let Some(tail) = raw.as_slice().get(cursor..) else {
                break;
            };
            let Some(record) = parse_legacy_subdir_record(tail) else {
                break;
            };
            cursor += record.consumed;
            // Release this entry's own reservation right before configuring it while keeping
            // any later queued legacy subdirs reserved against sibling `add_subdirectory` calls.
            match raw.as_slice().get(cursor..) {
                Some(remaining) if !remaining.is_empty() => {
                    self.project
                        .dir_mut(id)
                        .props
                        .set(LEGACY_SUBDIRS_PROP.into(), remaining.into());
                }
                _ => {
                    self.project.dir_mut(id).props.unset(LEGACY_SUBDIRS_PROP);
                }
            }
            if record.src.is_empty() {
                continue;
            }
            if self
                .project
                .dirs()
                .any(|(_, entry)| entry.binary_dir.as_slice() == record.bin)
            {
                self.error_here(cat(&[
                    b"The binary directory\n  ",
                    record.bin,
                    b"\nis already used to build a source directory.  It cannot be used to build source directory\n  ",
                    record.src,
                    b"\nSpecify a unique binary directory name.",
                ]));
                continue;
            }
            let options = SubdirOptions {
                exclude_from_all: record.exclude == b"1",
                system: false,
            };
            let src_native = path::to_native(record.src);
            let bin_native = path::to_native(record.bin);
            if let Ok(child_id) = self.run_subdirectory(&src_native, &bin_native, options)
                && let Ok(pos_str) = core::str::from_utf8(record.install_pos)
                && let Ok(pos) = pos_str.parse::<usize>()
            {
                self.project.dir_mut(child_id).install_position = pos;
            }
        }
        self.project.dir_mut(id).props.unset(LEGACY_SUBDIRS_PROP);
    }

    pub(super) fn record_list_file(&mut self, abs: &[u8]) {
        let id = self.current_dir_id();
        self.project.dir_mut(id).listfiles.push(BString::from(abs));
    }

    /// `cmMakefile::AddSubDirectory` + `ConfigureSubDirectory` (see
    /// [`crate::Cx::run_subdirectory`]).
    pub(crate) fn run_subdirectory(
        &mut self,
        src: &Path,
        bin: &Path,
        options: SubdirOptions,
    ) -> Result<DirId, CmdError> {
        if self.hooks.running_deferred {
            self.error_here("Subdirectories may not be created during deferred execution.".into());
            return Err(CmdError::Reported);
        }
        let src_abs = collapse_full_path(
            &rcmake_host::path::from_native_cmake(src),
            &self.current_source_dir_bytes(),
        );
        let bin_base = rcmake_host::path::from_native_cmake(&self.current_binary_dir());
        let bin_abs = collapse_full_path(&rcmake_host::path::from_native_cmake(bin), &bin_base);
        let lists = self.directory_list_file(&src_abs);
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

        self.push_variable_boundary(Some(id));
        self.loop_blocks.push(0);
        self.policies.push_copy();
        self.diags.push_copy();
        let policy_base = self.policies.len() - 1;
        let diag_base = self.diags.len() - 1;
        self.dir_stack.push(DirFrame {
            id,
            trace_base: self.depth,
            defer: None,
        });
        self.define(b"CMAKE_CURRENT_SOURCE_DIR", src_abs);
        self.define(b"CMAKE_CURRENT_BINARY_DIR", bin_abs);

        // A new directory starts a new backtrace: diagnostics and target backtraces in
        // the subdirectory do not show the parent `add_subdirectory` call.
        let parent_bt = core::mem::take(&mut self.bt);
        let _ = self.run_list_file(&lists, true);
        self.snapshot_dir_vars(id);
        self.bt = parent_bt;

        self.dir_stack.pop();
        self.diags.pop_above(diag_base);
        self.policies.pop_above(policy_base);
        self.loop_blocks.pop();
        self.vars.pop();
        Ok(id)
    }

    /// Capture all visible normal variable definitions onto `Directory::vars` before a
    /// directory scope exits so generate-time resolution and lowering observe normal
    /// compiler rules, flags, and platform variables.
    fn snapshot_dir_vars(&mut self, id: DirId) {
        self.warn_install_order(id);
        self.project.dir_mut(id).vars = self.vars.snapshot();
    }

    fn warn_install_order(&mut self, id: DirId) {
        const POLICY: PolicyId = PolicyId::new(82);
        if self.config.mode != RunMode::Configure
            || self.policy_status(POLICY) != PolicyStatus::Warn
            || !self
                .lookup_var(b"CMAKE_POLICY_WARNING_CMP0082")
                .is_some_and(|value| rcmake_base::truth::is_on(value))
        {
            return;
        }
        let directory = self.project.dir(id);
        let last_rule = self
            .project
            .installs
            .iter()
            .rposition(|rule| rule.dir == id);
        let Some(last_rule) = last_rule else { return };
        let different_order = self.project.installs.iter().any(|rule| {
            if rule.dir == id || self.project.dir_excluded_from_all(rule.dir) {
                return false;
            }
            self.project
                .ancestors(rule.dir)
                .take_while(|dir| *dir != id)
                .last()
                .is_some_and(|child| {
                    self.project.dir(child).parent == Some(id)
                        && self.project.dir(child).install_position <= last_rule
                })
        });
        if different_order {
            let file = cat(&[&directory.source_dir, b"/CMakeLists.txt"]);
            self.issue_at(
                IssueClass::policy(POLICY),
                Location::FileLine {
                    file: file.as_slice().into(),
                    line: 0,
                },
                Self::policy_warning_text(POLICY),
                true,
            );
        }
    }

    pub(crate) fn current_source_dir_bytes(&self) -> BString {
        rcmake_base::path_lex::cmake_style(&self.project.dir(self.current_dir_id()).source_dir)
    }
}

struct LegacySubdirRecord<'a> {
    src: &'a [u8],
    bin: &'a [u8],
    exclude: &'a [u8],
    install_pos: &'a [u8],
    consumed: usize,
}

fn parse_legacy_subdir_record(bytes: &[u8]) -> Option<LegacySubdirRecord<'_>> {
    let mut offset = 0;
    let mut next_field = || {
        let rest = bytes.get(offset..)?;
        let nul = rest.iter().position(|&byte| byte == 0)?;
        let field = rest.get(..nul)?;
        offset += nul + 1;
        Some(field)
    };
    let src = next_field()?;
    let bin = next_field()?;
    let exclude = next_field()?;
    let install_pos = next_field()?;
    Some(LegacySubdirRecord {
        src,
        bin,
        exclude,
        install_pos,
        consumed: offset,
    })
}

#[cfg(test)]
mod path_on_disk_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcmake_host::{Host, PureHost, forward_host};

    use crate::{Interp, InterpConfig, RunMode};

    /// Host wrapper that resolves paths case-insensitively on lookup and returns the
    /// on-disk spelling from `Host::path_on_disk`, modeling macOS/Windows filesystems.
    struct CaseInsensitiveHost {
        base: PureHost,
    }

    impl Host for CaseInsensitiveHost {
        forward_host!(base, env, process, services);

        fn metadata(
            &self,
            p: &Path,
            follow: rcmake_host::Follow,
        ) -> rcmake_host::HostResult<rcmake_host::Metadata> {
            let resolved = self.path_on_disk(p);
            self.base.metadata(&resolved, follow)
        }

        fn read(
            &self,
            p: &Path,
            range: rcmake_host::ReadRange,
        ) -> rcmake_host::HostResult<Vec<u8>> {
            let resolved = self.path_on_disk(p);
            self.base.read(&resolved, range)
        }

        fn write(
            &self,
            p: &Path,
            data: &[u8],
            mode: rcmake_host::WriteMode,
        ) -> rcmake_host::HostResult<()> {
            self.base.write(p, data, mode)
        }

        fn read_dir(&self, p: &Path) -> rcmake_host::HostResult<Vec<rcmake_host::DirEntry>> {
            self.base.read_dir(p)
        }

        fn create_dir_all(&self, p: &Path) -> rcmake_host::HostResult<()> {
            self.base.create_dir_all(p)
        }

        fn remove_file(&self, p: &Path) -> rcmake_host::HostResult<()> {
            self.base.remove_file(p)
        }

        fn remove_dir(&self, p: &Path) -> rcmake_host::HostResult<()> {
            self.base.remove_dir(p)
        }

        fn remove_dir_all(&self, p: &Path) -> rcmake_host::HostResult<()> {
            self.base.remove_dir_all(p)
        }

        fn rename(&self, from: &Path, to: &Path) -> rcmake_host::HostResult<()> {
            self.base.rename(from, to)
        }

        fn copy_file(
            &self,
            from: &Path,
            to: &Path,
            opts: rcmake_host::CopyOptions,
        ) -> rcmake_host::HostResult<()> {
            self.base.copy_file(from, to, opts)
        }

        fn symlink(&self, target: &Path, link: &Path) -> rcmake_host::HostResult<()> {
            self.base.symlink(target, link)
        }

        fn hard_link(&self, src: &Path, dst: &Path) -> rcmake_host::HostResult<()> {
            self.base.hard_link(src, dst)
        }

        fn read_link(&self, p: &Path) -> rcmake_host::HostResult<PathBuf> {
            self.base.read_link(p)
        }

        fn canonicalize(&self, p: &Path) -> rcmake_host::HostResult<PathBuf> {
            self.base.canonicalize(p)
        }

        fn path_on_disk(&self, p: &Path) -> PathBuf {
            rcmake_host::path::path_on_disk_with(p, |dir| self.base.read_dir(dir))
        }

        fn set_permissions(&self, p: &Path, mode: u32) -> rcmake_host::HostResult<()> {
            self.base.set_permissions(p, mode)
        }

        fn set_times(
            &self,
            p: &Path,
            atime: Option<rcmake_host::FileTime>,
            mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()> {
            self.base.set_times(p, atime, mtime)
        }
    }

    #[test]
    fn script_mode_uses_host_path_on_disk_for_script_and_list_file_vars() {
        let base = PureHost::new(PathBuf::from("/Work"), PathBuf::from("/bin/cmake"));
        base.seed_file(Path::new("/Work/P_PathOnDisk.cmake"), b"capture()\n")
            .unwrap();
        let host: Arc<dyn Host> = Arc::new(CaseInsensitiveHost { base });
        let config = InterpConfig::new(host, RunMode::Script, "/share", "/bin/cmake", "/Work");
        let mut interp = Interp::new(config, |reg| {
            reg.add(
                "capture",
                |cx, _| {
                    let script = cx.var_or_empty(b"CMAKE_SCRIPT_MODE_FILE");
                    let list = cx.var_or_empty(b"CMAKE_CURRENT_LIST_FILE");
                    cx.set_var(b"CAPTURED_SCRIPT", script);
                    cx.set_var(b"CAPTURED_LIST", list);
                    Ok(crate::Flow::Normal)
                },
                crate::CmdFlags { scriptable: true },
            );
        });
        let outcome = interp.run_script(Path::new("/work/p_pathondisk.cmake"));
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            interp.cx().var_or_empty(b"CAPTURED_SCRIPT"),
            "/Work/P_PathOnDisk.cmake"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CAPTURED_LIST"),
            "/Work/P_PathOnDisk.cmake"
        );
    }
}
