//! Scripted end-to-end integration tests for `rcmake-ccmake` TUI behavior.

use std::collections::BTreeSet;

use rcmake_base::BString;
use rcmake_ccmake::layout::{
    ERR_LOAD_CACHE, ERR_WINDOW_TOO_SMALL, KEYS_ROW_0, KEYS_ROW_1_GENERATE, KEYS_ROW_1_NO_GENERATE,
    KEYS_ROW_2, KEYS_ROW_3_ADV_OFF, KEYS_ROW_3_ADV_ON,
};
use rcmake_ccmake::model::snapshot_entry;
use rcmake_ccmake::terminal::scripted::ScriptedTerminal;
use rcmake_ccmake::{
    CacheDriver, CacheSnapshot, DirItem, DriverError, EntryEdit, LogLine, RunReport, run,
};
use rcmake_model::CacheType;

struct FakeDriver {
    snapshot: CacheSnapshot,
    fail_load: bool,
    configure_calls: Vec<Vec<EntryEdit>>,
    generate_calls: usize,
    configure_responses: Vec<(RunReport, Option<CacheSnapshot>)>,
    generate_report: RunReport,
    dir_entries: Vec<DirItem>,
    invalidations: usize,
}

impl FakeDriver {
    fn new(snapshot: CacheSnapshot) -> Self {
        Self {
            snapshot,
            fail_load: false,
            configure_calls: Vec::new(),
            generate_calls: 0,
            configure_responses: Vec::new(),
            generate_report: RunReport::default(),
            dir_entries: Vec::new(),
            invalidations: 0,
        }
    }
}

impl CacheDriver for FakeDriver {
    fn load(&mut self) -> Result<CacheSnapshot, DriverError> {
        if self.fail_load {
            return Err(DriverError::new("corrupt cache"));
        }
        Ok(self.snapshot.clone())
    }

    fn configure(&mut self, edits: &[EntryEdit]) -> RunReport {
        self.configure_calls.push(edits.to_vec());
        if !self.configure_responses.is_empty() {
            let (report, next) = self.configure_responses.remove(0);
            if let Some(snap) = next {
                self.snapshot = snap;
            }
            return report;
        }
        for edit in edits {
            match edit {
                EntryEdit::Set { name, value, .. } => {
                    if let Some(entry) = self.snapshot.entries.iter_mut().find(|e| e.name == *name)
                    {
                        entry.value = value.clone();
                    }
                }
                EntryEdit::Remove { name } => {
                    self.snapshot.entries.retain(|e| e.name != *name);
                }
            }
        }
        RunReport::default()
    }

    fn generate(&mut self) -> RunReport {
        self.generate_calls += 1;
        self.generate_report.clone()
    }

    fn invalidate(&mut self) {
        self.invalidations += 1;
    }

    fn list_dir(&mut self, _dir: &[u8]) -> Vec<DirItem> {
        self.dir_entries.clone()
    }
}

fn base_snapshot() -> CacheSnapshot {
    CacheSnapshot {
        entries: vec![
            snapshot_entry(
                "ALPHA",
                CacheType::Bool,
                "OFF",
                "Alpha option",
                false,
                vec![],
            ),
            snapshot_entry(
                "FOO_BAR",
                CacheType::String,
                "hello",
                "Foo string",
                false,
                vec![],
            ),
            snapshot_entry(
                "ADV_SECRET",
                CacheType::String,
                "hidden",
                "Advanced entry",
                true,
                vec![],
            ),
            snapshot_entry(
                "INTERNAL_ONLY",
                CacheType::Internal,
                "internal",
                "",
                false,
                vec![],
            ),
        ],
    }
}

fn assert_bottom_four_rows(term: &ScriptedTerminal, ok_to_generate: bool, advanced: bool) {
    let row20 = term.row_text(20);
    let row21 = term.row_text(21);
    let row22 = term.row_text(22);
    let row23 = term.row_text(23);

    assert!(row20.starts_with(KEYS_ROW_0), "row 20: {row20:?}");
    let expected_row1 = if ok_to_generate {
        KEYS_ROW_1_GENERATE
    } else {
        KEYS_ROW_1_NO_GENERATE
    };
    assert_eq!(
        row21.trim_end(),
        expected_row1.trim_end(),
        "row 21: {row21:?}"
    );
    assert_eq!(row22.trim_end(), KEYS_ROW_2, "row 22: {row22:?}");
    let expected_row3 = if advanced {
        KEYS_ROW_3_ADV_ON
    } else {
        KEYS_ROW_3_ADV_OFF
    };
    assert_eq!(row23.trim_end(), expected_row3, "row 23: {row23:?}");
}

#[test]
fn toggle_advanced_shows_advanced_entries_and_updates_legend() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"tq");
    let mut driver = FakeDriver::new(base_snapshot());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert!(stderr.is_empty());
    assert!(!term.is_raw());
    assert_eq!(term.enter_count(), 1);
    assert_eq!(term.leave_count(), 1);
    assert_bottom_four_rows(&term, false, true);
    let joined = term.screen_lines().join("\n");
    assert!(joined.contains("ADV_SECRET"), "{joined}");
    assert!(!joined.contains("INTERNAL_ONLY"), "{joined}");
}

#[test]
fn search_selects_foo_bar_and_enter_toggles_bool() {
    let mut term = ScriptedTerminal::new(80, 24);
    // Start on ALPHA (BOOL=OFF), toggle with \r -> ON, then search /FOO\n -> FOO_BAR, configure, quit.
    term.push_bytes(b"\r/FOO\ncq");
    let mut driver = FakeDriver::new(base_snapshot());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert_eq!(driver.configure_calls.len(), 1);
    assert_eq!(
        driver.configure_calls[0],
        vec![EntryEdit::Set {
            name: BString::from("ALPHA"),
            ty: CacheType::Bool,
            value: BString::from("ON"),
        }]
    );
    // Status bar row 19 shows FOO_BAR selected.
    assert!(term.row_text(19).starts_with("FOO_BAR: Foo string"));
    assert_bottom_four_rows(&term, true, false);
}

#[test]
fn delete_then_configure_sends_remove_edit() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"dcq");
    let mut driver = FakeDriver::new(base_snapshot());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert_eq!(driver.configure_calls.len(), 1);
    assert_eq!(
        driver.configure_calls[0],
        vec![EntryEdit::Remove {
            name: BString::from("ALPHA"),
        }]
    );
    assert_bottom_four_rows(&term, true, false);
}

#[test]
fn configure_with_new_entries_hides_generate_until_second_clean_configure() {
    let mut term = ScriptedTerminal::new(80, 24);
    // First c adds NEW_OPT -> g is ignored. Second c adds nothing -> g runs and exits 0.
    term.push_bytes(b"cg");
    // Second c, then g.
    term.push_bytes(b"cg");

    let mut snap_after_first = base_snapshot();
    snap_after_first.entries.push(snapshot_entry(
        "NEW_OPT",
        CacheType::Bool,
        "ON",
        "Newly added",
        false,
        vec![],
    ));
    let mut new_set = BTreeSet::new();
    new_set.insert(BString::from("NEW_OPT"));

    let mut driver = FakeDriver::new(base_snapshot());
    driver.configure_responses = vec![
        (
            RunReport {
                exit_code: 0,
                lines: Vec::new(),
                has_non_status: false,
                new_entries: new_set,
            },
            Some(snap_after_first.clone()),
        ),
        (
            RunReport {
                exit_code: 0,
                lines: Vec::new(),
                has_non_status: false,
                new_entries: BTreeSet::new(),
            },
            Some(snap_after_first),
        ),
    ];

    let mut stderr = Vec::new();
    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert_eq!(driver.configure_calls.len(), 2);
    assert_eq!(driver.generate_calls, 1);
    assert!(!term.is_raw());
}

#[test]
fn first_configure_with_new_entries_keeps_generate_hidden_in_legend() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"cq");
    let mut snap_after = base_snapshot();
    snap_after.entries.push(snapshot_entry(
        "NEW_OPT",
        CacheType::Bool,
        "ON",
        "Newly added",
        false,
        vec![],
    ));
    let mut driver = FakeDriver::new(base_snapshot());
    driver.configure_responses = vec![(RunReport::default(), Some(snap_after))];

    let mut stderr = Vec::new();
    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert_bottom_four_rows(&term, false, false);
    assert!(term.row_text(1).starts_with("NEW_OPT"));
    assert_eq!(term.row_text(1).chars().nth(30), Some('*'));
}

#[test]
fn small_window_returns_exit_one_with_exact_message_and_restores_terminal() {
    let mut term = ScriptedTerminal::new(60, 20);
    let mut driver = FakeDriver::new(base_snapshot());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 1);
    assert_eq!(String::from_utf8_lossy(&stderr), ERR_WINDOW_TOO_SMALL);
    assert!(!term.is_raw());
    assert_eq!(term.enter_count(), 1);
    assert_eq!(term.leave_count(), 1);
    assert!(driver.configure_calls.is_empty());
}

#[test]
fn load_cache_failure_prints_exact_message_and_restores_terminal() {
    let mut term = ScriptedTerminal::new(80, 24);
    let mut driver = FakeDriver::new(base_snapshot());
    driver.fail_load = true;
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 1);
    assert_eq!(String::from_utf8_lossy(&stderr), ERR_LOAD_CACHE);
    assert!(!term.is_raw());
    assert_eq!(term.leave_count(), 1);
}

#[test]
fn quit_without_generating_returns_zero_and_sends_no_edits() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"\rq");
    let mut driver = FakeDriver::new(base_snapshot());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert!(driver.configure_calls.is_empty());
    assert_eq!(driver.generate_calls, 0);
    assert_bottom_four_rows(&term, false, false);
}

#[test]
fn empty_cache_renders_placeholder_and_ctrl_c_quits_cleanly() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"\x03");
    let mut driver = FakeDriver::new(CacheSnapshot::default());
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert!(term.row_text(1).starts_with("EMPTY CACHE"));
    assert_eq!(term.row_text(0).trim(), "Page 1 of 1");
    assert_bottom_four_rows(&term, false, false);
    assert!(!term.is_raw());
}

#[test]
fn configure_error_opens_log_view_and_dismisses_with_e() {
    let mut term = ScriptedTerminal::new(80, 24);
    term.push_bytes(b"ceq");
    let mut driver = FakeDriver::new(base_snapshot());
    driver.configure_responses = vec![(
        RunReport {
            exit_code: 1,
            lines: vec![LogLine {
                is_error: true,
                text: BString::from("CMake Error: bad config"),
            }],
            has_non_status: true,
            new_entries: BTreeSet::new(),
        },
        None,
    )];
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert!(String::from_utf8_lossy(term.output_bytes()).contains("CMake Error: bad config"));
    assert_bottom_four_rows(&term, false, false);
}

#[test]
fn path_tab_completion_uses_driver_list_dir() {
    let mut term = ScriptedTerminal::new(80, 24);
    // Select PATH entry (index 0), enter edit mode, type Tab, Enter, c, q.
    term.push_bytes(b"\r\t\rcq");
    let snap = CacheSnapshot {
        entries: vec![snapshot_entry(
            "MY_DIR",
            CacheType::Path,
            "/work/su",
            "Directory",
            false,
            vec![],
        )],
    };
    let mut driver = FakeDriver::new(snap);
    driver.dir_entries = vec![
        DirItem {
            name: BString::from("sub_file.txt"),
            is_dir: false,
        },
        DirItem {
            name: BString::from("subdir"),
            is_dir: true,
        },
    ];
    let mut stderr = Vec::new();

    assert_eq!(run(&mut term, &mut driver, &mut stderr), 0);
    assert_eq!(
        driver.configure_calls,
        vec![vec![EntryEdit::Set {
            name: BString::from("MY_DIR"),
            ty: CacheType::Path,
            value: BString::from("/work/subdir/"),
        }]]
    );
}
