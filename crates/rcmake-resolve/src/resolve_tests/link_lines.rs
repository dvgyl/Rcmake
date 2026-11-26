use super::*;

fn archive_commands(
    generator: rcmake_model::GeneratorKind,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut model = project();
    model.generator = Some(generator);
    let host = PureHost::default();
    let sources: Vec<String> = (0..400)
        .map(|index| format!("{}_{index:03}.c", "long_source_name".repeat(5)))
        .collect();
    for source in &sources {
        host.seed_file(Path::new(&format!("/src/{source}")), b"")?;
    }
    let names: Vec<&str> = sources.iter().map(String::as_str).collect();
    model.add_target(target("big", TargetKind::StaticLibrary, &names))?;
    let result = resolve(&model, &host, &[])?;
    let BuildStep::Link { invocations, .. } = link_build_step(&result.graphs[0], "big")? else {
        return Err("Expected an archive step.".into());
    };
    Ok(invocations
        .iter()
        .map(|invocation| {
            String::from_utf8_lossy(if invocation.shell.is_empty() {
                invocation
                    .argv
                    .first()
                    .map_or(&[][..], |arg| arg.as_slice())
            } else {
                &invocation.shell
            })
            .into_owned()
        })
        .collect())
}

#[test]
fn ninja_archives_use_one_create_command_and_makefiles_split() -> TestResult {
    let appends = |commands: &[String]| {
        commands
            .iter()
            .filter(|command| command.contains(" q ") || command.ends_with(" q"))
            .count()
    };
    let ninja = archive_commands(rcmake_model::GeneratorKind::Ninja)?;
    assert_eq!(appends(&ninja), 0, "{ninja:?}");
    assert_eq!(
        ninja
            .iter()
            .filter(|command| command.contains(" qc "))
            .count(),
        1
    );
    let make = archive_commands(rcmake_model::GeneratorKind::UnixMakefiles)?;
    assert!(appends(&make) > 0, "{make:?}");
    Ok(())
}

#[test]
fn visibility_preset_uses_the_value_copied_at_target_creation() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    for (key, value) in [
        ("CMAKE_C_COMPILE_OPTIONS_VISIBILITY", "-fvisibility="),
        ("CMAKE_C_VISIBILITY_PRESET", "hidden"),
    ] {
        model.global.props.set(key.into(), value.into());
    }
    model.add_target(target("early", TargetKind::StaticLibrary, &["a.c"]))?;
    let mut late = target("late", TargetKind::StaticLibrary, &["a.c"]);
    late.props
        .set("C_VISIBILITY_PRESET".into(), "hidden".into());
    model.add_target(late)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let flags = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        let graph = &result.graphs[0];
        let target = graph
            .targets
            .iter()
            .find(|entry| entry.name == name.as_bytes())
            .ok_or("Missing target")?;
        target
            .steps
            .iter()
            .find_map(|index| match graph.steps.get(*index) {
                Some(BuildStep::Compile { invocation, .. }) => Some(
                    String::from_utf8_lossy(
                        &[invocation.shell.as_slice(), &invocation.argv.concat()].concat(),
                    )
                    .into_owned(),
                ),
                _ => None,
            })
            .ok_or_else(|| "Missing compile step".into())
    };
    assert!(!flags("early")?.contains("-fvisibility"));
    assert!(flags("late")?.contains("-fvisibility=hidden"));
    Ok(())
}

#[test]
fn directory_define_flags_follow_the_color_flags() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    model.dir_mut(DirId::ROOT).define_flags = " -D THREAD -fno-common".into();
    model.add_target(target("lib", TargetKind::StaticLibrary, &["a.c"]))?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/a.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    let command = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(invocation.shell.clone()),
            _ => None,
        })
        .ok_or("Missing compile step")?;
    let command = String::from_utf8_lossy(&command).into_owned();
    assert!(command.contains("-D THREAD -fno-common"), "{command}");
    Ok(())
}

#[test]
fn ninja_versioned_executable_symlink_is_a_separate_edge() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let mut app = target("tool", TargetKind::Executable, &["main.c"]);
    app.props.set("VERSION".into(), "2.0".into());
    model.add_target(app)?;
    let host = PureHost::default();
    host.seed_file(Path::new("/src/main.c"), b"")?;
    let result = resolve(&model, &host, &[])?;
    let graph = &result.graphs[0];
    let BuildStep::Link {
        invocations, step, ..
    } = link_build_step(graph, "tool")?
    else {
        return Err("Expected a link step.".into());
    };
    assert!(
        invocations
            .iter()
            .all(|invocation| invocation.argv.get(2) != Some(&"cmake_symlink_executable".into()))
    );
    let symlink = graph
        .steps
        .iter()
        .find_map(|candidate| match candidate {
            BuildStep::Custom(custom)
                if custom
                    .comment
                    .as_ref()
                    .is_some_and(|comment| comment.starts_with(b"Creating executable symlink")) =>
            {
                Some(custom)
            }
            _ => None,
        })
        .ok_or("Expected the executable symlink edge.")?;
    assert_eq!(symlink.outputs, ["tool"]);
    assert_eq!(symlink.depends, std::slice::from_ref(&step.output));
    Ok(())
}

#[test]
fn private_shared_dependency_must_follow_the_library_that_lists_it() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    model.add_target(target("leaf", TargetKind::SharedLibrary, &["l.c"]))?;
    let mut middle = target("mid", TargetKind::SharedLibrary, &["l.c"]);
    middle.usage.link_libraries = Some(vec![link_item("leaf")]);
    model.add_target(middle)?;
    let mut app = target("app", TargetKind::Executable, &["m.c"]);
    app.usage.link_libraries = Some(vec![link_item("leaf"), link_item("mid")]);
    model.add_target(app)?;
    let host = PureHost::default();
    for file in ["/src/l.c", "/src/m.c"] {
        host.seed_file(Path::new(file), b"")?;
    }
    let result = resolve(&model, &host, &[])?;
    let app = result
        .targets
        .iter()
        .find(|target| target.metadata.name == "app")
        .ok_or("Missing app")?;
    let libraries: Vec<_> = app
        .link
        .as_ref()
        .ok_or("Missing link step")?
        .fragments
        .iter()
        .filter(|fragment| fragment.role == LinkFragmentRole::Libraries)
        .map(|fragment| String::from_utf8_lossy(&fragment.text.value).into_owned())
        .collect();
    assert_eq!(libraries, ["libmid.dylib", "libleaf.dylib"]);
    Ok(())
}
