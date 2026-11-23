//! `CTestTestfile.cmake` scripts for `ctest`.

use rcmake_base::BString;
use rcmake_host::{
    Host,
    path::{from_native, to_native},
};
use rcmake_model::{DirId, ProjectModel, TestEntry};

use crate::{GenError, configuration_pattern, quote, write_if_changed};

/// Render the test file for a directory. Commands and properties use the values
/// stored in the model.
pub fn render(model: &ProjectModel, dir: DirId) -> Result<BString, GenError> {
    let dir = model.dir(dir);
    let mut out = BString::from("# CMake generated Testfile for \n");
    comment(&mut out, b"# Source directory: ", &dir.source_dir);
    comment(&mut out, b"# Build directory: ", &dir.binary_dir);
    out.extend_from_slice(
        b"# \n# This file includes the relevant testing commands required for \n\
          # testing this directory and lists subdirectories to be tested as well.\n",
    );
    for test in dir.tests.values().filter_map(|&id| model.test(id)) {
        render_test(&mut out, test);
    }
    let binary_dir = to_native(&dir.binary_dir);
    for &child in &dir.children {
        let path = to_native(&model.dir(child).binary_dir);
        let path = path.strip_prefix(&binary_dir).unwrap_or(&path);
        out.extend_from_slice(b"subdirs(");
        test_argument(&mut out, &from_native(path));
        out.extend_from_slice(b")\n");
    }
    Ok(out)
}

/// Write the test file to the directory's existing binary directory.
pub fn write_ctest_file(model: &ProjectModel, dir: DirId, host: &dyn Host) -> Result<(), GenError> {
    let content = render(model, dir)?;
    let path = to_native(&model.dir(dir).binary_dir).join("CTestTestfile.cmake");
    write_if_changed(host, &path, &content)
}

fn comment(out: &mut BString, label: &[u8], value: &[u8]) {
    out.extend_from_slice(label);
    for &byte in value {
        out.push(if matches!(byte, b'\n' | b'\r') {
            b' '
        } else {
            byte
        });
    }
    out.push(b'\n');
}

fn render_test(out: &mut BString, test: &TestEntry) {
    let mut indent = b"".as_slice();
    if !test.configurations.is_empty() {
        out.extend_from_slice(b"if(CTEST_CONFIGURATION_TYPE MATCHES ");
        quote(out, &configuration_pattern(&test.configurations));
        out.extend_from_slice(b")\n");
        indent = b"  ";
    }
    out.extend_from_slice(indent);
    out.extend_from_slice(b"add_test(");
    test_argument(out, &test.name);
    for arg in &test.command {
        out.push(b' ');
        test_argument(out, arg);
    }
    out.extend_from_slice(b")\n");
    out.extend_from_slice(indent);
    out.extend_from_slice(b"set_tests_properties(");
    test_argument(out, &test.name);
    out.extend_from_slice(b" PROPERTIES ");

    let mut names = test.props.names();
    if test.working_directory.is_some() && test.props.get(b"WORKING_DIRECTORY").is_none() {
        names.push("WORKING_DIRECTORY".into());
    }
    names.sort_unstable();
    for name in names {
        if name == b"_BACKTRACE_TRIPLES" {
            continue;
        }
        let value = test.props.get(&name).or(test.working_directory.as_ref());
        if let Some(value) = value {
            out.push(b' ');
            test_argument(out, &name);
            out.push(b' ');
            test_argument(out, value);
        }
    }

    out.extend_from_slice(b" _BACKTRACE_TRIPLES ");
    let mut triples = BString::default();
    for (index, frame) in test.backtrace.frames().enumerate() {
        if index != 0 {
            triples.push(b';');
        }
        triples.extend_from_slice(&frame.file);
        triples.push(b';');
        triples.extend_from_slice(frame.line.to_string().as_bytes());
        triples.push(b';');
        if let Some(name) = &frame.name {
            triples.extend_from_slice(name);
        }
    }
    test_argument(out, &triples);
    out.extend_from_slice(b")\n");

    if !indent.is_empty() {
        out.extend_from_slice(b"endif()\n");
    }
}

fn test_argument(out: &mut BString, value: &[u8]) {
    if !value
        .iter()
        .any(|&byte| matches!(byte, b'"' | b'\\' | b'$'))
    {
        quote(out, value);
        return;
    }
    // Leave room for any closing delimiter inside the argument or at its end.
    let width = value
        .split(|&byte| byte == b']')
        .skip(1)
        .map(|part| part.iter().take_while(|&&byte| byte == b'=').count() + 1)
        .max()
        .unwrap_or_default();
    let delimiter = std::iter::repeat_n(b'=', width);
    out.push(b'[');
    out.extend(delimiter.clone());
    out.push(b'[');
    if value.starts_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(value);
    out.push(b']');
    out.extend(delimiter);
    out.push(b']');
}
