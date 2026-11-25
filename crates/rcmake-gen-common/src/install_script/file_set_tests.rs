//! Install rules for file sets.

use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::Backtrace;
use rcmake_genex::ConfigId;
use rcmake_host::PureHost;
use rcmake_model::{
    BT, DirId, FileSet, FileSetKind, GeneratorKind, InstallKind, InstallOptions, InstallRule,
    ProjectModel, Target, TargetKind, TargetOrigin, UsageScope,
};
use rcmake_resolve::GenerationResult;

use crate::{install_script, write_supplementary_outputs};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn installs_only_the_interface_file_sets() -> TestResult {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let mut model = ProjectModel::default();
    model.set_root_dirs(format!("{root}/src").into(), format!("{root}/out").into());
    model.generator = Some(GeneratorKind::Ninja);
    let core = model.add_target(Target::new(
        "core".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    ))?;
    for (name, scope) in [("hidden", UsageScope::Private), ("api", UsageScope::Public)] {
        model.add_file_set(FileSet {
            target: core,
            name: name.into(),
            kind: FileSetKind::Headers,
            scope,
            base_dirs: vec![BT::new(format!("{root}/src").into(), Default::default())],
            files: vec![BT::new(
                format!("{root}/src/{name}.h").into(),
                Default::default(),
            )],
            props: Default::default(),
        })?;
    }
    let index = model.add_install(InstallRule {
        dir: DirId::ROOT,
        backtrace: Backtrace::default(),
        kind: InstallKind::Targets {
            targets: vec![core],
            export: None,
        },
        options: InstallOptions::default(),
    });
    for (arg, value) in [
        "FILE_SET",
        "hidden",
        "FILE_SET",
        "api",
        "DESTINATION",
        "include",
    ]
    .into_iter()
    .enumerate()
    {
        model.global.props.set(
            format!("_RCMAKE_INSTALL_{index}_ARG{arg}").into(),
            value.into(),
        );
    }
    let result = GenerationResult {
        configs: vec![ConfigId(BString::default())],
        ..GenerationResult::default()
    };
    let host = PureHost::default();
    let build = format!("{root}/out");
    let manifest = write_supplementary_outputs(&model, &result, &host, Path::new(&build))?;
    let script = install_script::render(&manifest, build.as_bytes())?.to_string();
    assert!(script.contains("api.h"), "{script}");
    assert!(!script.contains("hidden.h"), "{script}");
    Ok(())
}
