//! Debugger hook for `cmake --debugger` (the CMake 4.5.1 Debug Adapter Protocol server).
//!
//! The interpreter calls a [`DebugHook`] at the points where CMake calls its
//! `cmDebuggerAdapter`:
//!
//! - [`DebugHook::begin_parse`] and [`DebugHook::end_parse`] around the parse of a
//!   physical listfile. The adapter shows a temporary frame at line `0` for the file.
//! - [`DebugHook::file_parsed`] after a successful parse, with the line span of each
//!   command. `cmake_language(EVAL CODE)` sources call it too, but without a parse frame.
//! - [`DebugHook::begin_command`] and [`DebugHook::end_command`] around each command
//!   frame (`CallScope`). The adapter can stop in `begin_command`.
//! - [`DebugHook::message`] after a diagnostic or a `message(NOTICE)` is shown.
//!
//! The hook is `None` by default. Each call site checks one `bool` field and calls a cold
//! out-of-line function only when a debugger is attached. The interpreter takes the hook
//! out of its slot for the duration of a call and passes a read-only [`DebugView`], so
//! the hook can inspect the interpreter without a borrow conflict.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_diag::{MessageKind, PolicyMap};
use rcmake_model::{CacheEntry, DirId, Directory, Target, TestEntry};
use rcmake_syntax::Argument;

use crate::Interp;

/// Line span of one command in a parsed listfile (`cmListFileFunction` `Line()` and
/// `LineEnd()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpan {
    /// Line of the command name.
    pub start: u32,
    /// Line of the closing `)`.
    pub end: u32,
}

/// Message class that the debugger can stop on (`cmDebuggerExceptionManager` filters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugMessageKind {
    /// `FATAL_ERROR`: a `CMake Error` diagnostic.
    FatalError,
    /// `INTERNAL_ERROR`: a `CMake Internal Error` diagnostic.
    InternalError,
    /// `MESSAGE`: `message(NOTICE)` or `message()` without a mode.
    Message,
    /// `WARNING`: a `CMake Warning` diagnostic.
    Warning,
    /// `LOG`: a log diagnostic.
    Log,
}

impl DebugMessageKind {
    /// Map a diagnostic kind to the debugger message class, if the debugger has one.
    #[must_use]
    pub const fn from_diagnostic(kind: MessageKind) -> Option<Self> {
        match kind {
            MessageKind::Error => Some(Self::FatalError),
            MessageKind::InternalError => Some(Self::InternalError),
            MessageKind::Warning => Some(Self::Warning),
            MessageKind::Log => Some(Self::Log),
            _ => None,
        }
    }
}

/// One command call that enters the debugger frame stack.
#[derive(Clone, Copy, Debug)]
pub struct CallInfo<'a> {
    /// Listfile (or virtual `EVAL` source) that contains the command.
    pub file: &'a [u8],
    /// Line of the command. Line `0` is a frame that never stops.
    pub line: u32,
    /// Command name as written.
    pub name: &'a [u8],
    /// Command arguments: raw for a listfile command, the expanded values for a
    /// `cmake_language(CALL)` call.
    pub args: &'a [BString],
    /// Directory (makefile) that owns the frame.
    pub directory: DirId,
}

/// Receiver of debugger events from an [`Interp`].
///
/// Each method runs on the evaluator thread. A method can block (for example, at a
/// breakpoint) until the debugger client resumes execution.
pub trait DebugHook: Send + Sync {
    /// A listfile or virtual source parsed successfully with these command spans.
    fn file_parsed(&mut self, path: &[u8], spans: &[CommandSpan]);
    /// The parse of the physical listfile `path` starts.
    fn begin_parse(&mut self, path: &[u8], directory: DirId);
    /// The parse that [`Self::begin_parse`] started has ended.
    fn end_parse(&mut self);
    /// A command frame starts. The hook can stop here.
    fn begin_command(&mut self, view: &DebugView<'_>, call: &CallInfo<'_>);
    /// The innermost command frame ends.
    fn end_command(&mut self);
    /// A message of a debugger message class was shown. The hook can stop here.
    fn message(&mut self, view: &DebugView<'_>, kind: DebugMessageKind, text: &[u8]);
}

/// Read-only view of an [`Interp`] while a [`DebugHook`] method runs.
///
/// The view reads variables without variable watches, read tracking, or other command
/// callbacks.
pub struct DebugView<'a> {
    interp: &'a Interp,
}

impl<'a> DebugView<'a> {
    /// Directory (makefile) of the command being executed.
    #[must_use]
    pub fn current_directory(&self) -> DirId {
        self.interp.current_dir_id()
    }

    /// Directory record of `dir` (the root for an unknown id).
    #[must_use]
    pub fn directory(&self, dir: DirId) -> &'a Directory {
        self.interp.project.dir(dir)
    }

    /// Normal variables visible in the live scope of directory `dir`, sorted by name
    /// (`cmStateSnapshot::ClosureKeys`).
    #[must_use]
    pub fn variables(&self, dir: DirId) -> BTreeMap<&'a BString, &'a BString> {
        self.interp
            .vars
            .visible_in(self.interp.vars.directory_extent(dir))
    }

    /// Normal variable `name` in the live scope of directory `dir`.
    #[must_use]
    pub fn variable(&self, dir: DirId, name: &[u8]) -> Option<&'a [u8]> {
        let extent = self.interp.vars.directory_extent(dir);
        self.interp
            .vars
            .get_in(extent, name)
            .map(|value| value.as_slice())
    }

    /// Normal variable `name` in directory `dir`, else the cache entry value
    /// (`cmMakefile::GetDefinition`).
    #[must_use]
    pub fn definition(&self, dir: DirId, name: &[u8]) -> Option<&'a [u8]> {
        self.variable(dir, name).or_else(|| {
            self.interp
                .project
                .cache
                .get(name)
                .map(|entry| entry.value.as_slice())
        })
    }

    /// Cache entries in insertion order.
    pub fn cache(&self) -> impl Iterator<Item = (&'a BString, &'a CacheEntry)> {
        self.interp.project.cache.iter()
    }

    /// Targets created in directory `dir`, in creation order.
    pub fn targets(&self, dir: DirId) -> impl Iterator<Item = &'a Target> {
        let project = &self.interp.project;
        self.directory(dir)
            .targets
            .iter()
            .filter_map(move |id| project.target(*id))
    }

    /// Tests created in directory `dir`, in creation order.
    pub fn tests(&self, dir: DirId) -> impl Iterator<Item = &'a TestEntry> {
        let project = &self.interp.project;
        self.directory(dir)
            .tests
            .values()
            .filter_map(move |id| project.test(*id))
    }

    /// Current policy settings.
    #[must_use]
    pub fn policies(&self) -> &'a PolicyMap {
        self.interp.policies.top()
    }
}

impl Interp {
    /// Attach (`Some`) or detach (`None`) a debugger hook.
    ///
    /// An attached debugger also turns off incremental configure replay, so that every
    /// command runs and can hit a breakpoint.
    pub fn set_debug_hook(&mut self, hook: Option<Box<dyn DebugHook>>) {
        self.debug_attached = hook.is_some();
        self.debugger = hook;
    }

    /// Detach the debugger hook and return it, for example to pass it to another
    /// interpreter of the same top-level invocation.
    pub fn take_debug_hook(&mut self) -> Option<Box<dyn DebugHook>> {
        self.debug_attached = false;
        self.debugger.take()
    }

    /// Return whether a debugger hook is attached.
    ///
    /// This stays `true` while a hook method runs and the hook is out of its slot.
    #[must_use]
    pub const fn has_debugger(&self) -> bool {
        self.debug_attached
    }

    /// Run `f` with the hook out of its slot and a view of this interpreter.
    fn with_debug_hook(&mut self, f: impl FnOnce(&mut dyn DebugHook, &DebugView<'_>)) {
        let Some(mut hook) = self.debugger.take() else {
            return;
        };
        f(hook.as_mut(), &DebugView { interp: self });
        self.debugger = Some(hook);
    }

    /// Record the raw arguments of the listfile command that starts the next frame.
    #[cold]
    #[inline(never)]
    pub(crate) fn debug_note_raw_args(&mut self, args: &[Argument]) {
        self.debug_call_args = Some(args.iter().map(|arg| arg.text.clone()).collect());
    }

    /// Record the expanded arguments of a `cmake_language(CALL)` call.
    #[cold]
    #[inline(never)]
    pub(crate) fn debug_note_values(&mut self, args: impl Iterator<Item = BString>) {
        self.debug_call_args = Some(args.collect());
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn debug_begin_command(&mut self, file: &[u8], line: u32, name: &[u8]) {
        let args = self.debug_call_args.take().unwrap_or_default();
        let directory = self.current_dir_id();
        self.with_debug_hook(|hook, view| {
            hook.begin_command(
                view,
                &CallInfo {
                    file,
                    line,
                    name,
                    args: &args,
                    directory,
                },
            );
        });
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn debug_end_command(&mut self) {
        if let Some(hook) = self.debugger.as_mut() {
            hook.end_command();
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn debug_begin_parse(&mut self, path: &[u8]) {
        let directory = self.current_dir_id();
        if let Some(hook) = self.debugger.as_mut() {
            hook.begin_parse(path, directory);
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn debug_end_parse(&mut self) {
        if let Some(hook) = self.debugger.as_mut() {
            hook.end_parse();
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn debug_file_parsed(&mut self, path: &[u8], code: &crate::code::Code) {
        let spans: Vec<CommandSpan> = code
            .cmds
            .iter()
            .map(|cmd| CommandSpan {
                start: cmd.line,
                end: cmd.close_line.max(cmd.line),
            })
            .collect();
        if let Some(hook) = self.debugger.as_mut() {
            hook.file_parsed(path, &spans);
        }
    }

    /// Report a shown message to the debugger.
    #[cold]
    #[inline(never)]
    pub(crate) fn debug_message(&mut self, kind: DebugMessageKind, text: &[u8]) {
        self.with_debug_hook(|hook, view| hook.message(view, kind, text));
    }
}
