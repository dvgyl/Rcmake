use rcmake_eval::IncludeOptions;

use std::collections::BTreeSet;
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_commands::ConfigureRuntime;
use rcmake_eval::{Interp, RunMode, Services};
use rcmake_host::{FileKind, Follow, Host};
use rcmake_json::Value;

use crate::options::{self, CacheEdit};
use crate::{emit_cli_error, new_interp_with_paths};

use super::{
    ConfigureSession, diagnostics, initialize_diagnostics, is_file, remove_entries,
    set_directory_vars, unused_cli,
};

#[derive(Default)]
pub(super) struct Preloads {
    tracked: BTreeSet<BString>,
    pub(super) results: Vec<Value>,
    pub(super) exit_code: i32,
    pub(super) fatal: bool,
    pub(super) missing: bool,
}

impl ConfigureSession<'_> {
    pub(super) fn apply_cache_edits(
        &mut self,
        runtime: &ConfigureRuntime,
        interp: &mut Interp,
    ) -> Preloads {
        let evaluated = runtime.host();
        let host = self.host.as_ref();
        let mut preloads = Preloads::default();
        let mut configured_diagnostics = false;
        for edit in std::mem::take(&mut self.opts.cache_edits) {
            match edit {
                CacheEdit::Define(def) => {
                    let name = def.name.clone();
                    diagnostics::warn_legacy_define(host, &name);
                    if unused_cli::apply_define(interp.cx().cache(), def)
                        && preloads.tracked.insert(name.clone())
                    {
                        interp.track_variable_reads([name]);
                    }
                }
                CacheEdit::Remove(pattern) => {
                    remove_entries(interp, &pattern, &mut preloads.tracked)
                }
                CacheEdit::Preload(script) => {
                    if !configured_diagnostics {
                        self.apply_diagnostics(interp);
                        configured_diagnostics = true;
                    }
                    let path = host.initial_cwd().join(&script);
                    if is_preload_file(evaluated.as_ref(), &path) {
                        self.run_preload(runtime, interp, &script, true, &mut preloads);
                    } else if !runtime.denied() {
                        options::out(
                            host,
                            &format!("loading initial cache file {}\n", script.display()),
                        );
                        emit_cli_error(host, format!("Not a file: {}", path.display()));
                        if self.opts.view || is_file(host, &self.src.join("CMakeLists.txt")) {
                            emit_cli_error(
                                host,
                                format!("Error processing file: {}", path.display()),
                            );
                        }
                        preloads.missing = true;
                    }
                }
            }
            if preloads.fatal || runtime.denied() {
                break;
            }
        }
        if !configured_diagnostics {
            self.apply_diagnostics(interp);
        }
        if !self.preset.log.is_empty() {
            options::out(host, &self.preset.log);
        }
        for (index, dir) in [&self.src, &self.bin].into_iter().enumerate() {
            if runtime.denied() || (index == 1 && self.src == self.bin) {
                break;
            }
            let file = dir.join("PreLoad.cmake");
            if is_file(evaluated.as_ref(), &file) {
                self.run_preload(runtime, interp, &file, false, &mut preloads);
            }
        }
        preloads
    }

    fn run_preload(
        &self,
        runtime: &ConfigureRuntime,
        configure: &mut Interp,
        script: &Path,
        initial_cache: bool,
        preloads: &mut Preloads,
    ) {
        let (command, root) =
            rcmake_host::discover_cmake_root(self.host.as_ref(), self.argv.first());
        let mut interp = new_interp_with_paths(
            &self.opts,
            RunMode::Configure,
            runtime.host(),
            command,
            root.unwrap_or_default(),
            self.argv,
            Services::default(),
        );
        runtime.attach(&mut interp);
        self.profiling.attach(&mut interp);
        if let Some(options) = configure.diag_engine().rich_options() {
            interp.set_rich_diagnostics(options);
        }
        *interp.cx().diag_state_mut() = *configure.cx().diag_state();
        *interp.cx().cache() = std::mem::take(configure.cx().cache());
        set_directory_vars(&mut interp, &self.src, &self.bin);
        let pending = configure
            .unread_variables()
            .map(BString::from)
            .collect::<BTreeSet<_>>();
        interp.track_variable_reads(pending.iter().cloned());
        self.profiling
            .measure_listfile(self.host.as_ref(), script, || {
                if initial_cache {
                    let _ = interp.run_preload_script(script);
                } else {
                    let _ = interp.cx().include_file(script, IncludeOptions::default());
                }
            });
        let unread = interp
            .unread_variables()
            .map(BString::from)
            .collect::<BTreeSet<_>>();
        configure.untrack_variable_reads(pending.difference(&unread).cloned());
        *configure.cx().cache() = std::mem::take(interp.cx().cache());
        let name = b"_RCMAKE_FILE_API_PROJECT_QUERIES";
        let mut preload = interp.cx();
        if let Some(queries) = preload.global().props.get(name) {
            let mut project = configure.cx();
            let props = &mut project.global().props;
            let mut merged = BTreeSet::new();
            for value in props.get(name).into_iter().chain([queries]) {
                merged.extend(list::split(value, list::Empty::Drop));
            }
            props.set(name.as_slice().into(), list::join(merged));
        }
        let code = interp.exit_code();
        if code != 0 {
            preloads.exit_code = code;
        }
        preloads.fatal |= interp.fatal_occurred();
        preloads.results.extend(crate::sarif::take_results(&interp));
    }

    fn apply_diagnostics(&self, interp: &mut Interp) {
        let state = initialize_diagnostics(
            self.host.as_ref(),
            interp.cx().cache(),
            &self.preset.warnings,
            &self.opts.warning_flags,
            true,
        );
        *interp.cx().diag_state_mut() = state;
    }
}

fn is_preload_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes).is_ok_and(|meta| {
        meta.kind == FileKind::File
            || (meta.kind == FileKind::Other
                && path.parent().is_some_and(|dir| {
                    dir == Path::new("/dev/fd") || dir == Path::new("/proc/self/fd")
                }))
    })
}
