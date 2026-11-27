//! Cache-entry edit widgets (`BOOL`, `STRINGS`, `STRING`, `PATH`, `FILEPATH`).

use rcmake_base::{BString, truth::is_on};
use rcmake_model::CacheType;

use crate::DirItem;
use crate::keys::Key;

/// Maximum byte length accepted for a single edited cache entry value (64 KiB).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// Toggle a `BOOL` cache value between `"ON"` and `"OFF"`.
#[must_use]
pub fn toggle_bool(current: &[u8]) -> BString {
    if is_on(current) {
        BString::from("OFF")
    } else {
        BString::from("ON")
    }
}

/// Cycle through the `STRINGS` choice list forward (`forward == true`) or backward.
#[must_use]
pub fn cycle_strings(current: &[u8], options: &[BString], forward: bool) -> Option<BString> {
    if options.is_empty() {
        return None;
    }
    let idx = options.iter().position(|opt| opt.as_slice() == current);
    let next_idx = match (idx, forward) {
        (Some(i), true) => (i + 1) % options.len(),
        (Some(0), false) => options.len().saturating_sub(1),
        (Some(i), false) => i.saturating_sub(1),
        (None, true) => 0,
        (None, false) => options.len().saturating_sub(1),
    };
    options.get(next_idx).cloned()
}

/// Outcome of handling one key inside [`TextEditWidget`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Stay in edit mode; no redraw needed.
    None,
    /// Stay in edit mode and redraw the edited field.
    Redraw,
    /// Request directory listing for Tab path completion.
    RequestCompletion {
        /// Directory path bytes to query through `CacheDriver::list_dir`.
        dir: BString,
    },
    /// Accept the edited value and leave edit mode.
    Commit(BString),
    /// Discard edits and leave edit mode.
    Cancel,
    /// User pressed `Ctrl-C`; exit immediately without generating.
    Quit,
}

#[derive(Clone, Debug)]
struct CompletionState {
    dir_prefix: BString,
    candidates: Vec<DirItem>,
    index: usize,
}

/// Line-editor widget for `STRING`, `PATH`, and `FILEPATH` cache entries.
#[derive(Clone, Debug)]
pub struct TextEditWidget {
    ty: CacheType,
    buf: Vec<u8>,
    cursor: usize,
    completion: Option<CompletionState>,
    pending_leaf: BString,
    pending_prefix: BString,
}

impl TextEditWidget {
    /// Construct an editor initialized with `initial` and cursor placed at the end.
    #[must_use]
    pub fn new(ty: CacheType, initial: &[u8]) -> Self {
        let len = initial.len().min(MAX_VALUE_BYTES);
        let buf = initial.get(..len).unwrap_or_default().to_vec();
        let cursor = buf.len();
        Self {
            ty,
            buf,
            cursor,
            completion: None,
            pending_leaf: BString::default(),
            pending_prefix: BString::default(),
        }
    }

    /// Current edited buffer bytes.
    #[must_use]
    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }

    /// Current byte offset of the edit cursor inside [`TextEditWidget::buffer`].
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Handle one keyboard event while editing.
    pub fn handle_key(&mut self, key: Key) -> EditAction {
        match key {
            Key::Ctrl(b'c') => EditAction::Quit,
            Key::Enter => EditAction::Commit(BString::from(self.buf.as_slice())),
            Key::Esc => EditAction::Cancel,
            Key::Left | Key::Ctrl(b'b') => {
                if self.move_left() {
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Right | Key::Ctrl(b'f') => {
                if self.move_right() {
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Home | Key::Ctrl(b'a') => {
                if self.cursor != 0 {
                    self.cursor = 0;
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::End | Key::Ctrl(b'e') => {
                if self.cursor != self.buf.len() {
                    self.cursor = self.buf.len();
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Backspace => {
                self.completion = None;
                if self.delete_prev() {
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Delete | Key::Ctrl(b'd') => {
                self.completion = None;
                if self.delete_at_cursor() {
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Ctrl(b'k') => {
                self.completion = None;
                if self.cursor < self.buf.len() {
                    self.buf.truncate(self.cursor);
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            Key::Tab => self.handle_tab(),
            Key::Char(ch) => {
                self.completion = None;
                if self.insert_char(ch) {
                    EditAction::Redraw
                } else {
                    EditAction::None
                }
            }
            _ => EditAction::None,
        }
    }

    /// Apply directory entries returned by `CacheDriver::list_dir` in response to
    /// [`EditAction::RequestCompletion`].
    pub fn apply_completion(&mut self, items: Vec<DirItem>) {
        let dirs_only = self.ty == CacheType::Path;
        let leaf = self.pending_leaf.as_slice();
        let mut matches: Vec<DirItem> = items
            .into_iter()
            .filter(|item| (!dirs_only || item.is_dir) && item.name.as_slice().starts_with(leaf))
            .collect();
        matches.sort_by(|a, b| a.name.cmp(&b.name));
        if matches.is_empty() {
            self.completion = None;
            return;
        }
        self.completion = Some(CompletionState {
            dir_prefix: self.pending_prefix.clone(),
            candidates: matches,
            index: 0,
        });
        self.apply_current_candidate();
    }

    fn handle_tab(&mut self) -> EditAction {
        if !matches!(self.ty, CacheType::Path | CacheType::Filepath) {
            return EditAction::None;
        }
        if let Some(state) = &mut self.completion {
            if !state.candidates.is_empty() {
                state.index = (state.index + 1) % state.candidates.len();
                self.apply_current_candidate();
                return EditAction::Redraw;
            }
            self.completion = None;
        }
        let (dir_to_query, dir_prefix, leaf) = split_path_for_completion(&self.buf);
        self.pending_prefix = dir_prefix;
        self.pending_leaf = leaf;
        EditAction::RequestCompletion { dir: dir_to_query }
    }

    fn apply_current_candidate(&mut self) {
        let Some(state) = &self.completion else {
            return;
        };
        let Some(item) = state.candidates.get(state.index) else {
            return;
        };
        let mut next = Vec::with_capacity(
            state
                .dir_prefix
                .len()
                .saturating_add(item.name.len())
                .saturating_add(1),
        );
        next.extend_from_slice(state.dir_prefix.as_slice());
        next.extend_from_slice(item.name.as_slice());
        if item.is_dir {
            next.push(b'/');
        }
        if next.len() <= MAX_VALUE_BYTES {
            self.buf = next;
            self.cursor = self.buf.len();
        }
    }

    fn insert_char(&mut self, ch: char) -> bool {
        let mut encoded = [0u8; 4];
        let bytes = ch.encode_utf8(&mut encoded).as_bytes();
        if self.buf.len().saturating_add(bytes.len()) > MAX_VALUE_BYTES {
            return false;
        }
        let pos = self.cursor.min(self.buf.len());
        self.buf.splice(pos..pos, bytes.iter().copied());
        self.cursor = pos.saturating_add(bytes.len());
        true
    }

    fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut pos = self.cursor.saturating_sub(1);
        while pos > 0
            && self
                .buf
                .get(pos)
                .is_some_and(|&b| (b & 0b1100_0000) == 0b1000_0000)
        {
            pos = pos.saturating_sub(1);
        }
        self.cursor = pos;
        true
    }

    fn move_right(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let mut pos = self.cursor.saturating_add(1);
        while pos < self.buf.len()
            && self
                .buf
                .get(pos)
                .is_some_and(|&b| (b & 0b1100_0000) == 0b1000_0000)
        {
            pos = pos.saturating_add(1);
        }
        self.cursor = pos;
        true
    }

    fn delete_prev(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let end = self.cursor.min(self.buf.len());
        self.move_left();
        let start = self.cursor;
        if start < end && end <= self.buf.len() {
            self.buf.drain(start..end);
            return true;
        }
        false
    }

    fn delete_at_cursor(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let start = self.cursor;
        let mut end = start.saturating_add(1);
        while end < self.buf.len()
            && self
                .buf
                .get(end)
                .is_some_and(|&b| (b & 0b1100_0000) == 0b1000_0000)
        {
            end = end.saturating_add(1);
        }
        if end <= self.buf.len() {
            self.buf.drain(start..end);
            return true;
        }
        false
    }
}

/// Split a path buffer into `(dir_to_query, dir_prefix_to_keep, leaf_prefix)`.
fn split_path_for_completion(path: &[u8]) -> (BString, BString, BString) {
    if let Some(slash) = path.iter().rposition(|&b| b == b'/' || b == b'\\') {
        let prefix = path.get(..=slash).unwrap_or_default();
        let leaf = path.get(slash + 1..).unwrap_or_default();
        let query = if prefix.is_empty() {
            BString::from(".")
        } else {
            BString::from(prefix)
        };
        (query, BString::from(prefix), BString::from(leaf))
    } else {
        (BString::from("."), BString::default(), BString::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_toggle_and_strings_cycle() {
        assert_eq!(toggle_bool(b"OFF"), BString::from("ON"));
        assert_eq!(toggle_bool(b"ON"), BString::from("OFF"));
        assert_eq!(toggle_bool(b"FALSE"), BString::from("ON"));
        assert_eq!(toggle_bool(b"1"), BString::from("OFF"));

        let opts = vec![
            BString::from("Debug"),
            BString::from("Release"),
            BString::from("RelWithDebInfo"),
        ];
        assert_eq!(
            cycle_strings(b"Debug", &opts, true),
            Some(BString::from("Release"))
        );
        assert_eq!(
            cycle_strings(b"RelWithDebInfo", &opts, true),
            Some(BString::from("Debug"))
        );
        assert_eq!(
            cycle_strings(b"Debug", &opts, false),
            Some(BString::from("RelWithDebInfo"))
        );
    }

    #[test]
    fn text_edit_supports_navigation_editing_and_64kib_cap() {
        let mut w = TextEditWidget::new(CacheType::String, b"abc");
        assert_eq!(w.handle_key(Key::Left), EditAction::Redraw);
        assert_eq!(w.handle_key(Key::Char('X')), EditAction::Redraw);
        assert_eq!(w.buffer(), b"abXc");
        assert_eq!(w.handle_key(Key::Home), EditAction::Redraw);
        assert_eq!(w.handle_key(Key::Delete), EditAction::Redraw);
        assert_eq!(w.buffer(), b"bXc");
        assert_eq!(w.handle_key(Key::End), EditAction::Redraw);
        assert_eq!(w.handle_key(Key::Backspace), EditAction::Redraw);
        assert_eq!(w.buffer(), b"bX");
        assert_eq!(w.handle_key(Key::Enter), EditAction::Commit("bX".into()));

        let big = vec![b'a'; MAX_VALUE_BYTES];
        let mut capped = TextEditWidget::new(CacheType::String, &big);
        assert_eq!(capped.buffer().len(), MAX_VALUE_BYTES);
        assert_eq!(capped.handle_key(Key::Char('z')), EditAction::None);
        assert_eq!(capped.buffer().len(), MAX_VALUE_BYTES);
    }

    #[test]
    fn path_tab_completion_cycles_sorted_candidates() {
        let mut w = TextEditWidget::new(CacheType::Filepath, b"/tmp/fo");
        assert_eq!(
            w.handle_key(Key::Tab),
            EditAction::RequestCompletion {
                dir: BString::from("/tmp/")
            }
        );
        w.apply_completion(vec![
            DirItem {
                name: BString::from("foo.txt"),
                is_dir: false,
            },
            DirItem {
                name: BString::from("bar"),
                is_dir: true,
            },
            DirItem {
                name: BString::from("folder"),
                is_dir: true,
            },
        ]);
        assert_eq!(w.buffer(), b"/tmp/folder/");
        assert_eq!(w.handle_key(Key::Tab), EditAction::Redraw);
        assert_eq!(w.buffer(), b"/tmp/foo.txt");
        assert_eq!(w.handle_key(Key::Tab), EditAction::Redraw);
        assert_eq!(w.buffer(), b"/tmp/folder/");
    }
}
