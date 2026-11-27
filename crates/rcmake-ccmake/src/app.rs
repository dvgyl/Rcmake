//! Pure TUI state machine (`App::handle(Key) -> Effect`) and screen rendering.

use rcmake_base::BString;
use rcmake_model::CacheType;

use crate::keys::Key;
use crate::layout::{
    EDIT_KEYS_ROW_1, EDIT_KEYS_ROW_2, EMPTY_CACHE_LABEL, MARKER_COL, MIN_HEIGHT, MIN_WIDTH,
    NAME_COL_WIDTH, VALUE_COL, browse_key_lines, entries_per_page, page_header, version_banner,
};
use crate::log_view::{LogAction, LogView};
use crate::model::CacheView;
use crate::screen::{Screen, Style, sanitize_bytes};
use crate::widgets::{EditAction, TextEditWidget, cycle_strings, toggle_bool};
use crate::{CacheSnapshot, DirItem, EntryEdit, LogLine, RunReport};

const MAX_SEARCH_CHARS: usize = 256;

/// Side effect requested by [`App::handle`] for the driver loop to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// No visible change and no driver action.
    None,
    /// Redraw the screen.
    Redraw,
    /// Notify the driver that retained configure state is stale, then redraw.
    InvalidateAndRedraw,
    /// Query directory contents for Tab path completion.
    CompletePath {
        /// Directory path bytes to list.
        dir: BString,
    },
    /// Run configure with pending edits and reload the cache.
    Configure,
    /// Run generate on the retained configured session.
    Generate,
    /// Exit `ccmake` cleanly with the given exit code.
    Quit(i32),
}

enum Mode {
    Browse,
    Edit(TextEditWidget),
    Search { query: String },
    Log { view: LogView, exit_on_close: bool },
}

/// Pure interactive state machine for `ccmake`.
pub struct App {
    cache: CacheView,
    selected: usize,
    ok_to_generate: bool,
    last_search: String,
    log_lines: Vec<LogLine>,
    mode: Mode,
}

impl App {
    /// Construct a fresh [`App`] from an initial cache snapshot.
    #[must_use]
    pub fn new(snapshot: CacheSnapshot) -> Self {
        Self {
            cache: CacheView::from_initial(snapshot),
            selected: 0,
            ok_to_generate: false,
            last_search: String::new(),
            log_lines: Vec::new(),
            mode: Mode::Browse,
        }
    }

    /// Whether `[g] Generate` is currently enabled.
    #[must_use]
    pub const fn ok_to_generate(&self) -> bool {
        self.ok_to_generate
    }

    /// Whether advanced cache entries are currently shown.
    #[must_use]
    pub const fn advanced_mode(&self) -> bool {
        self.cache.advanced_mode()
    }

    /// Currently selected visible entry name, if the cache is non-empty.
    #[must_use]
    pub fn selected_name(&self) -> Option<&[u8]> {
        self.cache
            .visible_get(self.selected)
            .map(|entry| entry.name.as_slice())
    }

    /// The pending edits to pass to [`crate::CacheDriver::configure`]. They stay
    /// pending until [`Self::finish_configure`] gets a report that consumed them.
    #[must_use]
    pub fn pending_edits(&self) -> Vec<EntryEdit> {
        self.cache.pending_edits()
    }

    /// Feed directory entries back into an active path editor after [`Effect::CompletePath`].
    pub fn apply_path_completion(&mut self, items: Vec<DirItem>) {
        if let Mode::Edit(widget) = &mut self.mode {
            widget.apply_completion(items);
        }
    }

    /// Update state after [`crate::CacheDriver::configure`] completes.
    pub fn finish_configure(&mut self, report: RunReport, reloaded: Option<CacheSnapshot>) {
        let had_non_status = report.has_non_status || report.lines.iter().any(|line| line.is_error);
        let failed = report.exit_code != 0;
        let explicit_new = report.new_entries;
        self.log_lines = report.lines;
        if !report.edits_pending {
            self.cache.clear_pending_edits();
        }

        let mut any_new = !explicit_new.is_empty();
        if let Some(snapshot) = reloaded {
            any_new |= self.cache.reload_after_configure(snapshot, &explicit_new);
        }
        self.clamp_selection();

        self.ok_to_generate = !failed && !any_new;

        if failed || had_non_status {
            let title = if failed {
                "Configure failed with the following output"
            } else {
                "Configure produced the following output"
            };
            self.mode = Mode::Log {
                view: LogView::from_log(title, &self.log_lines, true),
                exit_on_close: false,
            };
        } else {
            self.mode = Mode::Browse;
        }
    }

    /// Update state after [`crate::CacheDriver::generate`] completes with warnings or errors.
    pub fn finish_generate(&mut self, report: RunReport) {
        let failed = report.exit_code != 0;
        self.log_lines = report.lines;
        if failed {
            self.ok_to_generate = false;
        }
        let title = if failed {
            "Generate failed with the following output"
        } else {
            "Generate produced the following output"
        };
        self.mode = Mode::Log {
            view: LogView::from_log(title, &self.log_lines, true),
            exit_on_close: !failed,
        };
    }

    /// Process a single [`Key`] event and return the requested [`Effect`].
    pub fn handle(&mut self, key: Key, cols: u16, rows: u16) -> Effect {
        if cols < MIN_WIDTH || rows < MIN_HEIGHT {
            return match key {
                Key::Char('q') | Key::Ctrl(b'c') => Effect::Quit(0),
                _ => Effect::None,
            };
        }

        match &mut self.mode {
            Mode::Browse => self.handle_browse(key, rows),
            Mode::Edit(_) => self.handle_edit(key),
            Mode::Search { .. } => self.handle_search(key),
            Mode::Log { .. } => self.handle_log(key, cols, rows),
        }
    }

    fn handle_browse(&mut self, key: Key, rows: u16) -> Effect {
        let visible_len = self.cache.visible_len();
        let page_size = entries_per_page(rows).max(1);

        match key {
            Key::Char('q') | Key::Ctrl(b'c') => Effect::Quit(0),
            Key::Down | Key::Char('j') | Key::Ctrl(b'n') => {
                if visible_len > 0 && self.selected + 1 < visible_len {
                    self.selected += 1;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Up | Key::Char('k') | Key::Ctrl(b'p') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::PageDown | Key::Ctrl(b'd') => {
                if visible_len == 0 {
                    return Effect::None;
                }
                let next_page_start = ((self.selected / page_size) + 1).saturating_mul(page_size);
                let next = next_page_start.min(visible_len.saturating_sub(1));
                if next != self.selected {
                    self.selected = next;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::PageUp | Key::Ctrl(b'u') => {
                if visible_len == 0 {
                    return Effect::None;
                }
                let cur_page = self.selected / page_size;
                let next = if cur_page > 0 {
                    (cur_page - 1).saturating_mul(page_size)
                } else {
                    0
                };
                if next != self.selected {
                    self.selected = next;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Home => {
                if self.selected != 0 {
                    self.selected = 0;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::End => {
                let last = visible_len.saturating_sub(1);
                if self.selected != last {
                    self.selected = last;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Char('t') => {
                let prev_name = self.selected_name().map(BString::from);
                self.cache.toggle_advanced();
                if let Some(name) = prev_name
                    && let Some(idx) = self.cache.visible_index_of(name.as_slice())
                {
                    self.selected = idx;
                } else {
                    self.selected = 0;
                }
                self.clamp_selection();
                Effect::Redraw
            }
            Key::Char('/') => {
                self.mode = Mode::Search {
                    query: String::new(),
                };
                Effect::Redraw
            }
            Key::Char('n') => {
                if let Some(idx) = self.cache.search_next(self.selected, &self.last_search) {
                    self.selected = idx;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Char('N') => {
                if let Some(idx) = self.cache.search_prev(self.selected, &self.last_search) {
                    self.selected = idx;
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Char('l' | 'w') => {
                self.mode = Mode::Log {
                    view: LogView::from_log(
                        "CMake produced the following output",
                        &self.log_lines,
                        false,
                    ),
                    exit_on_close: false,
                };
                Effect::Redraw
            }
            Key::Char('h') => {
                let entry = self.cache.visible_get(self.selected);
                let name = entry.map(|e| e.name.as_slice());
                let help = entry.map(|e| e.help.as_slice());
                self.mode = Mode::Log {
                    view: LogView::help(name, help),
                    exit_on_close: false,
                };
                Effect::Redraw
            }
            Key::Char('c') => Effect::Configure,
            Key::Char('g') => {
                if self.ok_to_generate {
                    Effect::Generate
                } else {
                    Effect::None
                }
            }
            Key::Char('d') => {
                if self.cache.remove_visible(self.selected).is_some() {
                    self.ok_to_generate = false;
                    self.clamp_selection();
                    Effect::InvalidateAndRedraw
                } else {
                    Effect::None
                }
            }
            Key::Enter => self.activate_selected(true),
            Key::Char(' ') => self.activate_bool_only(),
            Key::Char('i') => self.activate_string_only(),
            Key::Right | Key::Ctrl(b'f') => self.cycle_selected_strings(true),
            Key::Left | Key::Ctrl(b'b') => self.cycle_selected_strings(false),
            _ => Effect::None,
        }
    }

    fn activate_selected(&mut self, forward: bool) -> Effect {
        let Some(entry) = self.cache.visible_get(self.selected).cloned() else {
            return Effect::None;
        };
        if !entry.strings.is_empty() {
            if let Some(next) = cycle_strings(entry.value.as_slice(), &entry.strings, forward)
                && self.cache.set_visible_value(self.selected, next)
            {
                self.ok_to_generate = false;
                return Effect::InvalidateAndRedraw;
            }
            return Effect::None;
        }
        if entry.ty == CacheType::Bool {
            let next = toggle_bool(entry.value.as_slice());
            self.cache.set_visible_value(self.selected, next);
            self.ok_to_generate = false;
            return Effect::InvalidateAndRedraw;
        }
        self.mode = Mode::Edit(TextEditWidget::new(entry.ty, entry.value.as_slice()));
        Effect::Redraw
    }

    fn activate_bool_only(&mut self) -> Effect {
        let Some(entry) = self.cache.visible_get(self.selected).cloned() else {
            return Effect::None;
        };
        if entry.ty == CacheType::Bool && entry.strings.is_empty() {
            let next = toggle_bool(entry.value.as_slice());
            self.cache.set_visible_value(self.selected, next);
            self.ok_to_generate = false;
            Effect::InvalidateAndRedraw
        } else {
            Effect::None
        }
    }

    fn activate_string_only(&mut self) -> Effect {
        let Some(entry) = self.cache.visible_get(self.selected).cloned() else {
            return Effect::None;
        };
        if entry.strings.is_empty() && entry.ty != CacheType::Bool {
            self.mode = Mode::Edit(TextEditWidget::new(entry.ty, entry.value.as_slice()));
            Effect::Redraw
        } else {
            Effect::None
        }
    }

    fn cycle_selected_strings(&mut self, forward: bool) -> Effect {
        let Some(entry) = self.cache.visible_get(self.selected).cloned() else {
            return Effect::None;
        };
        if !entry.strings.is_empty()
            && let Some(next) = cycle_strings(entry.value.as_slice(), &entry.strings, forward)
            && self.cache.set_visible_value(self.selected, next)
        {
            self.ok_to_generate = false;
            return Effect::InvalidateAndRedraw;
        }
        Effect::None
    }

    fn handle_edit(&mut self, key: Key) -> Effect {
        let action = match &mut self.mode {
            Mode::Edit(widget) => widget.handle_key(key),
            _ => return Effect::None,
        };
        match action {
            EditAction::None => Effect::None,
            EditAction::Redraw => Effect::Redraw,
            EditAction::RequestCompletion { dir } => Effect::CompletePath { dir },
            EditAction::Commit(value) => {
                self.mode = Mode::Browse;
                if self.cache.set_visible_value(self.selected, value) {
                    self.ok_to_generate = false;
                    Effect::InvalidateAndRedraw
                } else {
                    Effect::Redraw
                }
            }
            EditAction::Cancel => {
                self.mode = Mode::Browse;
                Effect::Redraw
            }
            EditAction::Quit => Effect::Quit(0),
        }
    }

    fn handle_search(&mut self, key: Key) -> Effect {
        let Mode::Search { query } = &mut self.mode else {
            return Effect::None;
        };
        match key {
            Key::Ctrl(b'c') => Effect::Quit(0),
            Key::Esc => {
                self.mode = Mode::Browse;
                Effect::Redraw
            }
            Key::Enter => {
                let committed = std::mem::take(query);
                self.mode = Mode::Browse;
                if !committed.is_empty() {
                    if let Some(idx) = self.cache.search_next(self.selected, &committed) {
                        self.selected = idx;
                    }
                    self.last_search = committed;
                }
                Effect::Redraw
            }
            Key::Backspace => {
                if query.pop().is_some() {
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            Key::Char(ch) => {
                if query.chars().count() < MAX_SEARCH_CHARS {
                    query.push(ch);
                    Effect::Redraw
                } else {
                    Effect::None
                }
            }
            _ => Effect::None,
        }
    }

    fn handle_log(&mut self, key: Key, cols: u16, rows: u16) -> Effect {
        let (action, exit_on_close) = match &mut self.mode {
            Mode::Log {
                view,
                exit_on_close,
            } => (view.handle_key(key, cols, rows), *exit_on_close),
            _ => return Effect::None,
        };
        match action {
            LogAction::None => Effect::None,
            LogAction::Redraw => Effect::Redraw,
            LogAction::Close => {
                self.mode = Mode::Browse;
                if exit_on_close {
                    Effect::Quit(0)
                } else {
                    Effect::Redraw
                }
            }
            LogAction::Quit => Effect::Quit(0),
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.cache.visible_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Render the current TUI frame onto `screen`.
    pub fn render(&self, screen: &mut Screen, busy_message: Option<&str>) {
        let (cols, rows) = screen.size();
        if cols < MIN_WIDTH || rows < MIN_HEIGHT {
            screen.set_cursor(None);
            screen.put_str(
                0,
                0,
                "Window is too small. A size of at least 65x6 is required.",
                Style::default(),
            );
            return;
        }

        if let Mode::Log { view, .. } = &self.mode {
            view.render(screen);
            return;
        }

        let visible = self.cache.visible_entries();
        let per_page = entries_per_page(rows);
        let page_size = per_page.max(1);
        let total_pages = if visible.is_empty() || per_page == 0 {
            1
        } else {
            visible.len().div_ceil(page_size)
        };
        let current_page_zero = if visible.is_empty() || per_page == 0 {
            0
        } else {
            self.selected / page_size
        };

        let header = page_header(current_page_zero + 1, total_pages);
        let header_w = u16::try_from(header.len()).unwrap_or(cols);
        screen.put_str(0, cols.saturating_sub(header_w), &header, Style::default());

        let mut cursor_pos = None;
        let value_width = cols.saturating_sub(VALUE_COL);

        if per_page > 0 {
            if visible.is_empty() {
                let style = Style {
                    bold: false,
                    reverse: true,
                };
                screen.put_str(1, 0, EMPTY_CACHE_LABEL, style);
            } else {
                let page_start = current_page_zero.saturating_mul(page_size);
                for offset in 0..per_page {
                    let entry_idx = page_start.saturating_add(offset);
                    let Some(entry) = visible.get(entry_idx) else {
                        break;
                    };
                    let row = u16::try_from(offset.saturating_add(1)).unwrap_or(rows);
                    let is_selected = entry_idx == self.selected;
                    let label_style = Style {
                        bold: entry.is_new,
                        reverse: is_selected,
                    };
                    let marker_style = Style {
                        bold: entry.is_new,
                        reverse: is_selected,
                    };
                    let value_style = Style {
                        bold: entry.is_new,
                        reverse: false,
                    };

                    // Fill selected label region with spaces so reverse highlight spans the column.
                    if is_selected {
                        for col in 0..VALUE_COL {
                            screen.set(row, col, ' ', label_style);
                        }
                    }
                    screen.put_bytes(row, 0, entry.name.as_slice(), label_style, NAME_COL_WIDTH);
                    if entry.is_new {
                        screen.set(row, MARKER_COL, '*', marker_style);
                    }

                    if is_selected && let Mode::Edit(widget) = &self.mode {
                        let (display, cursor_col) = render_edit_window(
                            widget.buffer(),
                            widget.cursor(),
                            usize::from(value_width),
                        );
                        screen.put_str(row, VALUE_COL, &display, Style::default());
                        let col = VALUE_COL
                            .saturating_add(u16::try_from(cursor_col).unwrap_or(0))
                            .min(cols.saturating_sub(1));
                        cursor_pos = Some((row, col));
                    } else {
                        screen.put_bytes(
                            row,
                            VALUE_COL,
                            entry.value.as_slice(),
                            value_style,
                            value_width,
                        );
                    }
                }
            }
        }

        let status_row = rows.saturating_sub(5);
        let reverse = Style {
            bold: false,
            reverse: true,
        };
        screen.fill_row(status_row, ' ', reverse);

        if let Some(msg) = busy_message {
            screen.put_str(status_row, 0, msg, reverse);
        } else if let Mode::Search { query } = &self.mode {
            let line = format!("Search: {query}");
            screen.put_str(status_row, 0, &line, reverse);
            let col = u16::try_from(line.chars().count())
                .unwrap_or(cols)
                .min(cols.saturating_sub(1));
            cursor_pos = Some((status_row, col));
        } else if let Some(entry) = visible.get(self.selected) {
            let mut bar = sanitize_bytes(entry.name.as_slice());
            bar.push_str(": ");
            bar.push_str(&sanitize_bytes(entry.help.as_slice()));
            screen.put_str(status_row, 0, &bar, reverse);
        } else {
            screen.put_str(status_row, 0, "EMPTY CACHE: ", reverse);
        }

        let keys_top = rows.saturating_sub(4);
        match &self.mode {
            Mode::Browse if busy_message.is_none() => {
                let lines = browse_key_lines(self.ok_to_generate, self.cache.advanced_mode());
                for (offset, text) in lines.iter().enumerate() {
                    let r = keys_top.saturating_add(u16::try_from(offset).unwrap_or(0));
                    screen.put_str(r, 0, text, Style::default());
                }
                let ver = version_banner();
                let ver_w = u16::try_from(ver.len()).unwrap_or(cols);
                let keys0_w = u16::try_from(lines[0].len()).unwrap_or(cols);
                if cols >= keys0_w.saturating_add(1).saturating_add(ver_w) {
                    screen.put_str(keys_top, cols.saturating_sub(ver_w), &ver, Style::default());
                }
            }
            Mode::Edit(_) => {
                screen.put_str(
                    keys_top.saturating_add(1),
                    0,
                    EDIT_KEYS_ROW_1,
                    Style::default(),
                );
                screen.put_str(
                    keys_top.saturating_add(2),
                    0,
                    EDIT_KEYS_ROW_2,
                    Style::default(),
                );
            }
            _ => {}
        }

        screen.set_cursor(cursor_pos);
    }
}

/// Slice a sanitized edit string horizontally around `cursor_byte` to fit `width` columns.
fn render_edit_window(buf: &[u8], cursor_byte: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let before = sanitize_bytes(buf.get(..cursor_byte.min(buf.len())).unwrap_or_default());
    let after = sanitize_bytes(buf.get(cursor_byte.min(buf.len())..).unwrap_or_default());
    let cursor_chars = before.chars().count();
    let full: Vec<char> = before.chars().chain(after.chars()).collect();

    let scroll = if cursor_chars >= width {
        cursor_chars.saturating_sub(width.saturating_sub(1))
    } else {
        0
    };
    let end = scroll.saturating_add(width).min(full.len());
    let slice: String = full.get(scroll..end).unwrap_or_default().iter().collect();
    (slice, cursor_chars.saturating_sub(scroll))
}
