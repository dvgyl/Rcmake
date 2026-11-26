use std::error::Error;
use std::path::PathBuf;

use rcmake_diag::{Backtrace, Frame, FrameKind};

use super::*;
use crate::journal::codec::round_trip;

type TestResult = Result<(), Box<dyn Error>>;

fn bin() -> PathBuf {
    PathBuf::from(if cfg!(windows) {
        "C:/work/build"
    } else {
        "/work/build"
    })
}

fn origin(line: u32) -> PropertyOrigin {
    let backtrace = Backtrace::default().push(Frame {
        file: b"/work/CMakeLists.txt".as_slice().into(),
        line,
        name: Some(b"add_library".as_slice().into()),
        kind: FrameKind::Command,
    });
    PropertyOrigin::new(bin(), &backtrace)
}

fn value(text: &[u8]) -> Value {
    Value(text.to_vec(), origin(1))
}

fn usage() -> Usage {
    Usage {
        fields: vec![
            (
                "INCLUDE_DIRECTORIES".into(),
                vec![value(b"/work/include"), value(b"\xff-raw")],
            ),
            ("COMPILE_OPTIONS".into(), Vec::new()),
        ],
        links: Some(vec![
            Link(value(b"dep"), Some(1)),
            Link(value(b"other"), None),
        ]),
        interface_links: None,
    }
}

fn target() -> TargetRecord {
    TargetRecord {
        name: b"lib".to_vec(),
        origin: origin(2),
        props: vec![
            (b"OUTPUT_NAME".to_vec(), b"out\xff".to_vec()),
            (b"EMPTY".to_vec(), Vec::new()),
        ],
        usage: usage(),
        sources: vec![value(b"a.c"), value("é.c".as_bytes())],
    }
}

fn test_record(working_directory: Option<Vec<u8>>) -> TestRecord {
    TestRecord {
        name: b"unit".to_vec(),
        origin: origin(3),
        command: vec![b"run".to_vec(), b"--flag=\xff".to_vec()],
        configurations: vec![b"Debug".to_vec()],
        working_directory,
        command_expand_lists: true,
        old_style: false,
        build_depends: vec![value(b"lib")],
    }
}

#[test]
fn leaf_records_round_trip() -> TestResult {
    round_trip(&value(b"\0\x01"))?;
    round_trip(&Link(value(b"dep"), Some(2)))?;
    round_trip(&Link(value(b"dep"), None))?;
    round_trip(&usage())?;
    round_trip(&Usage {
        fields: Vec::new(),
        links: None,
        interface_links: Some(Vec::new()),
    })?;
    round_trip(&target())?;
    round_trip(&test_record(Some(b"/work/run".to_vec())))?;
    round_trip(&test_record(None))?;
    let full = LocalState {
        variable_inputs: Some((3, JournalDigest::hash(b"inputs"))),
        targets: vec![target()],
        aliases: vec![(b"alias".to_vec(), b"lib".to_vec())],
        tests: vec![test_record(None)],
        installs: None,
        link_check: Some(origin(4)),
        directory_usage: Some(usage()),
        used_cli: vec![b"CLI_VAR".to_vec(), b"\xff".to_vec()],
    };
    let decoded = round_trip(&full)?;
    assert_eq!(decoded.variable_inputs, full.variable_inputs);
    round_trip(&LocalState {
        variable_inputs: None,
        targets: Vec::new(),
        aliases: Vec::new(),
        tests: Vec::new(),
        installs: None,
        link_check: None,
        directory_usage: None,
        used_cli: Vec::new(),
    })?;
    Ok(())
}

#[test]
fn usage_digest_covers_values_links_and_scopes() {
    let base = usage();
    assert_eq!(base.digest(), usage().digest());
    let mut changed_scope = usage();
    changed_scope.links = Some(vec![
        Link(value(b"dep"), Some(2)),
        Link(value(b"other"), None),
    ]);
    assert_ne!(base.digest(), changed_scope.digest());
    let mut moved = usage();
    moved.interface_links = moved.links.take();
    assert_ne!(base.digest(), moved.digest());
    let mut empty_links = usage();
    empty_links.interface_links = Some(Vec::new());
    assert_ne!(base.digest(), empty_links.digest());
    let mut changed_origin = usage();
    changed_origin.fields = vec![
        (
            "INCLUDE_DIRECTORIES".into(),
            vec![
                Value(b"/work/include".to_vec(), origin(9)),
                value(b"\xff-raw"),
            ],
        ),
        ("COMPILE_OPTIONS".into(), Vec::new()),
    ];
    assert_ne!(base.digest(), changed_origin.digest());
}
