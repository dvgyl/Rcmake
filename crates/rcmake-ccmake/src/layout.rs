//! Layout calculations and exact upstream `ccmake` text constants.

use rcmake_base::CMAKE_VERSION_STR;

/// Minimum required terminal width (columns).
pub const MIN_WIDTH: u16 = 65;

/// Minimum required terminal height (rows).
pub const MIN_HEIGHT: u16 = 6;

/// Error message emitted when terminal initialization fails.
pub const ERR_INIT_FAILED: &str = "Error: ncurses initialization failed\n";

/// Error message emitted when the terminal starts smaller than 65 × 6.
pub const ERR_WINDOW_TOO_SMALL: &str =
    "Window is too small. A size of at least 65 x 6 is required to run ccmake.\n";

/// Error message emitted when `CacheDriver::load` returns an error.
pub const ERR_LOAD_CACHE: &str = "Error running cmake::LoadCache().  Aborting.\n";

/// Placeholder label shown when no cache entries are visible.
pub const EMPTY_CACHE_LABEL: &str = "EMPTY CACHE";

/// Width reserved for the entry name column.
pub const NAME_COL_WIDTH: u16 = 30;

/// Starting column of the new-entry marker (`*`).
pub const MARKER_COL: u16 = 30;

/// Starting column of the entry value column.
pub const VALUE_COL: u16 = 33;

/// First row of the bottom 4-row key legend (browse mode).
pub const KEYS_ROW_0: &str = "Keys: [enter] Edit an entry [d] Delete an entry";

/// Second row of the bottom 4-row key legend when generation is enabled (`OkToGenerate`).
pub const KEYS_ROW_1_GENERATE: &str =
    "      [l] Show log output   [c] Configure       [g] Generate        ";

/// Second row of the bottom 4-row key legend when generation is disabled.
pub const KEYS_ROW_1_NO_GENERATE: &str =
    "      [l] Show log output   [c] Configure                           ";

/// Third row of the bottom 4-row key legend.
pub const KEYS_ROW_2: &str = "      [h] Help              [q] Quit without generating";

/// Fourth row of the bottom 4-row key legend when advanced mode is off.
pub const KEYS_ROW_3_ADV_OFF: &str = "      [t] Toggle advanced mode (currently off)";

/// Fourth row of the bottom 4-row key legend when advanced mode is on.
pub const KEYS_ROW_3_ADV_ON: &str = "      [t] Toggle advanced mode (currently on)";

/// Edit-mode key hint line 1 (row `rows - 3`).
pub const EDIT_KEYS_ROW_1: &str = "Editing option, press [enter] to confirm";

/// Edit-mode key hint line 2 (row `rows - 2`).
pub const EDIT_KEYS_ROW_2: &str = "                press [esc] to cancel";

/// Log / help view exit instruction (row `rows - 2`).
pub const LONG_MSG_EXIT_HINT: &str = "Press [e] to exit screen";

/// Status-bar version banner (`CMake Version 4.5.1`).
#[must_use]
pub fn version_banner() -> String {
    format!("CMake Version {CMAKE_VERSION_STR}")
}

/// Top-right page indicator (`Page N of M`).
#[must_use]
pub fn page_header(page: usize, total_pages: usize) -> String {
    format!("Page {page} of {total_pages}")
}

/// Number of cache-entry rows that fit on a single page for terminal height `rows`.
///
/// Reserves row 0 for the page header, row `rows - 5` for the status bar, rows
/// `rows - 4..rows` for the 4-row key legend, and (when `rows >= 8`) one blank
/// separator row between entries and the status bar — yielding `rows - 7` on
/// standard terminals (e.g., 17 entries per page on 80 × 24), matching upstream.
#[must_use]
pub const fn entries_per_page(rows: u16) -> usize {
    if rows >= 8 {
        (rows - 7) as usize
    } else if rows >= 7 {
        1
    } else {
        0
    }
}

/// Bottom 4 key-legend lines for browse mode.
#[must_use]
pub const fn browse_key_lines(ok_to_generate: bool, advanced_mode: bool) -> [&'static str; 4] {
    let row1 = if ok_to_generate {
        KEYS_ROW_1_GENERATE
    } else {
        KEYS_ROW_1_NO_GENERATE
    };
    let row3 = if advanced_mode {
        KEYS_ROW_3_ADV_ON
    } else {
        KEYS_ROW_3_ADV_OFF
    };
    [KEYS_ROW_0, row1, KEYS_ROW_2, row3]
}
