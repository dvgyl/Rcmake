//! Generation diagnostics that follow the CMake 4.5.1 RunCMake suites.

use super::*;
use rcmake_host::Stream;

fn fixture() -> Result<(ProjectModel, PureHost), Box<dyn std::error::Error>> {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let host = PureHost::new(Path::new(root).to_path_buf(), Path::new(root).join("cmake"));
    let source = format!("{root}/src");
    host.seed_file(Path::new(&format!("{source}/main.c")), b"")?;
    let model = project_with_roots(&source, &format!("{root}/build"));
    Ok((model, host))
}

fn stderr(host: &PureHost) -> String {
    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
}

#[test]
fn own_relative_include_directory_names_the_consumer() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage
        .push(UsageField::IncludeDirectories, entry("relative"));
    model.add_target(app)?;
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(
        text.contains("Found relative path while evaluating include directories of \"app\":"),
        "{text}"
    );
    assert!(text.contains("\"relative\""), "{text}");
    Ok(())
}

#[test]
fn evaluation_errors_of_each_target_are_reported() -> TestResult {
    let (mut model, host) = fixture()?;
    for name in ["first", "second"] {
        let mut lib = target(name, TargetKind::StaticLibrary, &["main.c"]);
        lib.usage.push(
            UsageField::IncludeDirectories,
            entry(&format!(
                "$<TARGET_PROPERTY:{name}_missing/x,INCLUDE_DIRECTORIES>"
            )),
        );
        model.add_target(lib)?;
    }
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(text.contains("first_missing/x"), "{text}");
    assert!(text.contains("second_missing/x"), "{text}");
    Ok(())
}

#[test]
fn invalid_language_standard_is_an_error_at_the_target() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.props.set("C_STANDARD".into(), "bad".into());
    model.add_target(app)?;
    let error = resolve(&model, &host, &[]).expect_err("The standard is invalid.");
    let text = error.to_string();
    assert!(text.starts_with("CMake Error at "), "{text}");
    assert!(
        text.contains("C_STANDARD is set to invalid value 'bad'"),
        "{text}"
    );
    Ok(())
}
