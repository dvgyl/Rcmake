use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_host::{FileKind, Follow, HostResult, Stream, path};
use rcmake_model::{CacheEntry, CacheType, DirId};

use super::BodyMode;
use crate::util::{cat, collapse_full_path};
use crate::{CmdError, DirFrame, Flow, Interp, RunMode, RunOutcome, SubdirOptions};

/// `CMP0198`: `CMAKE_PARENT_LIST_FILE` is not defined in `CMakeLists.txt`.
pub(crate) const CMP0198: PolicyId = PolicyId::new(198);

impl Interp {
    /// Execute a standalone `-P` script at `path`.
    pub fn run_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path, true)
    }

    /// Run a listfile for a caller that reports its own file processing errors.
    pub fn run_embedded_script(&mut self, path: &Path) -> RunOutcome {
        self.run_script_impl(path, false)
    }

    fn run_script_impl(&mut self, path: &Path, report_processing_error: bool) -> RunOutcome {
        self.init_policy_minimum();
        let cwd = rcmake_host::path::from_native_cmake(&self.config.cwd);
        let abs = collapse_full_path(&rcmake_host::path::from_native_cmake(path), &cwd);

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
        let src_native = rcmake_host::path::to_native(&src_abs);
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
            self.vars.set(var, src_abs.clone());
        }
        for var in [b"CMAKE_BINARY_DIR".as_slice(), b"CMAKE_CURRENT_BINARY_DIR"] {
            self.vars.set(var, bin_abs.clone());
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

        let lists_path = src_native.join("CMakeLists.txt");
        let lists_bytes = rcmake_host::path::from_native_cmake(&lists_path);
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

        if let Err(error) = self.reset_package_redirects(&bin_native) {
            self.error_no_location(
                format!(
                    "Unable to (re)create the private pkgRedirects directory:\n  {}\n  {error}",
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
    pub(super) fn record_list_file(&mut self, abs: &[u8]) {
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
            &rcmake_host::path::from_native_cmake(src),
            &self.current_source_dir_bytes(),
        );
        let bin_base = rcmake_host::path::from_native_cmake(&self.current_binary_dir());
        let bin_abs = collapse_full_path(&rcmake_host::path::from_native_cmake(bin), &bin_base);
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

        self.vars.push_directory(id);
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

        let _ = self.run_list_file(&lists, true);
        self.snapshot_dir_vars(id);

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
        let names = self.vars.visible_names();
        let mut snap = indexmap::IndexMap::with_capacity(names.len());
        for name in names {
            if let Some(val) = self.vars.get(&name) {
                snap.insert(name, val.clone());
            }
        }
        self.project.dir_mut(id).vars = snap;
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
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Policy),
                Some(POLICY),
                Location::FileLine {
                    file: file.as_slice().into(),
                    line: 0,
                },
                Self::policy_warning_text(POLICY),
                true,
            );
        }
    }

    pub(super) fn current_source_dir_bytes(&self) -> BString {
        rcmake_host::path::from_native_cmake(&self.current_source_dir())
    }
}
