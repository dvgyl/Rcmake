use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{
    Backtrace, DiagCategory, DiagState, Effect, Frame, FrameKind, MessageKind, PolicyId, PolicyMap,
    PolicyStatus,
};
use rcmake_regex::Captures;

use crate::hooks::WatchAccess;
use crate::util::{cat, filename_path};
use crate::{CmdError, Interp, Snapshot};

/// Saved list-file variables (`FileScopeBase`).
pub(super) struct ListFileVars {
    old_parent: Option<BString>,
    old_current: BString,
}

impl Interp {
    /// Normal binding value, else cache value (`GetDefinition`).
    pub(crate) fn lookup_var(&self, name: &[u8]) -> Option<&BString> {
        self.vars
            .get(name)
            .or_else(|| self.project.cache.get(name).map(|e| &e.value))
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

    pub(super) fn push_list_file_vars(&mut self, new_current: &[u8]) -> ListFileVars {
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

    pub(super) fn pop_list_file_vars(&mut self, saved: ListFileVars) {
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

    pub(super) fn snapshot_bases(&self) -> Snapshot {
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
}
