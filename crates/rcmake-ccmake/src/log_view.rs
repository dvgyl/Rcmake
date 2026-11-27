//! Scrollable long-message viewer used for configure/generate logs and the help screen.

use crate::LogLine;
use crate::keys::Key;
use crate::layout::{LONG_MSG_EXIT_HINT, version_banner};
use crate::screen::{Screen, Style, sanitize_bytes};

/// Static help text displayed when the user presses `h`.
pub const HELP_TEXT: &str = "\
Welcome to ccmake, curses-style terminal interface for CMake.

Basic steps:
  1. Edit cache entries using the navigation and editing keys below.
  2. Press 'c' to configure. Newly added entries appear at the top marked with *.
  3. Repeat until satisfied and no new entries remain.
  4. Press 'g' to generate build files and exit.
  5. Press 'q' (or Ctrl-C) at any time to quit without generating.

Navigation keys:
  Up / k / Ctrl-P    : previous option
  Down / j / Ctrl-N  : next option
  PgUp / Ctrl-U      : previous page
  PgDn / Ctrl-D      : next page
  Home / End         : first / last option
  /                  : search variable names
  n / N              : next / previous search match

Editing keys:
  Enter              : toggle BOOL, cycle STRINGS, or edit value
  Space              : toggle BOOL
  i                  : enter edit mode for STRING / PATH / FILEPATH
  Tab                : complete path (PATH / FILEPATH)
  Esc                : cancel edit
  d                  : delete selected cache entry

Commands:
  c                  : configure
  g                  : generate and exit (when available)
  t                  : toggle advanced mode
  l / w              : show configure/generate log output
  h                  : show this help screen
  q / Ctrl-C         : quit without generating";

/// Outcome of handling a key inside [`LogView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogAction {
    /// Stay in the log/help viewer; no redraw required.
    None,
    /// Stay in the viewer and redraw.
    Redraw,
    /// Leave the viewer and return to the previous screen.
    Close,
    /// Exit `ccmake` immediately (`Ctrl-C`).
    Quit,
}

/// Scrollable text screen for log output and help pages.
#[derive(Clone, Debug)]
pub struct LogView {
    title: String,
    lines: Vec<String>,
    scroll: usize,
    scroll_to_bottom: bool,
}

impl LogView {
    /// Create a log viewer populated from captured [`LogLine`] entries.
    #[must_use]
    pub fn from_log(title: impl Into<String>, log: &[LogLine], scroll_to_bottom: bool) -> Self {
        let mut lines = Vec::new();
        for entry in log {
            let text = sanitize_bytes(entry.text.as_slice());
            lines.push(text);
        }
        Self {
            title: title.into(),
            lines,
            scroll: 0,
            scroll_to_bottom,
        }
    }

    /// Create a help screen describing the selected entry (if any) and key bindings.
    #[must_use]
    pub fn help(selected_name: Option<&[u8]>, selected_help: Option<&[u8]>) -> Self {
        let mut lines = Vec::new();
        if let Some(name) = selected_name {
            lines.push(format!("Current option is: {}", sanitize_bytes(name)));
            if let Some(help) = selected_help
                && !help.is_empty()
            {
                lines.push(format!(
                    "Help string for this option is: {}",
                    sanitize_bytes(help)
                ));
            }
            lines.push(String::new());
        }
        for line in HELP_TEXT.lines() {
            lines.push(line.to_owned());
        }
        Self {
            title: "Help".to_owned(),
            lines,
            scroll: 0,
            scroll_to_bottom: false,
        }
    }

    /// Handle one keyboard event in the log/help screen.
    pub fn handle_key(&mut self, key: Key, cols: u16, rows: u16) -> LogAction {
        let page = viewport_height(rows).max(1);
        let max_scroll = self.wrapped_lines(cols).len().saturating_sub(page);
        if self.scroll_to_bottom {
            self.scroll = max_scroll;
            self.scroll_to_bottom = false;
        }
        match key {
            Key::Ctrl(b'c') => LogAction::Quit,
            Key::Char('e' | 'o' | 'q') | Key::Esc => LogAction::Close,
            Key::Down | Key::Char('j') | Key::Ctrl(b'n') => {
                let next = (self.scroll.saturating_add(1)).min(max_scroll);
                if next != self.scroll {
                    self.scroll = next;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            Key::Up | Key::Char('k') | Key::Ctrl(b'p') => {
                let next = self.scroll.saturating_sub(1);
                if next != self.scroll {
                    self.scroll = next;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            Key::PageDown | Key::Ctrl(b'd') => {
                let next = (self.scroll.saturating_add(page)).min(max_scroll);
                if next != self.scroll {
                    self.scroll = next;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            Key::PageUp | Key::Ctrl(b'u') => {
                let next = self.scroll.saturating_sub(page);
                if next != self.scroll {
                    self.scroll = next;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            Key::Home => {
                if self.scroll != 0 {
                    self.scroll = 0;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            Key::End => {
                if self.scroll != max_scroll {
                    self.scroll = max_scroll;
                    LogAction::Redraw
                } else {
                    LogAction::None
                }
            }
            _ => LogAction::None,
        }
    }

    /// Render the log or help page onto `screen`.
    pub fn render(&self, screen: &mut Screen) {
        let (cols, rows) = screen.size();
        screen.set_cursor(None);
        let view_h = viewport_height(rows);
        let wrapped = self.wrapped_lines(cols);
        let max_scroll = wrapped.len().saturating_sub(view_h.max(1));
        let scroll = if self.scroll_to_bottom {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        for row_idx in 0..view_h {
            if let Some(line) = wrapped.get(scroll.saturating_add(row_idx)) {
                let r = u16::try_from(row_idx.saturating_add(1)).unwrap_or(rows);
                screen.put_str(r, 1, line, Style::default());
            }
        }

        if rows >= 4 {
            let status_row = rows.saturating_sub(4);
            let reverse = Style {
                bold: false,
                reverse: true,
            };
            screen.fill_row(status_row, ' ', reverse);
            screen.put_str(status_row, 0, &self.title, reverse);

            let ver = version_banner();
            let ver_len = u16::try_from(ver.len()).unwrap_or(cols);
            let ver_col = cols.saturating_sub(ver_len);
            screen.put_str(rows.saturating_sub(3), ver_col, &ver, Style::default());
            screen.put_str(
                rows.saturating_sub(2),
                0,
                LONG_MSG_EXIT_HINT,
                Style::default(),
            );
        }
    }

    fn wrapped_lines(&self, cols: u16) -> Vec<String> {
        let width = usize::from(cols.saturating_sub(2)).max(1);
        let mut out = Vec::new();
        for line in &self.lines {
            if line.is_empty() {
                out.push(String::new());
                continue;
            }
            let mut cur = String::new();
            let mut count = 0usize;
            for ch in line.chars() {
                if count >= width {
                    out.push(std::mem::take(&mut cur));
                    count = 0;
                }
                cur.push(ch);
                count += 1;
            }
            out.push(cur);
        }
        out
    }
}

const fn viewport_height(rows: u16) -> usize {
    rows.saturating_sub(6) as usize
}
