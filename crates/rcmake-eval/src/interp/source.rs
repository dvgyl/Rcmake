use std::path::Path;
use std::sync::Arc;

use rcmake_diag::{Effect, Frame, FrameKind, Location, MessageKind, source::SourceKind};
use rcmake_host::{FileKind, Follow, ReadRange};
use rcmake_syntax::ParseError;

use super::BodyMode;
use crate::code::{Code, LimitsKey, MAX_CACHE_FILE_BYTES, ParsedSource, SourceText};
use crate::digest::JournalDigest;
use crate::interp::IssueClass;
use crate::util::{cat, collapse_full_path};
use crate::{CmdError, CmdResult, Flow, IncludeOptions, Interp};

impl Interp {
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

    pub(crate) fn parse_source(
        &mut self,
        path: &[u8],
        data: &[u8],
        kind: SourceKind,
    ) -> Option<Code> {
        let cache_key =
            (self.parse_cache.is_some() && data.len() <= MAX_CACHE_FILE_BYTES).then(|| {
                (
                    JournalDigest::hash(data),
                    LimitsKey::new(kind, &self.config.limits),
                )
            });
        let hit = cache_key.and_then(|key| self.parse_cache.as_ref()?.get(key, data));
        let digest = hit
            .as_ref()
            .map(|(text, _)| text.digest)
            .or(cache_key.map(|(digest, _)| digest));
        if kind == SourceKind::File {
            self.note_replay_source(path, data, digest);
        }
        let text = hit
            .as_ref()
            .map_or_else(|| Arc::from(data), |(text, _)| text.bytes.clone());
        let id = self.sources.add(path.into(), kind, text.clone())?;
        if let Some(shared) = self.session_data::<crate::hermetic::HermeticSources>() {
            shared.add(path.into(), kind, Arc::clone(&text));
        }
        let file: Arc<[u8]> = Arc::from(path);
        let ParsedSource { code, warnings } = if let Some((_, parsed)) = hit {
            parsed
        } else {
            let parsed = ParsedSource::parse(data, id, kind, &self.config.limits);
            if let (Some(cache), Some((digest, limits))) = (&self.parse_cache, cache_key) {
                cache.insert(
                    limits,
                    SourceText {
                        bytes: text,
                        digest,
                    },
                    parsed.clone(),
                );
            }
            parsed
        };
        for mut warning in warnings.iter().cloned() {
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
        match code {
            Ok(mut code) => {
                code.file = file;
                Some(code)
            }
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
            IssueClass {
                kind: MessageKind::Error,
                effect,
                category: None,
                policy: None,
            },
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
                let r = self.run_listfile_body(&code, abs, false);
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
        // `cmMakefile::GetBacktrace().Top()` names the file that holds the call. Inside a
        // function this differs from `CMAKE_CURRENT_LIST_FILE` (the caller's file).
        let (current, line) = match self.bt.top() {
            Some(frame) => (frame.file.to_vec(), frame.line),
            None => (
                self.lookup_var(b"CMAKE_CURRENT_LIST_FILE")
                    .map(|value| value.to_vec())
                    .unwrap_or_default(),
                self.cur_line,
            ),
        };
        let name = cat(&[&current, b":", line.to_string().as_bytes(), b":EVAL"]);
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
}
