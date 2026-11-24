use rcmake_diag::PolicyMap;
use rcmake_model::{DirId, TargetKind, TargetOrigin};
use rcmake_resolve::InterfaceInclude;

use super::*;

#[test]
fn preserves_compile_order_and_applicability() {
    let target = Target::new(
        "app".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        PolicyMap::default(),
    );
    let mut result = ConfigureResult::default();
    result
        .model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_CXX_FLAGS".into(), "-O2".into());
    let shared = BT::new("SHARED".into(), Backtrace::default());
    let unique = BT::new("UNIQUE".into(), Backtrace::default());
    let include = InterfaceInclude {
        path: BT::new("/path".into(), Backtrace::default()),
        system: true,
    };
    let groups = [
        CompileGroup {
            language: "CXX".into(),
            defines: vec![shared.clone()],
            includes: vec![include.clone()],
            ..CompileGroup::default()
        },
        CompileGroup {
            language: "CXX".into(),
            defines: vec![unique, shared],
            includes: vec![include],
            ..CompileGroup::default()
        },
    ];
    let (_, entries) = compile_entries(&result, &target, b"Debug", &groups, &[]);
    let [flags, shared, include, unique] = entries.as_slice() else {
        assert_eq!(entries.len(), 4);
        return;
    };
    assert_eq!(flags.value, "-O2");
    assert!(
        flags
            .input
            .as_ref()
            .is_some_and(|input| input.name == "CMAKE_CXX_FLAGS")
    );
    assert_eq!(
        shared.compile_positions,
        [
            CompilePosition {
                group: 0,
                ordinal: 1
            },
            CompilePosition {
                group: 1,
                ordinal: 2
            },
        ]
    );
    assert_eq!(
        include.spelling.as_ref().map(|value| value.as_slice()),
        Some(b"-isystem /path".as_slice())
    );
    assert_eq!(
        unique.compile_positions,
        [CompilePosition {
            group: 1,
            ordinal: 1
        }]
    );

    let mut record = shared.clone();
    record.chain = Some(Vec::new());
    let entries = join_entries(entries.clone(), vec![record]);
    assert_eq!(entries.len(), 4);
    assert!(
        entries
            .get(1)
            .is_some_and(|shared| shared.chain.as_ref().is_some_and(Vec::is_empty))
    );
    assert!(
        entries
            .get(2)
            .is_some_and(|include| include.chain.is_none())
    );
}
