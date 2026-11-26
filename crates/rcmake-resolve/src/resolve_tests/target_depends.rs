use super::*;

fn resolve_with_sources(
    model: &ProjectModel,
    sources: &[&str],
) -> Result<GenerationResult, Box<dyn std::error::Error>> {
    let host = PureHost::default();
    for source in sources {
        host.seed_file(Path::new(&format!("/src/{source}")), b"")?;
    }
    Ok(resolve(model, &host, &[])?)
}

fn depends(result: &GenerationResult, id: TargetId) -> Vec<TargetId> {
    result
        .target_depends
        .direct_depends(id)
        .iter()
        .map(|depend| depend.target)
        .collect()
}

fn static_library(name: &str, source: &str, links: &[&str]) -> Target {
    let mut library = target(name, TargetKind::StaticLibrary, &[source]);
    library.usage.link_libraries = Some(links.iter().map(|name| link_item(name)).collect());
    library
}

#[test]
fn static_library_cycle_becomes_a_linear_chain() -> TestResult {
    let mut model = project();
    let a = model.add_target(static_library("a", "a.c", &["b"]))?;
    let b = model.add_target(static_library("b", "b.c", &["c"]))?;
    let c = model.add_target(static_library("c", "c.c", &["a"]))?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("a")]);
    let app = model.add_target(app)?;
    let result = resolve_with_sources(&model, &["a.c", "b.c", "c.c", "main.c"])?;
    assert_eq!(result.target_depends.targets(), [a, b, c, app]);
    assert_eq!(depends(&result, app), [a]);
    assert_eq!(depends(&result, a), [b]);
    assert_eq!(depends(&result, b), [c]);
    assert!(depends(&result, c).is_empty());
    let resolved = result
        .targets
        .iter()
        .find(|target| target.id == app)
        .ok_or("Missing app")?;
    assert_eq!(resolved.dependencies, [a]);
    Ok(())
}

#[test]
fn link_interfaces_follow_static_but_not_private_shared_dependencies() -> TestResult {
    let mut model = project();
    let leaf = model.add_target(static_library("leaf", "leaf.c", &[]))?;
    let mut shared = target("shared", TargetKind::SharedLibrary, &["shared.c"]);
    shared.usage.link_libraries = Some(vec![link_item("leaf")]);
    let shared = model.add_target(shared)?;
    let mut middle = static_library("middle", "middle.c", &["leaf"]);
    middle.usage.interface_link_libraries = Some(vec![link_item("$<LINK_ONLY:leaf>")]);
    let middle = model.add_target(middle)?;
    let mut uses_shared = target("uses_shared", TargetKind::Executable, &["main.c"]);
    uses_shared.usage.link_libraries = Some(vec![link_item("shared")]);
    let uses_shared = model.add_target(uses_shared)?;
    let mut uses_static = target("uses_static", TargetKind::Executable, &["main.c"]);
    uses_static.usage.link_libraries = Some(vec![link_item("middle")]);
    let uses_static = model.add_target(uses_static)?;
    let result = resolve_with_sources(&model, &["leaf.c", "shared.c", "middle.c", "main.c"])?;
    assert_eq!(depends(&result, uses_shared), [shared]);
    assert_eq!(depends(&result, shared), [leaf]);
    assert_eq!(depends(&result, uses_static), [middle, leaf]);
    assert!(
        result
            .target_depends
            .direct_depends(uses_static)
            .iter()
            .all(|depend| !depend.strong)
    );
    Ok(())
}

#[test]
fn object_references_and_interface_utilities_become_strong_edges() -> TestResult {
    let mut model = project();
    let objects = model.add_target(target("objects", TargetKind::ObjectLibrary, &["o.c"]))?;
    let prepare = model.add_target(target("prepare", TargetKind::Utility, &[]))?;
    let mut headers = target("headers", TargetKind::InterfaceLibrary, &[]);
    headers.utility_deps.push(entry("prepare"));
    model.add_target(headers)?;
    let mut app = target(
        "app",
        TargetKind::Executable,
        &["main.c", "$<TARGET_OBJECTS:objects>"],
    );
    app.usage.link_libraries = Some(vec![link_item("headers")]);
    let app = model.add_target(app)?;
    let result = resolve_with_sources(&model, &["o.c", "main.c"])?;
    let mut direct: Vec<_> = result
        .target_depends
        .direct_depends(app)
        .iter()
        .map(|depend| (depend.target, depend.strong))
        .collect();
    direct.sort();
    assert_eq!(direct, [(objects, true), (prepare, true)]);
    let utilities: Vec<_> = result
        .target_depends
        .utility_items(app)
        .iter()
        .map(|item| item.target)
        .collect();
    assert_eq!(utilities, [objects]);
    Ok(())
}

#[test]
fn imported_target_without_location_has_no_link_interface() -> TestResult {
    let mut model = project();
    let dep = model.add_target(static_library("dep", "dep.c", &[]))?;
    for (name, location) in [("missing", None), ("found", Some("/pkg/libfound.a"))] {
        let mut imported = target(name, TargetKind::StaticLibrary, &[]);
        imported.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        imported.usage.interface_link_libraries = Some(vec![link_item("dep")]);
        if let Some(location) = location {
            imported
                .props
                .set("IMPORTED_LOCATION".into(), location.into());
        }
        model.add_target(imported)?;
    }
    let mut ids = Vec::new();
    for (name, link) in [("uses_missing", "missing"), ("uses_found", "found")] {
        let mut app = target(name, TargetKind::Executable, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item(link)]);
        ids.push(model.add_target(app)?);
    }
    let result = resolve_with_sources(&model, &["dep.c", "main.c"])?;
    assert!(depends(&result, ids[0]).is_empty());
    assert_eq!(depends(&result, ids[1]), [dep]);
    Ok(())
}

#[test]
fn deep_strong_chain_inside_a_static_cycle_links_without_recursion() -> TestResult {
    const COUNT: usize = 2000;
    let mut model = project();
    let mut ids = Vec::with_capacity(COUNT);
    for index in 0..COUNT {
        let mut library = target(&format!("s{index}"), TargetKind::StaticLibrary, &["s.c"]);
        if index == 0 {
            library.usage.link_libraries = Some(vec![link_item(&format!("s{}", COUNT - 1))]);
        } else {
            library.utility_deps.push(entry(&format!("s{}", index - 1)));
        }
        ids.push(model.add_target(library)?);
    }
    let result = resolve_with_sources(&model, &["s.c"])?;
    // The walk from the last member reaches `s0` first, so `s0` is the tail.
    assert!(depends(&result, ids[0]).is_empty());
    for pair in ids.windows(2) {
        let [previous, next] = pair else {
            continue;
        };
        assert_eq!(depends(&result, *next), [*previous]);
    }
    Ok(())
}

#[test]
fn cycle_across_configurations_is_an_error() -> TestResult {
    let mut model = project();
    let mut a = target("a", TargetKind::SharedLibrary, &["a.c"]);
    a.usage.link_libraries = Some(vec![link_item("$<$<CONFIG:Debug>:b>")]);
    model.add_target(a)?;
    let mut b = target("b", TargetKind::SharedLibrary, &["b.c"]);
    b.usage.link_libraries = Some(vec![link_item("$<$<CONFIG:Release>:a>")]);
    model.add_target(b)?;
    let host = PureHost::default();
    for source in ["a.c", "b.c"] {
        host.seed_file(Path::new(&format!("/src/{source}")), b"")?;
    }
    let single = resolve(&model, &host, &[ConfigId("Debug".into())]);
    assert!(single.is_ok());
    let configs = [ConfigId("Debug".into()), ConfigId("Release".into())];
    let Err(error) = resolve(&model, &host, &configs) else {
        return Err("expected a dependency cycle".into());
    };
    let message = error.to_string();
    assert!(message.contains("strongly connected component (cycle)"));
    assert!(message.contains("\"a\" of type SHARED_LIBRARY\n    depends on \"b\" (weak)"));
    assert!(message.contains("\"b\" of type SHARED_LIBRARY\n    depends on \"a\" (weak)"));
    assert!(message.ends_with("Cyclic dependencies are allowed only among static libraries."));
    Ok(())
}

#[test]
fn strong_cycle_inside_a_static_component_is_an_error() -> TestResult {
    let mut model = project();
    let mut a = static_library("a", "a.c", &["b"]);
    a.utility_deps.push(entry("b"));
    model.add_target(a)?;
    let mut b = static_library("b", "b.c", &[]);
    b.utility_deps.push(entry("a"));
    model.add_target(b)?;
    let Err(error) = resolve_with_sources(&model, &["a.c", "b.c"]) else {
        return Err("expected a strong dependency cycle".into());
    };
    assert!(
        error
            .to_string()
            .contains("cycle consisting of strong dependencies")
    );
    Ok(())
}

#[test]
fn imported_target_utilities_propagate_to_linking_targets() -> TestResult {
    let mut model = project();
    let helper = model.add_target(target("helper", TargetKind::Utility, &[]))?;
    let mut imported = target("imported", TargetKind::InterfaceLibrary, &[]);
    imported.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    imported.utility_deps.push(entry("helper"));
    let imported = model.add_target(imported)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("imported")]);
    let app = model.add_target(app)?;
    let result = resolve_with_sources(&model, &["main.c"])?;
    assert!(!result.target_depends.targets().contains(&imported));
    let utilities: Vec<_> = result
        .target_depends
        .utility_items(imported)
        .iter()
        .map(|item| item.target)
        .collect();
    assert_eq!(utilities, [helper]);
    let direct: Vec<_> = result
        .target_depends
        .direct_depends(app)
        .iter()
        .map(|depend| (depend.target, depend.strong))
        .collect();
    assert_eq!(direct, [(helper, true)]);
    Ok(())
}

fn command(kind: rcmake_model::CustomCommandKind) -> rcmake_model::CustomCommand {
    let mut command =
        rcmake_model::CustomCommand::new(DirId::ROOT, kind, backtrace(5), Default::default());
    command.command_lines.push(vec!["true".into()]);
    command
}

#[test]
fn custom_target_depends_trace_the_rule_of_a_generated_output() -> TestResult {
    let mut model = project();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    let mut generate = command(rcmake_model::CustomCommandKind::Output);
    generate.outputs.push(entry("/out/gen.c"));
    generate.depends.push(entry("a.c"));
    let _ = model.add_command(generate);
    model.add_target(target("genlib", TargetKind::StaticLibrary, &["/out/gen.c"]))?;
    let custom = model.add_target(target("ct2", TargetKind::Utility, &[]))?;
    let mut utility = command(rcmake_model::CustomCommandKind::Utility(custom));
    utility.depends.push(entry("/out/gen.c"));
    let _ = model.add_command(utility);
    let mut event_owner = target("app", TargetKind::Executable, &["main.c"]);
    event_owner.usage.link_libraries = Some(Vec::new());
    let app = model.add_target(event_owner)?;
    let mut event = command(rcmake_model::CustomCommandKind::TargetEvent {
        target: app,
        event: rcmake_model::TargetEvent::PostBuild,
    });
    event.depends.push(entry("/out/gen.c"));
    let _ = model.add_command(event);
    let result = resolve_with_sources(&model, &["a.c", "main.c"])?;
    assert_eq!(
        result.target_depends.traced_sources(custom),
        ["/out/gen.c.rule"]
    );
    assert_eq!(
        result.target_depends.traced_sources(app),
        ["/out/gen.c.rule"]
    );
    Ok(())
}
