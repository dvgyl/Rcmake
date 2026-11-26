use rcmake_base::BString;
use rcmake_model::DirId;
use rcmake_syntax::Argument;

use super::Cx;

impl Cx<'_> {
    /// Return true while this directory runs its deferred calls.
    #[must_use]
    pub fn running_deferred(&self) -> bool {
        self.interp.hooks.running_deferred
    }

    /// `variable_watch(<var> [<command>])`: add a watch. Every call adds a new watch,
    /// also for the same variable and command.
    ///
    /// On each access to `var`, the watch calls `command` with the arguments
    /// `<var> <access> <value> <CMAKE_CURRENT_LIST_FILE> <LISTFILE_STACK>` (see
    /// [`WatchAccess`](crate::WatchAccess)).
    /// Without a command (`None` or empty), the watch prints
    /// `Variable "<var>" was accessed using <access> with value "<value>".` as a log
    /// message. A callback does not run again while it is running. `CMP0219` applies to
    /// the callback arguments. The command checks `CMAKE_CURRENT_LIST_FILE` itself.
    pub fn add_variable_watch(&mut self, var: &[u8], command: Option<&[u8]>) {
        self.interp.add_watch(var, command.unwrap_or_default());
    }

    /// Callback commands of the watches of `var` in registration order. An empty entry
    /// is a watch without a command.
    #[must_use]
    pub fn variable_watches(&self, var: &[u8]) -> Vec<BString> {
        self.interp.watch_commands(var)
    }

    /// `true` while a directory listfile runs in configure mode, so that deferred calls
    /// can be queued for the end of the directory.
    #[must_use]
    pub fn defer_active(&self) -> bool {
        self.interp.dir_frame().defer.is_some()
    }

    /// A new unique deferred-call identifier (`__0`, `__1`, ...).
    pub fn defer_new_id(&mut self) -> BString {
        self.interp.new_defer_id()
    }

    /// Queue `command` with raw `args` (see [`Self::raw_args`]) for the end of the
    /// selected directory. The arguments are expanded when the call runs. The call keeps
    /// the file and line of the current command, and runs with the policies that are
    /// active at the end of the directory. Returns `false` when no queue is active
    /// (`DEFER CALL may not be scheduled in directory ... at this time.`).
    pub fn defer_call(
        &mut self,
        dir: DirId,
        id: BString,
        command: &[u8],
        args: &[Argument],
    ) -> bool {
        let queued = self.interp.queue_deferred(dir, id, command, args);
        if queued {
            self.interp.note_replay_defer(dir, command, args);
        }
        queued
    }

    /// Identifiers of the queued calls that are not canceled, in queue order. `None`
    /// when no queue is active.
    #[must_use]
    pub fn defer_get_call_ids(&self, dir: DirId) -> Option<Vec<BString>> {
        self.interp.deferred_ids(dir)
    }

    /// `[command, args...]` of the queued call `id` (raw argument text), or an empty
    /// list if `id` is unknown. `None` when no queue is active.
    #[must_use]
    pub fn defer_get_call(&self, dir: DirId, id: &[u8]) -> Option<Vec<BString>> {
        self.interp.deferred_call(dir, id)
    }

    /// Cancel every queued call with identifier `id`. Returns `false` when no queue is
    /// active.
    pub fn defer_cancel_call(&mut self, dir: DirId, id: &[u8]) -> bool {
        self.interp.cancel_deferred(dir, id)
    }
}
