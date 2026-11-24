//! Terminal cell and editor column coordinates.

use std::fmt::Write as _;

use rcmake_base::ByteSlice as _;

const TAB_WIDTH: usize = 4;
const CHECKPOINT_SPACING: usize = 256;
const MAX_GLYPH_BYTES: usize = 256;

#[derive(Clone, Copy, Default)]
struct Boundary {
    byte: usize,
    column: usize,
}

/// A terminal cell map with sparse byte and column checkpoints.
pub(super) struct Cells<'a> {
    bytes: &'a [u8],
    checkpoints: Vec<Boundary>,
    width: usize,
}

/// A portion of a source line with markers for omitted cells.
pub(super) struct Window {
    /// Zero-based column of the first cell.
    pub(super) start: usize,
    /// Zero-based column after the last cell.
    pub(super) end: usize,
    /// Text for the visible cells, with tabs expanded.
    pub(super) text: String,
    /// Whether this window omits cells on the left.
    pub(super) before: bool,
    /// Whether this window omits cells on the right.
    pub(super) after: bool,
}

impl<'a> Cells<'a> {
    /// Measure a line of source bytes in terminal cells.
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        let mut checkpoints = vec![Boundary::default()];
        let mut width = 0;
        let mut next_checkpoint = CHECKPOINT_SPACING;
        for glyph in glyphs(bytes, Boundary::default()) {
            if glyph.start.column >= next_checkpoint {
                checkpoints.push(glyph.start);
                next_checkpoint = glyph.start.column + CHECKPOINT_SPACING;
            }
            width = glyph.end.column;
        }
        Self {
            bytes,
            checkpoints,
            width,
        }
    }

    /// Get the number of terminal cells in the line.
    pub(super) fn width(&self) -> usize {
        self.width
    }

    /// Map a byte offset to the start or end of its display glyph.
    pub(super) fn column(&self, byte: usize, round_up: bool) -> usize {
        let index = self.checkpoints.partition_point(|point| point.byte <= byte);
        let point = self.checkpoint(index);
        glyphs(self.bytes, point)
            .find(|glyph| glyph.end.byte > byte)
            .map_or(self.width, |glyph| {
                if round_up && glyph.start.byte < byte {
                    glyph.end.column
                } else {
                    glyph.start.column
                }
            })
    }

    /// Crop to `width` cells while keeping the glyph at `focus` visible.
    pub(super) fn window(&self, focus: usize, width: usize, ellipsis: usize) -> Window {
        let point = self.at_or_before(focus);
        let end = glyphs(self.bytes, point)
            .next()
            .map_or(focus, |glyph| glyph.end.column);
        let start = if self.width > width && end > width.saturating_sub(ellipsis) {
            let payload = width.saturating_sub(2 * ellipsis);
            let target = self.at_or_before(focus.saturating_sub(payload / 2));
            if end - target.column <= payload {
                target
            } else {
                point
            }
        } else {
            Boundary::default()
        };
        let before = start.column != 0;
        let limit = start.column + width.saturating_sub(usize::from(before) * ellipsis);
        let after = limit < self.width;
        let limit = limit.saturating_sub(usize::from(after) * ellipsis);
        let (text, end) = self.render(start, limit);
        Window {
            start: start.column,
            end,
            text,
            before,
            after,
        }
    }

    /// Get the text and end column of the cells that fit after `start`.
    pub(super) fn piece(&self, start: usize, width: usize) -> (String, usize) {
        self.render(self.at_or_before(start), start + width)
    }

    fn render(&self, start: Boundary, limit: usize) -> (String, usize) {
        let mut text = String::new();
        let mut end = start.column;
        for glyph in glyphs(self.bytes, start).take_while(|glyph| glyph.end.column <= limit) {
            glyph.write(self.bytes, &mut text);
            end = glyph.end.column;
        }
        (text, end)
    }

    fn at_or_before(&self, column: usize) -> Boundary {
        let index = self
            .checkpoints
            .partition_point(|point| point.column <= column);
        let point = self.checkpoint(index);
        glyphs(self.bytes, point)
            .take_while(|glyph| glyph.end.column <= column)
            .last()
            .map_or(point, |glyph| glyph.end)
    }

    fn checkpoint(&self, upper_bound: usize) -> Boundary {
        self.checkpoints
            .get(upper_bound.saturating_sub(1))
            .copied()
            .unwrap_or_default()
    }
}

struct Glyph {
    start: Boundary,
    end: Boundary,
    head: char,
}

impl Glyph {
    fn write(&self, bytes: &[u8], text: &mut String) {
        let width = self.end.column - self.start.column;
        let content = bytes
            .get(self.start.byte..self.end.byte)
            .unwrap_or_default();
        if content.len() > MAX_GLYPH_BYTES {
            text.extend(std::iter::repeat_n('�', width));
        } else if is_format(self.head) {
            let _ = write!(text, "<U+{:04X}>", u32::from(self.head));
        } else if self.head == '\t' {
            text.extend(std::iter::repeat_n(' ', width));
        } else {
            if extends(self.head) {
                text.push('◌');
            }
            text.extend(content.chars().map(visible_char));
        }
    }
}

fn glyphs(bytes: &[u8], from: Boundary) -> impl Iterator<Item = Glyph> + '_ {
    let mut scalars = bytes
        .get(from.byte..)
        .unwrap_or_default()
        .char_indices()
        .peekable();
    let mut column = from.column;
    std::iter::from_fn(move || {
        let (start, mut end, head) = scalars.next()?;
        let format = is_format(head);
        let mut width = if format {
            4 + if u32::from(head) > 0xffff { 5 } else { 4 }
        } else if head == '\t' {
            TAB_WIDTH - column % TAB_WIDTH
        } else if !extends(head) && is_wide(head) {
            2
        } else {
            1
        };
        let emoji = is_pictograph(head);
        let mut joins_regional = is_regional(head);
        while let Some(&(_, next_end, next)) = scalars.peek() {
            let regional_pair = joins_regional && is_regional(next);
            if format || head.is_control() || (!extends(next) && !regional_pair) {
                break;
            }
            if (emoji && next == '\u{fe0f}') || next == '\u{20e3}' || regional_pair {
                width = 2;
            }
            end = next_end;
            scalars.next();
            joins_regional = false;
        }
        let glyph = Glyph {
            start: Boundary {
                byte: from.byte + start,
                column,
            },
            end: Boundary {
                byte: from.byte + end,
                column: column + width,
            },
            head,
        };
        column += width;
        Some(glyph)
    })
}

fn extends(ch: char) -> bool {
    is_combining(ch) || is_variant(ch)
}

fn is_regional(ch: char) -> bool {
    matches!(ch, '\u{1f1e6}'..='\u{1f1ff}')
}

/// Get a one-based Unicode scalar column. Count a tab as one scalar.
pub(super) fn editor_column(bytes: &[u8], byte: usize) -> usize {
    bytes
        .char_indices()
        .take_while(|&(_, end, _)| end <= byte)
        .count()
        + 1
}

/// Replace terminal control and format characters with a visible character.
pub(super) fn visible_char(ch: char) -> char {
    if ch.is_control() || is_format(ch) {
        '�'
    } else {
        ch
    }
}

fn is_variant(ch: char) -> bool {
    matches!(ch, '\u{fe00}'..='\u{fe0f}' | '\u{1f3fb}'..='\u{1f3ff}' | '\u{e0100}'..='\u{e01ef}')
}

fn is_combining(ch: char) -> bool {
    matches!(ch,
        '\u{0300}'..='\u{036f}' | '\u{0483}'..='\u{0489}' | '\u{0591}'..='\u{05bd}' |
        '\u{05bf}' | '\u{05c1}'..='\u{05c2}' | '\u{05c4}'..='\u{05c5}' | '\u{05c7}' |
        '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06dc}' |
        '\u{06df}'..='\u{06e4}' | '\u{06e7}'..='\u{06e8}' | '\u{06ea}'..='\u{06ed}' |
        '\u{0711}' | '\u{0730}'..='\u{074a}' | '\u{07a6}'..='\u{07b0}' | '\u{07eb}'..='\u{07f3}' |
        '\u{0816}'..='\u{0819}' | '\u{081b}'..='\u{0823}' | '\u{0825}'..='\u{0827}' | '\u{0829}'..='\u{082d}' |
        '\u{0859}'..='\u{085b}' | '\u{08d3}'..='\u{08e1}' | '\u{08e3}'..='\u{0902}' |
        '\u{093a}' | '\u{093c}' | '\u{0941}'..='\u{0948}' | '\u{094d}' | '\u{0951}'..='\u{0957}' |
        '\u{0962}'..='\u{0963}' | '\u{0981}' | '\u{09bc}' | '\u{09c1}'..='\u{09c4}' | '\u{09cd}' |
        '\u{0e31}' | '\u{0e34}'..='\u{0e3a}' | '\u{0e47}'..='\u{0e4e}' |
        '\u{0eb1}' | '\u{0eb4}'..='\u{0ebc}' | '\u{0ec8}'..='\u{0ecf}' |
        '\u{1160}'..='\u{11ff}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}' |
        '\u{20d0}'..='\u{20ff}' | '\u{302a}'..='\u{302f}' | '\u{3099}'..='\u{309a}' |
        '\u{d7b0}'..='\u{d7ff}' | '\u{fe20}'..='\u{fe2f}'
    )
}

fn is_pictograph(ch: char) -> bool {
    matches!(ch,
        '©' | '®' | '\u{203c}' | '\u{2049}' | '\u{2122}' | '\u{2139}' |
        '\u{2194}'..='\u{21aa}' | '\u{231a}'..='\u{23fa}' | '\u{24c2}' |
        '\u{25aa}'..='\u{27bf}' | '\u{2934}'..='\u{2935}' | '\u{2b05}'..='\u{2b55}' |
        '\u{3030}' | '\u{303d}' | '\u{3297}' | '\u{3299}' | '\u{1f000}'..='\u{1faff}'
    )
}

fn is_wide(ch: char) -> bool {
    matches!(ch,
        '\u{1100}'..='\u{115f}' | '\u{231a}'..='\u{231b}' | '\u{2329}'..='\u{232a}' |
        '\u{23e9}'..='\u{23ec}' | '\u{23f0}' | '\u{23f3}' | '\u{25fd}'..='\u{25fe}' |
        '\u{2614}'..='\u{2615}' | '\u{2648}'..='\u{2653}' | '\u{267f}' | '\u{2693}' |
        '\u{26a1}' | '\u{26aa}'..='\u{26ab}' | '\u{26bd}'..='\u{26be}' | '\u{26c4}'..='\u{26c5}' |
        '\u{26ce}' | '\u{26d4}' | '\u{26ea}' | '\u{26f2}'..='\u{26f3}' | '\u{26f5}' | '\u{26fa}' |
        '\u{26fd}' | '\u{2705}' | '\u{270a}'..='\u{270b}' | '\u{2728}' | '\u{274c}' | '\u{274e}' |
        '\u{2753}'..='\u{2755}' | '\u{2757}' | '\u{2795}'..='\u{2797}' | '\u{27b0}' | '\u{27bf}' |
        '\u{2b1b}'..='\u{2b1c}' | '\u{2b50}' | '\u{2b55}' | '\u{2e80}'..='\u{a4cf}' |
        '\u{a960}'..='\u{a97f}' | '\u{ac00}'..='\u{d7a3}' | '\u{f900}'..='\u{faff}' |
        '\u{fe10}'..='\u{fe19}' | '\u{fe30}'..='\u{fe6f}' | '\u{ff01}'..='\u{ff60}' |
        '\u{ffe0}'..='\u{ffe6}' | '\u{16fe0}'..='\u{16fff}' | '\u{17000}'..='\u{18dff}' |
        '\u{1b000}'..='\u{1b2ff}' | '\u{1f004}' | '\u{1f0cf}' | '\u{1f18e}' |
        '\u{1f191}'..='\u{1f19a}' | '\u{1f1e6}'..='\u{1f1ff}' | '\u{1f200}'..='\u{1f251}' |
        '\u{1f300}'..='\u{1faff}' | '\u{20000}'..='\u{3fffd}'
    )
}

fn is_format(ch: char) -> bool {
    // Unicode 17.0 General_Category=Cf.
    matches!(ch,
        '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' |
        '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' |
        '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' |
        '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
        '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' |
        '\u{e0001}' | '\u{e0020}'..='\u{e007f}'
    )
}

#[test]
fn escapes_bidi_and_zero_width_characters_at_their_display_width() {
    for (ch, escaped) in [('\u{202e}', "<U+202E>"), ('\u{200b}', "<U+200B>")] {
        let line = format!("a{ch}b");
        let cells = Cells::new(line.as_bytes());
        let width = escaped.len() + 2;
        assert_eq!(cells.column(1, false), 1);
        assert_eq!(cells.column(1 + ch.len_utf8(), false), width - 1);
        assert_eq!(cells.width(), width);
        assert_eq!(cells.piece(0, width), (format!("a{escaped}b"), width));
    }
}
