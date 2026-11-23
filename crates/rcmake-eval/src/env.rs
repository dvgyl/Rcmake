//! [`ExpandEnv`] and [`CondEnv`] implementations so that `rcmake_expr::expand_args` and
//! `rcmake_expr::eval_condition` read interpreter state directly.

use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Diagnostic, Location, PolicyId, PolicyStatus};
use rcmake_expr::{CondEnv, ExpandEnv};
use rcmake_host::Host;
use rcmake_regex::{Captures, Regex, RegexError};

use crate::Interp;
use crate::hooks::WatchAccess;

impl ExpandEnv for Interp {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        if name == b"CMAKE_CURRENT_LIST_LINE" {
            // `--trace-expand` expands without a file context, so the variable is not
            // special there.
            if self.trace_expanding {
                return Some(self.read_var(name).unwrap_or_default());
            }
            if let Some(id) = &self.top_defer_id {
                let mut line = b"DEFERRED:".to_vec();
                line.extend_from_slice(id);
                return Some(BString::from(line));
            }
        }
        self.read_var(name)
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.cache.get(name).map(|e| e.value.clone())
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.config.host.env_get(name)
    }

    fn current_line(&self) -> u32 {
        self.cur_line
    }
}

impl CondEnv for Interp {
    fn is_defined(&mut self, name: &[u8]) -> bool {
        let defined = self.vars.get(name).is_some() || self.cache.contains(name);
        if !defined {
            self.fire_watches(name, WatchAccess::UnknownDefined, None);
        }
        defined
    }

    fn cache_defined(&mut self, name: &[u8]) -> bool {
        self.cache.contains(name)
    }

    fn env_defined(&mut self, name: &[u8]) -> bool {
        self.config.host.env_get(name).is_some()
    }

    fn command_exists(&self, name: &[u8]) -> bool {
        Interp::command_exists(self, name)
    }

    fn policy_known(&self, name: &[u8]) -> bool {
        PolicyId::parse(name).is_some()
    }

    fn diagnostic_known(&self, name: &[u8]) -> bool {
        DiagCategory::parse(name).is_some()
    }

    fn target_exists(&self, name: &[u8]) -> bool {
        self.project.target_names.contains_key(name)
    }

    fn test_exists(&self, name: &[u8]) -> bool {
        self.project.test_names.contains_key(name)
    }

    fn rule_exists(&self, name: &[u8]) -> bool {
        self.project.rule_names.contains_key(name)
    }

    fn policy(&self, id: PolicyId) -> PolicyStatus {
        self.policy_status(id)
    }

    fn host(&self) -> &dyn Host {
        self.config.host.as_ref()
    }

    fn base_dir(&self) -> &Path {
        &self.config.cwd
    }

    fn regex(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError> {
        self.regex_cache.get_or_compile(pattern)
    }

    fn set_match_vars(&mut self, captures: Option<(&Captures, &[u8])>) {
        self.store_matches(captures);
    }

    fn warn(&mut self, mut diag: Diagnostic) {
        if matches!(diag.location, Location::None) {
            diag.location = Location::Backtrace(self.bt.clone());
        }
        let state = *self.diag_top();
        self.emit(&diag, &state);
    }
}
