use rcmake_diag::PolicyMap;

use super::*;
use crate::TargetKind;

fn origin(bt: &Backtrace) -> PropOrigin<'_> {
    PropOrigin {
        dir: DirId::ROOT,
        backtrace: bt,
    }
}

fn target(name: &str, dir: DirId, origin: TargetOrigin) -> Target {
    Target::new(
        name.into(),
        TargetKind::StaticLibrary,
        origin,
        dir,
        Backtrace::default(),
        PolicyMap::default(),
    )
}

#[test]
fn ancestors_follow_decreasing_parent_indices_and_stop_at_a_cycle() {
    let mut model = ProjectModel::default();
    let child = model
        .add_directory(
            DirId::ROOT,
            "/s/c".into(),
            "/b/c".into(),
            Backtrace::default(),
        )
        .unwrap();
    let grandchild = model
        .add_directory(
            child,
            "/s/c/g".into(),
            "/b/c/g".into(),
            Backtrace::default(),
        )
        .unwrap();
    for (id, directory) in model.dirs() {
        if let Some(parent) = directory.parent {
            assert!(parent.index() < id.index());
        }
    }
    assert!(
        model
            .ancestors(grandchild)
            .eq([grandchild, child, DirId::ROOT])
    );

    model.dir_mut(child).parent = Some(grandchild);
    assert!(model.ancestors(grandchild).eq([grandchild, child]));
}

#[test]
fn local_imported_targets_are_scoped_to_directory_and_later_children() {
    let mut model = ProjectModel::default();
    let early = model
        .add_directory(
            DirId::ROOT,
            "/s/early".into(),
            "/b/early".into(),
            Backtrace::default(),
        )
        .unwrap();
    let local = TargetOrigin::Imported(ImportedVisibility::Local);
    let imp = model.add_target(target("imp", DirId::ROOT, local)).unwrap();
    model.add_alias(DirId::ROOT, "ns::imp".into(), imp).unwrap();
    let late = model
        .add_directory(
            DirId::ROOT,
            "/s/late".into(),
            "/b/late".into(),
            Backtrace::default(),
        )
        .unwrap();
    assert_eq!(model.find_target(DirId::ROOT, b"ns::imp"), Some(imp));
    assert_eq!(model.find_target(late, b"imp"), Some(imp));
    assert_eq!(model.find_target(early, b"imp"), None);

    model.set_imported_global(imp).unwrap();
    assert_eq!(model.find_target(early, b"imp"), Some(imp));

    let normal = model
        .add_target(target("lib", late, TargetOrigin::Normal))
        .unwrap();
    assert_eq!(model.find_target(early, b"lib"), Some(normal));
    assert_eq!(
        model.add_target(target("lib", DirId::ROOT, TargetOrigin::Normal)),
        Err(ModelError::DuplicateTarget(normal))
    );
    assert_eq!(
        model.get_prop(&PropScope::Directory(late), b"BUILDSYSTEM_TARGETS"),
        Some("lib".into())
    );
    assert_eq!(
        model.get_prop(&PropScope::Directory(DirId::ROOT), b"SUBDIRECTORIES"),
        Some("/s/early;/s/late".into())
    );
}

#[test]
fn new_directory_copies_only_initialize_from_parent_state() {
    let bt = Backtrace::default();
    let mut model = ProjectModel::default();
    let root = PropScope::Directory(DirId::ROOT);
    for (name, value) in [
        ("LABELS", "l"),
        ("OTHER", "o"),
        ("INCLUDE_DIRECTORIES", "/i"),
    ] {
        model
            .set_prop(&root, name.as_bytes(), Some(value.into()), origin(&bt))
            .unwrap();
    }
    let child = model
        .add_directory(DirId::ROOT, "/s/c".into(), "/b/c".into(), bt.clone())
        .unwrap();
    let child = PropScope::Directory(child);
    assert_eq!(model.get_prop(&child, b"LABELS"), Some("l".into()));
    assert_eq!(model.get_prop(&child, b"OTHER"), None);
    assert_eq!(
        model.get_prop(&child, b"INCLUDE_DIRECTORIES"),
        Some("/i".into())
    );
    assert_eq!(
        model.get_prop(&child, b"PARENT_DIRECTORY"),
        Some(BString::default())
    );
}

#[test]
fn inherited_lookup_stops_at_explicit_empty_value() {
    let bt = Backtrace::default();
    let mut model = ProjectModel::default();
    let child = model
        .add_directory(DirId::ROOT, "/s/c".into(), "/b/c".into(), bt.clone())
        .unwrap();
    let t = model
        .add_target(target("t", child, TargetOrigin::Normal))
        .unwrap();
    model
        .set_prop(&PropScope::Global, b"P", Some("global".into()), origin(&bt))
        .unwrap();
    assert_eq!(
        model.get_prop_inherited(&PropScope::Target(t), b"P"),
        Some("global".into())
    );
    model
        .set_prop(
            &PropScope::Directory(DirId::ROOT),
            b"P",
            Some("root".into()),
            origin(&bt),
        )
        .unwrap();
    assert_eq!(
        model.get_prop_inherited(&PropScope::Target(t), b"P"),
        Some("root".into())
    );
    model
        .set_prop(
            &PropScope::Directory(child),
            b"P",
            Some(BString::default()),
            origin(&bt),
        )
        .unwrap();
    assert_eq!(
        model.get_prop_inherited(&PropScope::Target(t), b"P"),
        Some(BString::default())
    );
    assert_eq!(model.get_prop(&PropScope::Target(t), b"P"), None);
}

#[test]
fn usage_properties_keep_one_entry_per_append() {
    let bt = Backtrace::default();
    let mut model = ProjectModel::default();
    let t = model
        .add_target(target("t", DirId::ROOT, TargetOrigin::Normal))
        .unwrap();
    let scope = PropScope::Target(t);
    for value in ["a", "", "b"] {
        model
            .append_prop(
                &scope,
                b"INCLUDE_DIRECTORIES",
                value.as_bytes(),
                AppendMode::List,
                origin(&bt),
            )
            .unwrap();
    }
    model
        .append_prop(
            &scope,
            b"LINK_LIBRARIES",
            b"m",
            AppendMode::List,
            origin(&bt),
        )
        .unwrap();
    let target = model.target(t).unwrap();
    assert_eq!(
        target.usage.entries(UsageField::IncludeDirectories).len(),
        2
    );
    assert_eq!(
        model.get_prop(&scope, b"INCLUDE_DIRECTORIES"),
        Some("a;b".into())
    );
    assert_eq!(model.get_prop(&scope, b"LINK_LIBRARIES"), Some("m".into()));
    assert_eq!(
        model.get_prop(&scope, b"TYPE"),
        Some("STATIC_LIBRARY".into())
    );
    assert_eq!(
        model.set_prop(&scope, b"NAME", Some("x".into()), origin(&bt)),
        Err(ModelError::ReadOnly)
    );
    model
        .append_prop(
            &scope,
            b"COMPILE_FLAGS",
            b"-a",
            AppendMode::String,
            origin(&bt),
        )
        .unwrap();
    model
        .append_prop(
            &scope,
            b"COMPILE_FLAGS",
            b" -b",
            AppendMode::String,
            origin(&bt),
        )
        .unwrap();
    assert_eq!(
        model.get_prop(&scope, b"COMPILE_FLAGS"),
        Some("-a -b".into())
    );
}

fn test_entry(name: &str, kind: crate::TestKind) -> TestEntry {
    TestEntry {
        name: name.into(),
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        command: vec!["run".into()],
        configurations: Vec::new(),
        working_directory: None,
        command_expand_lists: false,
        old_style: false,
        policies: Default::default(),
        props: crate::PropertyMap::default(),
        build_depends: Vec::new(),
        kind,
    }
}

#[test]
fn file_set_properties_are_scoped_and_inherit_from_the_target() {
    let bt = Backtrace::default();
    let mut model = ProjectModel::default();
    let lib = model
        .add_target(target("lib", DirId::ROOT, TargetOrigin::Normal))
        .unwrap();
    let set = model
        .add_file_set(FileSet {
            target: lib,
            name: "SOURCES".into(),
            kind: crate::FileSetKind::Sources,
            scope: crate::UsageScope::Private,
            base_dirs: Vec::new(),
            files: Vec::new(),
            props: crate::PropertyMap::default(),
        })
        .unwrap();
    let scope = PropScope::FileSet(set);
    model
        .set_prop(&scope, b"LANGUAGE", Some("CXX".into()), origin(&bt))
        .unwrap();
    assert_eq!(model.get_prop(&scope, b"LANGUAGE"), Some("CXX".into()));
    assert_eq!(model.get_prop(&PropScope::Target(lib), b"LANGUAGE"), None);
    model
        .set_prop(
            &PropScope::Target(lib),
            b"FOLDER",
            Some("f".into()),
            origin(&bt),
        )
        .unwrap();
    assert_eq!(
        model.get_prop_inherited(&scope, b"FOLDER"),
        Some("f".into())
    );
    model
        .set_prop(&scope, b"LANGUAGE", None, origin(&bt))
        .unwrap();
    assert_eq!(model.get_prop(&scope, b"LANGUAGE"), None);
}

#[test]
fn discovery_entries_skip_name_lookup_but_keep_call_order() {
    let mut model = ProjectModel::default();
    let spec = crate::TestKind::Discover(Box::default());
    let first = model
        .add_test(test_entry("a", crate::TestKind::Single))
        .unwrap();
    let discover = model.add_test(test_entry("", spec.clone())).unwrap();
    let again = model.add_test(test_entry("", spec)).unwrap();
    let last = model
        .add_test(test_entry("b", crate::TestKind::Single))
        .unwrap();
    assert_eq!(model.find_test(DirId::ROOT, b""), None);
    assert_eq!(model.dir(DirId::ROOT).discoveries, vec![discover, again]);
    assert_eq!(
        model.get_prop(&PropScope::Directory(DirId::ROOT), b"TESTS"),
        Some("a;b".into())
    );
    let order: Vec<_> = model.dir_tests(DirId::ROOT).map(|(id, _)| id).collect();
    assert_eq!(order, vec![first, discover, again, last]);
}

#[test]
fn install_components_default_to_unspecified_and_sort() {
    let mut model = ProjectModel::default();
    assert!(model.install_components().is_empty());
    for component in [Some("dev"), None, Some("dev")] {
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: Backtrace::default(),
            kind: crate::InstallKind::Code("x".into()),
            options: crate::InstallOptions {
                component: component.map(BString::from),
                ..crate::InstallOptions::default()
            },
        });
    }
    let names: Vec<_> = model.install_components().into_iter().collect();
    assert_eq!(names, vec![BString::from("Unspecified"), "dev".into()]);
}
