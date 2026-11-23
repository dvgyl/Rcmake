use rcmake_diag::{Backtrace, PolicyId, PolicyMap, PolicyStatus};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, GenexHost};
use rcmake_model::{GeneratorKind, ImportedVisibility, LinkItem, TargetKind, TargetOrigin};

use super::*;

fn model(system: &str) -> ProjectModel {
    let mut model = ProjectModel::default();
    model.set_root_dirs("/src".into(), "/build".into());
    model
        .global
        .props
        .set("CMAKE_SYSTEM_NAME".into(), system.into());
    model
}

fn add(model: &mut ProjectModel, name: &str, kind: TargetKind) -> TargetId {
    add_in(model, name, kind, TargetOrigin::Normal, DirId::ROOT)
}

fn add_in(
    model: &mut ProjectModel,
    name: &str,
    kind: TargetKind,
    origin: TargetOrigin,
    dir: DirId,
) -> TargetId {
    model
        .add_target(Target::new(
            name.into(),
            kind,
            origin,
            dir,
            Backtrace::default(),
            PolicyMap::default(),
        ))
        .unwrap()
}

fn usage(model: &mut ProjectModel, id: TargetId, field: UsageField, value: &str) {
    model
        .target_mut(id)
        .unwrap()
        .usage
        .push(field, BT::new(value.into(), Backtrace::default()));
}

fn prop(model: &mut ProjectModel, id: TargetId, key: &str, value: &str) {
    model
        .target_mut(id)
        .unwrap()
        .props
        .set(key.into(), value.into());
}

fn link(model: &mut ProjectModel, id: TargetId, interface: bool, items: &[&str]) {
    let dir = model.target(id).unwrap().dir;
    let usage = &mut model.target_mut(id).unwrap().usage;
    let entries = if interface {
        &mut usage.interface_link_libraries
    } else {
        &mut usage.link_libraries
    };
    entries
        .get_or_insert_default()
        .extend(items.iter().map(|name| LinkItem {
            text: BT::new((*name).into(), Backtrace::default()),
            lookup_dir: dir,
        }));
}

fn eval(
    resolver: &mut UsageResolver<'_>,
    expression: &[u8],
    ctx: &EvalContext,
) -> Result<BString, GenexError> {
    evaluate(&parse(expression)?, ctx, &mut resolver.host(DirId::ROOT)).map(|result| result.0)
}

fn values(entries: &[BT<BString>]) -> BString {
    list::join(entries.iter().map(|entry| &entry.value))
}

fn link_line(items: &[ResolvedLinkItem]) -> BString {
    list::join(items.iter().map(|entry| &entry.text.value))
}

#[test]
fn diamond_matches_compile_property_and_link_line_order() {
    let mut model = model("Linux");
    let top = add(&mut model, "top", TargetKind::StaticLibrary);
    let left = add(&mut model, "left", TargetKind::StaticLibrary);
    let right = add(&mut model, "right", TargetKind::StaticLibrary);
    let base = add(&mut model, "base", TargetKind::StaticLibrary);
    let ctx = EvalContext::default();
    for (id, name) in [
        (top, "top"),
        (left, "left"),
        (right, "right"),
        (base, "base"),
    ] {
        usage(
            &mut model,
            id,
            UsageField::InterfaceCompileDefinitions,
            name,
        );
    }
    usage(&mut model, top, UsageField::CompileDefinitions, "top");
    link(&mut model, top, false, &["left", "right"]);
    link(&mut model, top, true, &["left", "right"]);
    link(&mut model, left, true, &["base"]);
    link(&mut model, right, true, &["base"]);

    let mut resolver = UsageResolver::new(&model);
    assert_eq!(
        values(
            &resolver
                .collect(top, UsageField::CompileDefinitions, &ctx)
                .unwrap()
                .entries
        ),
        "top;left;base;right"
    );
    assert_eq!(
        eval(
            &mut resolver,
            b"$<TARGET_PROPERTY:top,INTERFACE_LINK_LIBRARIES>",
            &ctx
        )
        .unwrap(),
        "left;right;base;base"
    );
    assert_eq!(
        link_line(&resolver.collect_link_items(top, &ctx).unwrap()),
        "left;right;base"
    );
    assert!(resolver.memo.keys().all(|key| key.usage.head.is_none()));
}

#[test]
fn link_only_skips_compile_usage_and_keeps_link_usage() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let mid = add(&mut model, "mid", TargetKind::StaticLibrary);
    let hidden = add(&mut model, "hidden", TargetKind::StaticLibrary);
    usage(
        &mut model,
        hidden,
        UsageField::InterfaceCompileDefinitions,
        "HIDDEN",
    );
    usage(
        &mut model,
        hidden,
        UsageField::InterfaceIncludeDirectories,
        "/hidden",
    );
    usage(
        &mut model,
        hidden,
        UsageField::InterfaceLinkOptions,
        "-pthread",
    );
    link(&mut model, app, false, &["mid"]);
    link(
        &mut model,
        mid,
        true,
        &["$<LINK_ONLY:$<TARGET_NAME_IF_EXISTS:hidden>>"],
    );
    let ctx = EvalContext::default();
    let mut resolver = UsageResolver::new(&model);

    assert!(resolver.collect_includes(app, &ctx).unwrap().is_empty());
    assert!(
        resolver
            .collect(app, UsageField::CompileDefinitions, &ctx)
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        values(
            &resolver
                .collect(app, UsageField::LinkOptions, &ctx)
                .unwrap()
                .entries
        ),
        "-pthread"
    );
    assert_eq!(
        link_line(&resolver.collect_link_items(app, &ctx).unwrap()),
        "mid;hidden"
    );
}

#[test]
fn two_pass_evaluation_and_head_sensitive_memoization() {
    let mut model = model("Linux");
    let a = add(&mut model, "a", TargetKind::Executable);
    let b = add(&mut model, "b", TargetKind::Executable);
    let interface = add(&mut model, "api", TargetKind::InterfaceLibrary);
    for (id, name) in [(a, "FIRST"), (b, "SECOND")] {
        prop(&mut model, id, "KEY", name);
        prop(&mut model, id, "EXPR", "$<TARGET_PROPERTY:KEY>");
        link(&mut model, id, false, &["api"]);
    }
    usage(
        &mut model,
        interface,
        UsageField::InterfaceCompileDefinitions,
        "$<TARGET_PROPERTY:KEY>",
    );
    let mut resolver = UsageResolver::new(&model);
    let ctx = EvalContext {
        head: Some(handle(a)),
        ..Default::default()
    };
    assert_eq!(
        eval(
            &mut resolver,
            b"$<TARGET_GENEX_EVAL:b,$<TARGET_PROPERTY:EXPR>>",
            &ctx
        )
        .unwrap(),
        "SECOND"
    );
    for (id, value) in [(a, "FIRST"), (b, "SECOND"), (a, "FIRST")] {
        let result = resolver
            .collect(id, UsageField::CompileDefinitions, &EvalContext::default())
            .unwrap();
        assert_eq!(values(&result.entries), value);
        assert!(result.flags.head_sensitive);
    }
    let heads: HashSet<_> = resolver
        .memo
        .keys()
        .filter(|key| key.usage.target == interface)
        .map(|key| key.usage.head)
        .collect();
    assert_eq!(heads, HashSet::from([Some(a), Some(b)]));
}

#[test]
fn active_property_cycle_is_distinct_from_a_static_link_cycle() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let one = add(&mut model, "one", TargetKind::StaticLibrary);
    let two = add(&mut model, "two", TargetKind::StaticLibrary);
    link(&mut model, app, false, &["one"]);
    link(&mut model, one, true, &["two"]);
    link(&mut model, two, true, &["one"]);
    usage(
        &mut model,
        one,
        UsageField::InterfaceCompileDefinitions,
        "ONE",
    );
    usage(
        &mut model,
        two,
        UsageField::InterfaceCompileDefinitions,
        "TWO",
    );
    prop(&mut model, two, "LINK_INTERFACE_MULTIPLICITY", "3");
    let ctx = EvalContext::default();
    assert_eq!(
        values(
            &collect(&model, app, UsageField::CompileDefinitions, &ctx)
                .unwrap()
                .entries
        ),
        "ONE;TWO"
    );
    assert_eq!(
        link_line(&collect_link_items(&model, app, &ctx).unwrap()),
        "one;two;one;two;one;two"
    );

    usage(
        &mut model,
        two,
        UsageField::InterfaceCompileDefinitions,
        "$<TARGET_PROPERTY:app,COMPILE_DEFINITIONS>",
    );
    assert_eq!(
        collect(&model, app, UsageField::CompileDefinitions, &ctx)
            .unwrap_err()
            .to_string(),
        "Self reference on target \"app\"."
    );
}

#[test]
fn system_includes_use_imported_defaults_and_explicit_paths() {
    let mut model = model("Linux");
    let parent = model
        .add_directory(
            DirId::ROOT,
            "/src/parent".into(),
            "/build/parent".into(),
            Backtrace::default(),
        )
        .unwrap();
    let directory = &mut model.dir_mut(parent).usage;
    directory.push(
        UsageField::IncludeDirectories,
        BT::new("/dirsys;/local".into(), Backtrace::default()),
    );
    directory.push(
        UsageField::InterfaceSystemIncludeDirectories,
        BT::new("/dirsys".into(), Backtrace::default()),
    );
    let app = add_in(
        &mut model,
        "app",
        TargetKind::Executable,
        TargetOrigin::Normal,
        parent,
    );
    let imported = add_in(
        &mut model,
        "imported",
        TargetKind::InterfaceLibrary,
        TargetOrigin::Imported(ImportedVisibility::Global),
        DirId::ROOT,
    );
    usage(
        &mut model,
        imported,
        UsageField::InterfaceIncludeDirectories,
        "/implicit;/explicit;/local",
    );
    usage(
        &mut model,
        imported,
        UsageField::InterfaceSystemIncludeDirectories,
        "/explicit",
    );
    link(&mut model, app, false, &["imported"]);
    let include_flags = |model: &ProjectModel| {
        collect_includes(model, app, &EvalContext::default())
            .unwrap()
            .into_iter()
            .map(|entry| (entry.path.value, entry.system))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        include_flags(&model),
        vec![
            ("/dirsys".into(), true),
            ("/local".into(), true),
            ("/implicit".into(), true),
            ("/explicit".into(), true)
        ]
    );
    prop(&mut model, app, "NO_SYSTEM_FROM_IMPORTED", "ON");
    assert_eq!(
        include_flags(&model),
        vec![
            ("/local".into(), false),
            ("/implicit".into(), false),
            ("/dirsys".into(), true),
            ("/explicit".into(), true)
        ]
    );
}

#[test]
fn cmp0079_uses_callers_directory_for_literal_and_genex_names() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let dir = model
        .add_directory(
            DirId::ROOT,
            "/src/other".into(),
            "/build/other".into(),
            Backtrace::default(),
        )
        .unwrap();
    let lib = add_in(
        &mut model,
        "local::lib",
        TargetKind::StaticLibrary,
        TargetOrigin::Imported(ImportedVisibility::Local),
        dir,
    );
    usage(
        &mut model,
        lib,
        UsageField::InterfaceCompileDefinitions,
        "FOUND",
    );
    link(
        &mut model,
        app,
        false,
        &["$<TARGET_NAME_IF_EXISTS:local::lib>"],
    );
    model
        .target_mut(app)
        .unwrap()
        .usage
        .link_libraries
        .as_mut()
        .unwrap()[0]
        .lookup_dir = dir;
    let ctx = EvalContext::default();
    assert!(model.find_target(DirId::ROOT, b"local::lib").is_none());
    assert_eq!(
        values(
            &collect(&model, app, UsageField::CompileDefinitions, &ctx)
                .unwrap()
                .entries
        ),
        "FOUND"
    );
    assert_eq!(
        collect_link_items(&model, app, &ctx).unwrap()[0].kind,
        ResolvedLinkKind::Library(lib)
    );
}

#[test]
fn injections_exclusions_qualifiers_and_link_features() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let base = add(&mut model, "base", TargetKind::StaticLibrary);
    let injected = add(&mut model, "plugin", TargetKind::StaticLibrary);
    let objects = add(&mut model, "obj", TargetKind::ObjectLibrary);
    let debug = add(&mut model, "dbg", TargetKind::StaticLibrary);
    let release = add(&mut model, "rel", TargetKind::StaticLibrary);
    let _member = add(&mut model, "member", TargetKind::StaticLibrary);
    prop(
        &mut model,
        base,
        "INTERFACE_LINK_LIBRARIES_DIRECT",
        "plugin",
    );
    prop(
        &mut model,
        base,
        "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        "base",
    );
    link(&mut model, injected, true, &["base", "obj"]);
    link(
        &mut model,
        app,
        false,
        &[
            "base",
            "debug",
            "dbg",
            "optimized",
            "rel",
            "general",
            "$<LINK_GROUP:RESCAN,member,$<LINK_LIBRARY:DEFAULT,-lm>>",
        ],
    );
    let ctx = EvalContext {
        config: ConfigId("dEbUg".into()),
        ..Default::default()
    };
    let items = collect_link_items(&model, app, &ctx).unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].kind, ResolvedLinkKind::Library(injected));
    assert_eq!(items[1].kind, ResolvedLinkKind::Library(debug));
    let ResolvedLinkKind::Group { feature, members } = &items[2].kind else {
        panic!("Expected link group")
    };
    assert_eq!(feature, "RESCAN");
    assert_eq!(link_line(members), "member;-lm");
    assert_eq!(members[1].feature, Some("DEFAULT".into()));
    assert_eq!(items[3].kind, ResolvedLinkKind::Library(base));

    link(&mut model, app, false, &["obj"]);
    let release_ctx = EvalContext {
        config: ConfigId("Release".into()),
        ..Default::default()
    };
    let items = collect_link_items(&model, app, &release_ctx).unwrap();
    assert_eq!(items[0].kind, ResolvedLinkKind::Objects(objects));
    assert!(
        items
            .iter()
            .any(|item| item.kind == ResolvedLinkKind::Library(release))
    );
}

#[test]
fn deduplication_respects_linker_processing_and_static_policy() {
    let mut model = model("Darwin");
    let app = add(&mut model, "app", TargetKind::Executable);
    let _a = add(&mut model, "a", TargetKind::StaticLibrary);
    let b = add(&mut model, "b", TargetKind::StaticLibrary);
    let _c = add(&mut model, "c", TargetKind::StaticLibrary);
    link(&mut model, b, true, &["a"]);
    link(&mut model, app, false, &["a", "b", "c"]);
    let line = |model: &ProjectModel| {
        link_line(&collect_link_items(model, app, &EvalContext::default()).unwrap())
    };
    assert_eq!(line(&model), "a;b;c;a");
    model
        .target_mut(app)
        .unwrap()
        .policies
        .policies
        .set(PolicyId::new(156), PolicyStatus::New)
        .unwrap();
    assert_eq!(line(&model), "b;c;a");
    model
        .target_mut(app)
        .unwrap()
        .policies
        .policies
        .set(PolicyId::new(179), PolicyStatus::New)
        .unwrap();
    assert_eq!(line(&model), "a;b;c");
}

#[test]
fn artifacts_objects_policies_and_toolchain_definitions() {
    let mut model = model("Darwin");
    model.generator = Some(GeneratorKind::NinjaMultiConfig);
    model
        .global
        .props
        .set("CMAKE_C_COMPILER_ID".into(), "AppleClang".into());
    let lib = add(&mut model, "shared", TargetKind::SharedLibrary);
    prop(&mut model, lib, "OUTPUT_NAME", "renamed");
    prop(
        &mut model,
        lib,
        "LIBRARY_OUTPUT_DIRECTORY",
        "out/$<LOWER_CASE:$<CONFIG>>",
    );
    prop(&mut model, lib, "DEBUG_POSTFIX", "_d");
    prop(&mut model, lib, "VERSION", "2.3");
    prop(&mut model, lib, "SOVERSION", "2");
    let objects = add(&mut model, "obj", TargetKind::ObjectLibrary);
    model
        .target_mut(objects)
        .unwrap()
        .sources
        .push(BT::new("x.c;y.cpp;header.h".into(), Backtrace::default()));
    let imported = add_in(
        &mut model,
        "imported",
        TargetKind::StaticLibrary,
        TargetOrigin::Imported(ImportedVisibility::Global),
        DirId::ROOT,
    );
    prop(
        &mut model,
        imported,
        "MAP_IMPORTED_CONFIG_DEBUG",
        "RelWithDebInfo",
    );
    prop(
        &mut model,
        imported,
        "IMPORTED_LOCATION_RELWITHDEBINFO",
        "/vendor/libimported.a",
    );
    model
        .target_mut(lib)
        .unwrap()
        .policies
        .policies
        .set(PolicyId::new(99), PolicyStatus::New)
        .unwrap();
    let config = ConfigId("Debug".into());
    let ctx = EvalContext {
        head: Some(handle(lib)),
        config: config.clone(),
        ..Default::default()
    };
    let mut resolver = UsageResolver::new(&model);
    let path = |kind| {
        resolver
            .artifact(lib, kind, ArtifactPart::Full, &config)
            .unwrap()
    };
    assert_eq!(
        path(ArtifactKind::File),
        "/build/out/debug/librenamed_d.2.3.dylib"
    );
    assert_eq!(
        path(ArtifactKind::LinkerFile),
        "/build/out/debug/librenamed_d.dylib"
    );
    assert_eq!(
        path(ArtifactKind::SonameFile),
        "/build/out/debug/librenamed_d.2.dylib"
    );
    assert_eq!(eval(&mut resolver, b"$<TARGET_FILE_BASE_NAME:shared,POSTFIX:EXCLUDE>;$<TARGET_FILE:imported>;$<TARGET_POLICY:CMP0099>;$<C_COMPILER_ID>;$<PLATFORM_ID>", &ctx).unwrap(), "renamed;/vendor/libimported.a;1;AppleClang;Darwin");
    assert_eq!(
        resolver.target_objects(objects, &config, None).unwrap(),
        "/build/CMakeFiles/obj.dir/Debug/x.c.o;/build/CMakeFiles/obj.dir/Debug/y.cpp.o"
    );
    assert_eq!(
        resolver
            .target_objects(objects, &config, Some(b"y.cpp"))
            .unwrap(),
        "/build/CMakeFiles/obj.dir/Debug/y.cpp.o"
    );
    assert_eq!(
        resolver
            .host(DirId::ROOT)
            .raw_property(handle(objects), b"SOURCES")
            .unwrap(),
        "x.c;y.cpp;header.h"
    );
}

#[test]
fn context_sensitive_diamonds_share_one_interface_value_per_target() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let mut previous = [app, app];
    for level in 0..18 {
        let names = [format!("left{level}"), format!("right{level}")];
        let current = names
            .each_ref()
            .map(|name| add(&mut model, name, TargetKind::InterfaceLibrary));
        for id in current {
            usage(
                &mut model,
                id,
                UsageField::InterfaceCompileDefinitions,
                "$<CONFIG>",
            );
        }
        let dependencies = names.each_ref().map(String::as_str);
        link(&mut model, previous[0], level != 0, &dependencies);
        if level != 0 {
            link(&mut model, previous[1], true, &dependencies);
        }
        previous = current;
    }

    let ctx = EvalContext {
        config: ConfigId("Debug".into()),
        ..Default::default()
    };
    let mut resolver = UsageResolver::new(&model);
    let result = resolver
        .collect(app, UsageField::CompileDefinitions, &ctx)
        .unwrap();
    assert_eq!(values(&result.entries), "Debug");
    assert_eq!(resolver.memo.len(), 37);
    assert!(resolver.memo.keys().all(|key| key.usage.head.is_none()));
    let value = eval(
        &mut resolver,
        b"$<TARGET_PROPERTY:app,COMPILE_DEFINITIONS>",
        &ctx,
    )
    .unwrap();
    assert_eq!(list::split(&value, list::Empty::Drop).len(), 36);
}

#[test]
fn link_property_preserves_feature_markers_and_original_direct_entries() {
    let mut model = model("Linux");
    let app = add(&mut model, "app", TargetKind::Executable);
    let base = add(&mut model, "base", TargetKind::StaticLibrary);
    let plugin = add(&mut model, "plugin", TargetKind::StaticLibrary);
    prop(
        &mut model,
        base,
        "INTERFACE_LINK_LIBRARIES_DIRECT",
        "plugin",
    );
    prop(
        &mut model,
        base,
        "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        "base",
    );
    link(
        &mut model,
        app,
        false,
        &["base", "$<LINK_LIBRARY:DEFAULT,m;n>"],
    );
    link(&mut model, plugin, true, &["base"]);
    let mut resolver = UsageResolver::new(&model);
    assert_eq!(
        eval(
            &mut resolver,
            b"$<TARGET_PROPERTY:app,LINK_LIBRARIES>",
            &EvalContext::default()
        )
        .unwrap(),
        "base;<LINK_LIBRARY:DEFAULT>;m;n;</LINK_LIBRARY:DEFAULT>;base"
    );
}

#[test]
fn archive_deduplication_keeps_raw_link_flags_in_place() {
    let mut model = model("Darwin");
    let app = add(&mut model, "app", TargetKind::Executable);
    let policies = &mut model.target_mut(app).unwrap().policies.policies;
    for policy in [156, 179] {
        policies
            .set(PolicyId::new(policy), PolicyStatus::New)
            .unwrap();
    }
    link(
        &mut model,
        app,
        false,
        &[
            "/tmp/liba.a",
            "-pthread",
            "/tmp/liba.a",
            "-pthread",
            "-framework Foundation",
            "-framework Foundation",
        ],
    );
    let result = collect_link_items(&model, app, &EvalContext::default()).unwrap();
    assert_eq!(
        link_line(&result),
        "-pthread;/tmp/liba.a;-pthread;-framework Foundation"
    );
}
