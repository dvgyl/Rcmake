use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Follow, Host, ProcessResult, PureHost, RunOutput, Stream};
use rcmake_json::{ReadOptions, Value};

fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
    let argv: Vec<_> = args.iter().copied().map(BString::from).collect();
    crate::run(&argv, host.clone())
}

#[test]
fn applies_no_tests_action() {
    for (action, expected) in [("error", 8), ("ignore", 0), ("action", 1)] {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/CTestTestfile.cmake"), b"")
            .unwrap();
        assert_eq!(
            invoke(&host, &["ctest", &format!("--no-tests={action}")]),
            expected
        );
    }
}

#[test]
fn shows_tests_from_a_preset_as_json() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakePresets.json"),
        br#"{"version": 6, "configurePresets": [
          {"name": "configure", "generator": "Ninja", "binaryDir": "out"}
        ], "testPresets": [
          {"name": "tests", "configurePreset": "configure"}
        ]}"#,
    )
    .unwrap();
    host.seed_file(
        Path::new("/work/out/CTestTestfile.cmake"),
        b"add_test(example true)\n",
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["ctest", "--preset=tests", "--show-only=json-v1"]),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let stdout = host.stream_bytes(Stream::Stdout);
    let Value::Object(result) = rcmake_json::parse(&stdout, &ReadOptions::default()).unwrap()
    else {
        panic!("Expected a JSON object.");
    };
    assert_eq!(
        result.get(b"kind".as_slice()),
        Some(&Value::string("ctestInfo"))
    );
    assert!(
        stdout
            .windows(b"example".len())
            .any(|text| text == b"example")
    );
}

#[test]
fn runs_the_instrumentation_launcher() {
    let host = Arc::new(PureHost::default());
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.pipeline[0].program, "true");
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "ctest",
                "--instrument",
                "--command-type",
                "compile",
                "--",
                "true"
            ]
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let data = Path::new("/work/.cmake/instrumentation/v1/data");
    assert!(
        host.read_dir(data)
            .unwrap()
            .iter()
            .any(|entry| entry.name.starts_with(b"compile-"))
    );
    assert!(host.metadata(&data.join("v1"), Follow::Yes).is_err());
}
