//! Cell grid, control-byte sanitization, and minimal VT100 diff renderer.

use std::fmt::Write as _;

/// Visual styling for a single terminal cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Highlighted in bold (used for newly added cache entries).
    pub bold: bool,
    /// Highlighted in reverse video (used for the selected entry and status bar).
    pub reverse: bool,
}

/// Single character cell inside [`Screen`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Printable character displayed at this coordinate.
    pub ch: char,
    /// Styling applied to this cell.
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
        }
    }
}

/// Sanitize a single raw byte for safe display on a terminal.
///
/// Control bytes below `0x20` and `0x7f` render as `'?'`, preventing untrusted
/// cache values from injecting terminal escape sequences.
#[must_use]
pub const fn sanitize_byte(byte: u8) -> char {
    if matches!(byte, 0x20..=0x7e) {
        byte as char
    } else {
        '?'
    }
}

/// Sanitize arbitrary raw bytes into safe display characters.
///
/// Any byte `< 0x20`, `0x7f`, C1 control (`U+0080..=U+009F`), or invalid UTF-8
/// byte renders as `'?'`, ensuring cache values can never inject escape sequences.
#[must_use]
pub fn sanitize_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        let Some(&byte) = bytes.get(idx) else {
            break;
        };
        if (0x20..=0x7e).contains(&byte) {
            out.push(char::from(byte));
            idx += 1;
        } else if byte < 0x20 || byte == 0x7f {
            out.push('?');
            idx += 1;
        } else {
            let tail = bytes.get(idx..).unwrap_or_default();
            match std::str::from_utf8(tail) {
                Ok(valid) => {
                    for ch in valid.chars() {
                        if ch.is_control() {
                            out.push('?');
                        } else {
                            out.push(ch);
                        }
                    }
                    break;
                }
                Err(err) => {
                    let valid_len = err.valid_up_to();
                    if valid_len > 0
                        && let Some(prefix) = tail.get(..valid_len)
                        && let Ok(valid) = std::str::from_utf8(prefix)
                    {
                        for ch in valid.chars() {
                            if ch.is_control() {
                                out.push('?');
                            } else {
                                out.push(ch);
                            }
                        }
                        idx += valid_len;
                    }
                    let bad_len = err.error_len().unwrap_or(1).max(1);
                    for _ in 0..bad_len {
                        out.push('?');
                    }
                    idx = idx.saturating_add(bad_len);
                }
            }
        }
    }
    out
}

/// Sanitize a single character before placing it in a screen cell.
#[must_use]
fn sanitize_char(ch: char) -> char {
    if ch.is_control() { '?' } else { ch }
}

/// Rectangular 2D grid of styled cells and optional cursor position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor: Option<(u16, u16)>,
}

impl Screen {
    /// Allocate a blank screen of `cols` × `rows` cells.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let total = usize::from(cols).saturating_mul(usize::from(rows));
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); total],
            cursor: None,
        }
    }

    /// Screen dimensions as `(cols, rows)`.
    #[must_use]
    pub const fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Set the visible cursor position `(row, col)` (0-indexed), or `None` to hide it.
    pub fn set_cursor(&mut self, cursor: Option<(u16, u16)>) {
        self.cursor = cursor.map(|(r, c)| {
            (
                r.min(self.rows.saturating_sub(1)),
                c.min(self.cols.saturating_sub(1)),
            )
        });
    }

    /// Current visible cursor position, if any.
    #[must_use]
    pub const fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    /// Read cell at `(row, col)`, or `None` if out of bounds.
    #[must_use]
    pub fn get(&self, row: u16, col: u16) -> Option<Cell> {
        let idx = self.index(row, col)?;
        self.cells.get(idx).copied()
    }

    /// Write one cell at `(row, col)`. Out-of-bounds writes are ignored safely.
    pub fn set(&mut self, row: u16, col: u16, ch: char, style: Style) {
        if let Some(idx) = self.index(row, col)
            && let Some(cell) = self.cells.get_mut(idx)
        {
            *cell = Cell {
                ch: sanitize_char(ch),
                style,
            };
        }
    }

    /// Fill an entire row with `ch` and `style`.
    pub fn fill_row(&mut self, row: u16, ch: char, style: Style) {
        if row >= self.rows {
            return;
        }
        let ch = sanitize_char(ch);
        let start = usize::from(row).saturating_mul(usize::from(self.cols));
        let end = start.saturating_add(usize::from(self.cols));
        if let Some(slice) = self.cells.get_mut(start..end) {
            slice.fill(Cell { ch, style });
        }
    }

    /// Write sanitized text starting at `(row, col)` up to the right edge of the screen.
    pub fn put_str(&mut self, row: u16, col: u16, text: &str, style: Style) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let mut c = col;
        for ch in text.chars() {
            if c >= self.cols {
                break;
            }
            self.set(row, c, ch, style);
            c = c.saturating_add(1);
        }
    }

    /// Write sanitized raw bytes at `(row, col)` truncated to `max_width` columns.
    pub fn put_bytes(&mut self, row: u16, col: u16, bytes: &[u8], style: Style, max_width: u16) {
        if row >= self.rows || col >= self.cols || max_width == 0 {
            return;
        }
        let sanitized = sanitize_bytes(bytes);
        let limit = col.saturating_add(max_width).min(self.cols);
        let mut c = col;
        for ch in sanitized.chars() {
            if c >= limit {
                break;
            }
            self.set(row, c, ch, style);
            c = c.saturating_add(1);
        }
    }

    /// Extract the characters of `row` as a `String`.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        if row >= self.rows {
            return String::new();
        }
        let start = usize::from(row).saturating_mul(usize::from(self.cols));
        let end = start.saturating_add(usize::from(self.cols));
        self.cells
            .get(start..end)
            .map(|slice| slice.iter().map(|cell| cell.ch).collect())
            .unwrap_or_default()
    }

    fn index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        Some(
            usize::from(row)
                .saturating_mul(usize::from(self.cols))
                .saturating_add(usize::from(col)),
        )
    }
}

/// Stateful VT100 diff renderer emitting minimal `CUP` and `SGR` sequences.
#[derive(Default)]
pub struct Renderer {
    prev: Option<Screen>,
    prev_cursor_visible: bool,
}

impl Renderer {
    /// Create a fresh renderer with no prior frame.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prev: None,
            prev_cursor_visible: false,
        }
    }

    /// Force a full screen redraw on the next [`Renderer::render`] call.
    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    /// Render `next` relative to the previous frame and return the VT100 byte stream.
    pub fn render(&mut self, next: &Screen) -> Vec<u8> {
        let mut out = String::new();
        let (cols, rows) = next.size();
        let full = self.prev.as_ref().is_none_or(|p| p.size() != (cols, rows));

        let mut active_style = Style::default();

        if full {
            out.push_str("\x1b[?25l\x1b[0m\x1b[2J");
            self.prev_cursor_visible = false;
            for row in 0..rows {
                let mut col = 0u16;
                while col < cols {
                    let Some(cell) = next.get(row, col) else {
                        break;
                    };
                    if cell == Cell::default() {
                        col = col.saturating_add(1);
                        continue;
                    }
                    let _ = write!(&mut out, "\x1b[{};{}H", row + 1, col + 1);
                    while col < cols {
                        let Some(c) = next.get(row, col) else {
                            break;
                        };
                        if c == Cell::default() && is_rest_default(next, row, col, cols) {
                            break;
                        }
                        write_style(&mut out, &mut active_style, c.style);
                        out.push(c.ch);
                        col = col.saturating_add(1);
                    }
                }
            }
        } else if let Some(prev) = &self.prev {
            for row in 0..rows {
                let mut col = 0u16;
                while col < cols {
                    if prev.get(row, col) == next.get(row, col) {
                        col = col.saturating_add(1);
                        continue;
                    }
                    let _ = write!(&mut out, "\x1b[{};{}H", row + 1, col + 1);
                    while col < cols && prev.get(row, col) != next.get(row, col) {
                        if let Some(c) = next.get(row, col) {
                            write_style(&mut out, &mut active_style, c.style);
                            out.push(c.ch);
                        }
                        col = col.saturating_add(1);
                    }
                }
            }
        }

        if active_style != Style::default() {
            out.push_str("\x1b[0m");
        }

        match next.cursor() {
            Some((r, c)) => {
                let _ = write!(&mut out, "\x1b[{};{}H", r + 1, c + 1);
                if !self.prev_cursor_visible {
                    out.push_str("\x1b[?25h");
                    self.prev_cursor_visible = true;
                }
            }
            None => {
                if self.prev_cursor_visible {
                    out.push_str("\x1b[?25l");
                    self.prev_cursor_visible = false;
                }
            }
        }

        self.prev = Some(next.clone());
        out.into_bytes()
    }
}

fn is_rest_default(screen: &Screen, row: u16, start_col: u16, cols: u16) -> bool {
    (start_col..cols).all(|c| screen.get(row, c) == Some(Cell::default()))
}

fn write_style(out: &mut String, active: &mut Style, next: Style) {
    if *active == next {
        return;
    }
    match (next.bold, next.reverse) {
        (false, false) => out.push_str("\x1b[0m"),
        (true, false) => out.push_str("\x1b[0;1m"),
        (false, true) => out.push_str("\x1b[0;7m"),
        (true, true) => out.push_str("\x1b[0;1;7m"),
    }
    *active = next;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bytes_render_as_question_mark() {
        let mut screen = Screen::new(20, 2);
        screen.put_bytes(0, 0, b"a\x1b[31mb\x00\x07\x7fc", Style::default(), 20);
        assert_eq!(screen.row_text(0).trim_end(), "a?[31mb???c");

        // C1 control byte U+009B (8-bit CSI) is also neutralized.
        let mut c1 = Vec::new();
        c1.extend_from_slice("x\u{009b}31my".as_bytes());
        assert_eq!(sanitize_bytes(&c1), "x?31my");
    }

    #[test]
    fn single_cell_diff_emits_one_cup_and_one_byte() {
        let mut renderer = Renderer::new();
        let mut frame = Screen::new(10, 4);
        frame.set(2, 4, 'a', Style::default());
        let _initial = renderer.render(&frame);

        frame.set(2, 4, 'b', Style::default());
        let diff = renderer.render(&frame);
        assert_eq!(diff, b"\x1b[3;5Hb");
    }
}
