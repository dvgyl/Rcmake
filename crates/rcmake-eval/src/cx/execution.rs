use std::{path::Path, sync::Arc};

use rcmake_base::BString;
use rcmake_expr::{CondError, ExpandError, ExpandedArg};
use rcmake_host::{Follow, WriteMode};
use rcmake_model::DirId;
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::Argument;

use super::Cx;
use crate::{CmdError, CmdResult, IncludeOptions, SubdirOptions};

impl Cx<'_> {
    /// Process a subdirectory (`add_subdirectory()`): create the directory in the
    /// [`ProjectModel`](rcmake_model::ProjectModel) and run `<src>/CMakeLists.txt` with `<bin>` as its binary
    /// directory. Relative paths resolve against the current source and binary
    /// directories.
    ///
    /// The child starts with a copy of the current variables, so `set(... PARENT_SCOPE)`
    /// and `return(PROPAGATE)` in the child update the caller. Policy, diagnostic, and
    /// loop-block scopes are isolated. The child's deferred calls run before this call
    /// returns, and [`rcmake_model::Directory::final_policies`] records the child's
    /// policies at its end.
    ///
    /// Issues the error `The source directory ... does not contain a CMakeLists.txt
    /// file.` and returns `Err(CmdError::Reported)` when the listfile is missing.
    /// Processing continues after that error, as in CMake.
    pub fn run_subdirectory(
        &mut self,
        src: &Path,
        bin: &Path,
        options: SubdirOptions,
    ) -> Result<DirId, CmdError> {
        self.interp.run_subdirectory(src, bin, options)
    }

    /// Compile or retrieve a cached regex.
    pub fn regex(&mut self, pat: &[u8]) -> Result<Arc<Regex>, RegexError> {
        self.interp.regex_cache.get_or_compile(pat)
    }

    /// Update `CMAKE_MATCH_0..9` and `CMAKE_MATCH_COUNT`. `None` clears previous matches.
    pub fn set_match_vars(&mut self, caps: Option<(&Captures, &[u8])>) {
        self.interp.store_matches(caps);
    }

    /// Evaluate an `if()` condition in the current context.
    pub fn eval_condition(&mut self, args: &[ExpandedArg]) -> Result<bool, CondError> {
        let cond = self.interp.frontend.cond;
        cond(args, self.interp)
    }

    /// Expand variable references and escapes in `src` without list splitting.
    pub fn expand_string(&mut self, src: &[u8]) -> Result<BString, ExpandError> {
        let expand = self.interp.frontend.expand_string;
        expand(src, self.interp)
    }

    /// Invoke another command by name with already expanded arguments (used by `list()`
    /// callbacks and `cmake_language(CALL)`).
    pub fn call_command(&mut self, name: &[u8], args: Vec<ExpandedArg>) -> CmdResult {
        self.interp.call_command(name, args)
    }

    /// Invoke a command by name with already expanded arguments inside the current
    /// backtrace frame (used for generation-time rule configurators).
    ///
    /// Preserves step accounting, recursion checks, cancellation handling, and
    /// command status error reporting without pushing a synthetic caller frame
    /// for `name`.
    pub fn call_command_in_current_frame(
        &mut self,
        name: &[u8],
        args: Vec<ExpandedArg>,
    ) -> CmdResult {
        self.interp.call_command_in_current_frame(name, args)
    }

    /// Include and execute a CMake file (`include()` without module lookup).
    ///
    /// Returns `Err(CmdError::Status)` with CMake's text when the file is missing or can
    /// not be read, unless `options.optional` is set.
    pub fn include_file(&mut self, path: &Path, options: IncludeOptions) -> CmdResult {
        self.interp.include_file(path, options)
    }

    /// Run a callback in a temporary normal variable scope.
    pub fn with_variable_scope<T>(&mut self, run: impl FnOnce(&mut Self) -> T) -> T {
        self.interp.vars.push();
        let result = run(self);
        self.interp.vars.pop();
        result
    }

    /// Run a callback with a configured directory's definitions and policies.
    pub fn with_directory_scope<T>(&mut self, dir: DirId, run: impl FnOnce(&mut Self) -> T) -> T {
        let directory = self.interp.project.dir(dir);
        let definitions = directory.vars.clone();
        let policies = directory.final_policies;
        let lists = self.interp.directory_list_file(&directory.source_dir);
        self.interp.dir_stack.push(crate::DirFrame {
            id: dir,
            trace_base: self.interp.depth,
            defer: None,
        });
        let saved_bt = core::mem::replace(
            &mut self.interp.bt,
            rcmake_diag::Backtrace::default().push(rcmake_diag::Frame {
                file: Arc::from(lists.as_slice()),
                line: 0,
                name: None,
                kind: rcmake_diag::FrameKind::Command,
            }),
        );
        let policy_base = self.interp.policies.len();
        self.interp.policies.push_copy();
        if let Some(policies) = policies {
            *self.interp.policies.top_mut() = policies;
        }
        let result = self.with_variable_scope(|cx| {
            for (key, value) in definitions.iter() {
                if !cx.interp.store_var(key, value.clone()) {
                    break;
                }
            }
            run(cx)
        });
        self.interp.bt = saved_bt;
        self.interp.policies.pop_above(policy_base);
        self.interp.dir_stack.pop();
        result
    }

    /// Read one package module or configuration file with its own unwind scope.
    pub fn include_package_file(&mut self, path: &Path, options: IncludeOptions) -> CmdResult {
        let limit = (self.recursion_limit() / 2).max(100) as usize;
        if self.interp.package_file_depth >= limit {
            self.interp.fatal_here(
                format!("find_package maximum nesting depth of {limit} exceeded.").into(),
            );
            return Err(CmdError::Reported);
        }
        let saved = core::mem::take(&mut self.interp.unwinding_package);
        let prefix = (self.interp.package_file_depth != 0)
            .then(|| self.var_normal_only(b"PACKAGE_PREFIX_DIR"));
        self.interp.package_file_depth += 1;
        let result = self.include_file(path, options);
        self.interp.package_file_depth -= 1;
        self.interp.unwinding_package = saved;
        if let Some(prefix) = prefix {
            if let Some(value) = prefix {
                self.set_var(b"PACKAGE_PREFIX_DIR", value);
            } else {
                self.unset_var(b"PACKAGE_PREFIX_DIR");
            }
        }
        result
    }

    /// Validate the lookup context for `find_package()`.
    pub fn check_package_find(&self, unwind: bool) -> Result<(), CmdError> {
        if self.interp.unwinding_package {
            return Err(CmdError::status("called while already in an UNWIND state"));
        }
        if unwind && self.interp.package_file_depth == 0 {
            return Err(CmdError::status(
                "called with UNWIND_INCLUDE in an invalid context",
            ));
        }
        Ok(())
    }

    /// Start returning at include boundaries after a failed dependency lookup.
    pub fn unwind_package(&mut self) {
        self.interp.unwinding_package = self.interp.package_file_depth != 0;
    }

    /// Parse and execute virtual CMake code (`cmake_language(EVAL CODE ...)`).
    pub fn eval_code(&mut self, code: &[u8]) -> CmdResult {
        self.interp.eval_code(code)
    }

    /// Raw (unexpanded) arguments of the listfile command being executed. Empty for a
    /// command run through [`Self::call_command`].
    ///
    /// Commands that delay expansion (`cmake_language(DEFER CALL)`) use this together
    /// with [`Self::expand_raw_args`].
    #[must_use]
    pub fn raw_args(&self) -> &[Argument] {
        self.interp
            .cur_cmd
            .as_ref()
            .and_then(|(cmds, i)| cmds.get(*i))
            .map_or(&[], |c| c.args.as_slice())
    }

    /// Expand raw arguments as command arguments are expanded. Reports expansion errors
    /// and returns `Err(CmdError::Reported)` after an error.
    pub fn expand_raw_args(&mut self, args: &[Argument]) -> Result<Vec<ExpandedArg>, CmdError> {
        self.interp.expand(args)
    }

    /// Write a cross-compiled probe result block.
    /// Replace the old file with the header and block on the first write.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exceeds the size limit or the host cannot write it.
    pub fn write_try_run_result(
        &mut self,
        path: &Path,
        header: &[u8],
        block: &[u8],
    ) -> Result<(), CmdError> {
        let started = self.interp.try_run_results_started;
        let previous = if started {
            self.host()
                .metadata(path, Follow::Yes)
                .map_err(|error| CmdError::status(error.to_string()))?
                .len
        } else {
            header.len() as u64
        };
        if previous.saturating_add(block.len() as u64) > self.limits().max_file_read {
            return Err(CmdError::status(
                "TryRunResults.cmake exceeds the size limit.",
            ));
        }
        let result = if started {
            self.host().write(path, block, WriteMode::Append)
        } else {
            self.host()
                .write(path, &[header, block].concat(), WriteMode::Truncate)
        };
        result.map_err(|error| CmdError::status(error.to_string()))?;
        self.interp.try_run_results_started = true;
        Ok(())
    }
}
