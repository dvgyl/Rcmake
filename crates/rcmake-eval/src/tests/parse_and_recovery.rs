use super::{new_interp, run, run_configure};
use crate::RunMode;
use rcmake_host::Stream;
use std::path::PathBuf;

#[test]
fn not_a_file_and_parse_errors() {
    let (mut interp, host) = new_interp(&[], RunMode::Script);
    let outcome = interp.run_script(&PathBuf::from("/work/nope.cmake"));
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "CMake Error: Not a file: /work/nope.cmake\nCMake Error: Error processing file: /work/nope.cmake\n"
    );

    let o = run("message(a)\nendif()\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:2 (endif):\n",
            "  Flow control statements are not properly nested.\n\n\n",
            "CMake Error: Error processing file: /work/main.cmake\n",
        )
    );
}

#[test]
fn failed_while_runs_body_and_reports_endwhile() {
    let o = run_configure("while()\nmessage(inside)\nendwhile()\n", &[]);
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at CMakeLists.txt:1 (while):\n",
            "  while called with incorrect number of arguments\n\n\n",
            "inside\n",
            "CMake Error at CMakeLists.txt:3 (endwhile):\n",
            "  endwhile An ENDWHILE command was found outside of a proper WHILE ENDWHILE\n",
            "  structure.  Or its arguments did not match the opening WHILE command.\n\n\n",
        )
    );
}

#[test]
fn failed_function_definition_runs_body_inline() {
    let o = run_configure("function()\nmessage(body)\nendfunction()\n", &[]);
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("function called with incorrect number of arguments")
    );
    assert!(o.stderr.contains("\nbody\n"), "{}", o.stderr);
    assert!(
        o.stderr
            .contains("An ENDFUNCTION command was found outside")
    );
}

#[test]
fn nesting_error_stops_processing_without_include_error() {
    let o = run_configure(
        "include(bad.cmake)\nmessage(after)\n",
        &[("/work/src/bad.cmake", "while(a)\n")],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at bad.cmake:1 (while):\n",
            "  Flow control statements are not properly nested.\n",
            "Call Stack (most recent call first):\n",
            "  CMakeLists.txt:1 (include)\n\n\n",
        )
    );
}

#[test]
fn missing_close_paren_at_end_of_file_is_not_fatal() {
    let o = run_configure(
        "include(bad.cmake)\nmessage(after)\n",
        &[("/work/src/bad.cmake", "message(a\n")],
    );
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("Function missing ending \")\".  End of file reached.")
    );
    assert!(o.stderr.contains("include could not load requested file:"));
    assert!(o.stderr.ends_with("after\n"), "{}", o.stderr);
}

#[test]
fn non_utf8_bom_is_a_file_level_error() {
    let (mut interp, host) = new_interp(
        &[(
            "/work/src/CMakeLists.txt",
            "include(bom.cmake)\nmessage(after)\n",
        )],
        RunMode::Configure,
    );
    host.seed_file(&PathBuf::from("/work/src/bom.cmake"), b"\xff\xfem\x00")
        .expect("seed file");
    let code = interp
        .run_configure(
            &PathBuf::from("/work/src"),
            &PathBuf::from("/work/build"),
            &[],
        )
        .exit_code;
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        concat!(
            "CMake Error in bom.cmake:\n",
            "  File starts with a Byte-Order-Mark that is not UTF-8.\n",
            "Call Stack (most recent call first):\n",
            "  CMakeLists.txt:1 (include)\n\n\n",
            "CMake Error at CMakeLists.txt:1 (include):\n",
            "  include could not load requested file:\n\n",
            "    bom.cmake\n\n\n",
            "after\n",
        )
    );
}

#[test]
fn expansion_error_is_fatal_and_names_the_argument() {
    let o = run("message(ok \"a\\q\" b)\nmessage(after)\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:1 (message):\n",
            "  Syntax error in cmake code at\n\n",
            "    /work/main.cmake:1\n\n",
            "  when parsing string\n\n",
            "    a\\q\n\n",
            "  Invalid character escape '\\q'.\n\n\n",
        )
    );
}
