use std::path::PathBuf;
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{
    PureHost, Stream,
    path::{from_native_cmake, to_native},
};
use rcmake_model::{DirId, PropScope, TargetKind};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    host: Arc<PureHost>,
    interp: Interp,
    source: PathBuf,
    binary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let cwd = to_native(if cfg!(windows) { b"C:/work" } else { b"/work" });
        let exe = cwd.join("cmake");
        let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
        let interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: cwd.join("share/cmake-4.5"),
                cmake_command: exe,
                cwd: cwd.clone(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        Self {
            host,
            interp,
            source: cwd.join("src"),
            binary: cwd.join("build"),
        }
    }

    fn configure(&mut self, script: &str, code: i32) -> TestResult {
        self.host.seed_file(
            &self.source.join("CMakeLists.txt"),
            format!("cmake_minimum_required(VERSION 4.5)\n{script}").as_bytes(),
        )?;
        let result = self.interp.run_configure(&self.source, &self.binary, &[]);
        assert_eq!(
            result.exit_code,
            code,
            "{}",
            self.host.stream_bytes(Stream::Stderr)
        );
        Ok(())
    }
}

#[test]
fn output_commands_resolve_paths_and_append_dependencies() -> TestResult {
    let mut f = Fixture::new();
    f.configure(
        r#"
        add_custom_command(OUTPUT generated/../result other
            COMMAND tool ARGS "one;two" "with space"
            COMMAND tool2 "" next
            DEPENDS dependency
            MAIN_DEPENDENCY input
            BYPRODUCTS side
            WORKING_DIRECTORY work
            COMMENT "Generating files"
            DEPFILE deps.d JOB_POOL pool JOB_SERVER_AWARE ON
            COMMAND_EXPAND_LISTS VERBATIM DEPENDS_EXPLICIT_ONLY)
        add_custom_command(OUTPUT other APPEND
            COMMAND tool3 arg DEPENDS later IMPLICIT_DEPENDS C header.h)
        "#,
        0,
    )?;
    let mut cx = f.interp.cx();
    let project = cx.project();
    assert_eq!(project.commands().count(), 1);
    let (id, command) = project.commands().next().ok_or("missing output command")?;
    assert_eq!(command.kind, CustomCommandKind::Output);
    assert_eq!(
        command.command_lines,
        [
            vec![BString::from("tool"), "one;two".into(), "with space".into()],
            vec![BString::from("tool2"), "".into(), "next".into()],
            vec![BString::from("tool3"), "arg".into()],
        ]
    );
    assert_eq!(
        command
            .depends
            .iter()
            .map(|dep| dep.value.as_slice())
            .collect::<Vec<_>>(),
        [b"dependency".as_slice(), b"later", b"header.h"]
    );
    assert_eq!(
        command.implicit_depends,
        [(BString::from("C"), "header.h".into())]
    );
    assert_eq!(command.main_dependency, Some("input".into()));
    assert_eq!(command.depfile, Some("deps.d".into()));
    assert_eq!(command.job_pool, Some("pool".into()));
    assert_eq!(
        command.working_directory,
        Some(from_native_cmake(&f.binary.join("work")))
    );
    assert_eq!(command.comment, Some("Generating files".into()));
    assert!(command.verbatim && command.job_server_aware && command.command_expand_lists);
    assert!(!command.backtrace.is_empty());
    assert_eq!(
        project.dir(DirId::ROOT).props.get(
            format!(
                "_RCMAKE_CUSTOM_COMMAND_{}_DEPENDS_EXPLICIT_ONLY",
                id.index()
            )
            .as_bytes()
        ),
        Some(&"1".into())
    );
    for (entry, name) in command
        .outputs
        .iter()
        .chain(&command.byproducts)
        .zip(["result", "other", "side"])
    {
        let path = from_native_cmake(&f.binary.join(name));
        assert_eq!(entry.value, path);
        assert_eq!(
            project.get_prop(
                &PropScope::Source {
                    dir: DirId::ROOT,
                    path
                },
                b"GENERATED"
            ),
            Some("1".into())
        );
    }
    Ok(())
}

#[test]
fn utility_targets_and_build_events_keep_their_attachments() -> TestResult {
    let mut f = Fixture::new();
    f.configure(
        r#"
        add_custom_target(stage ALL tool first COMMAND tool second
            DEPENDS result BYPRODUCTS staged SOURCES input.c input.h
            WORKING_DIRECTORY work COMMENT "Stage inputs" USES_TERMINAL)
        add_custom_target(manual)
        add_custom_command(TARGET stage PRE_BUILD COMMAND start)
        add_custom_command(TARGET stage PRE_LINK COMMAND prepare)
        add_custom_command(TARGET stage POST_BUILD COMMAND finish BYPRODUCTS report VERBATIM)
        "#,
        0,
    )?;
    let mut cx = f.interp.cx();
    let project = cx.project();
    let id = project
        .find_target(DirId::ROOT, b"stage")
        .ok_or("missing stage target")?;
    let target = project.target(id).ok_or("missing stage target")?;
    assert_eq!(target.kind, TargetKind::Utility);
    assert_eq!(target.props.get(b"EXCLUDE_FROM_ALL"), None);
    assert_eq!(
        target
            .sources
            .iter()
            .map(|source| source.value.as_slice())
            .collect::<Vec<_>>(),
        [b"input.c", b"input.h"]
    );
    let manual = project
        .find_target(DirId::ROOT, b"manual")
        .ok_or("missing manual target")?;
    assert_eq!(
        project.get_prop(&PropScope::Target(manual), b"EXCLUDE_FROM_ALL"),
        Some("TRUE".into())
    );
    let utility = project
        .commands()
        .find_map(|(_, command)| {
            (command.kind == CustomCommandKind::Utility(id)).then_some(command)
        })
        .ok_or("missing utility command")?;
    assert_eq!(
        utility.command_lines,
        [
            vec![BString::from("tool"), "first".into()],
            vec![BString::from("tool"), "second".into()],
        ]
    );
    assert_eq!(
        utility.depends.first().map(|entry| entry.value.as_slice()),
        Some(b"result".as_slice())
    );
    assert!(utility.uses_terminal);
    let events = [
        TargetEvent::PreBuild,
        TargetEvent::PreLink,
        TargetEvent::PostBuild,
    ];
    assert_eq!(target.event_commands.len(), events.len());
    for (command_id, event) in target.event_commands.iter().zip(events) {
        let command = project
            .command(*command_id)
            .ok_or("missing event command")?;
        assert_eq!(
            command.kind,
            CustomCommandKind::TargetEvent { target: id, event }
        );
        assert!(!command.backtrace.is_empty());
    }
    for name in ["staged", "report"] {
        let path = from_native_cmake(&f.binary.join(name));
        assert_eq!(
            project.get_prop(
                &PropScope::Source {
                    dir: DirId::ROOT,
                    path
                },
                b"GENERATED"
            ),
            Some("1".into())
        );
    }
    Ok(())
}

#[test]
fn invalid_custom_commands_leave_the_model_unchanged() -> TestResult {
    for (script, diagnostic) in [
        (
            "add_custom_command(OUTPUT missing APPEND COMMAND tool)",
            "not already a custom command output",
        ),
        (
            "add_custom_command(TARGET missing POST_BUILD COMMAND tool)",
            "has been created in this directory",
        ),
        (
            "add_custom_command(TARGET missing COMMAND tool)",
            "Exactly one of PRE_BUILD",
        ),
        (
            r#"add_custom_target(t COMMAND "\"tool\"")"#,
            "COMMAND may not contain literal quotes",
        ),
        (
            "add_custom_command(TARGET missing SOURCE input OUTPUT out COMMAND tool)",
            "The SOURCE signatures of add_custom_command are no longer supported.",
        ),
        (
            "add_custom_target(t COMMAND tool JOB_POOL pool USES_TERMINAL)",
            "JOB_POOL is shadowed",
        ),
    ] {
        let mut f = Fixture::new();
        f.configure(script, 1)?;
        assert!(
            f.host
                .stream_bytes(Stream::Stderr)
                .to_string()
                .contains(diagnostic)
        );
        let mut cx = f.interp.cx();
        assert_eq!(cx.project().commands().count(), 0);
        assert_eq!(cx.project().targets().count(), 0);
    }
    Ok(())
}

#[test]
fn custom_command_flags_preserve_arguments() -> TestResult {
    let mut f = Fixture::new();
    f.configure(
        r#"add_custom_command(OUTPUT out COMMAND tool VERBATIM "$<IF:$<CONFIG:Debug>,a;b,c>" "\"argument\"")"#,
        0,
    )?;
    let mut cx = f.interp.cx();
    let project = cx.project();
    let (_, command) = project.commands().next().ok_or("Missing command")?;
    assert_eq!(
        command.command_lines,
        vec![vec![
            BString::from("tool"),
            BString::from("$<IF:$<CONFIG:Debug>,a;b,c>"),
            BString::from("\"argument\""),
        ]]
    );
    Ok(())
}

#[test]
fn custom_rule_templates_keep_scope_and_arguments() -> TestResult {
    let mut f = Fixture::new();
    f.configure(
        r#"
        function(configure_source rule target fileset output file patterns)
        endfunction()
        add_custom_rule(example OUTPUT "<BASE_NAME>.gen"
            COMMAND tool "<SOURCE>" "arg;list" BYPRODUCTS "<BASE_NAME>.extra" GLOBAL)
        add_custom_rule(derived FROM_RULE example
            CONFIGURATOR FOR_SOURCE configure_source CHAIN)
        "#,
        0,
    )?;
    let mut cx = f.interp.cx();
    let model = cx.project();
    let rule = model
        .custom_rule(DirId::ROOT, b"derived")
        .ok_or("Missing rule")?;
    assert_eq!(
        rule.get(b"OUTPUT_FILE_SET_BASE_DIRS"),
        Some(&BString::from("<CURRENT_BINARY_DIR>"))
    );
    assert_eq!(
        model.global.props.get(b"_RCMAKE_CUSTOM_RULE_example").map(|value| value.as_slice()),
        Some(br#"["OUTPUT","<BASE_NAME>.gen","COMMAND","tool","<SOURCE>","arg;list","BYPRODUCTS","<BASE_NAME>.extra","GLOBAL"]"#.as_slice()),
    );
    assert_eq!(
        model
            .dir(DirId::ROOT)
            .props
            .get(b"_RCMAKE_CUSTOM_RULE_derived")
            .map(|value| value.as_slice()),
        Some(
            br#"["FROM_RULE","example","CONFIGURATOR","FOR_SOURCE","configure_source","CHAIN"]"#
                .as_slice()
        ),
    );
    assert_eq!(model.commands().count(), 0);
    Ok(())
}

#[test]
fn build_events_reject_targets_from_other_directories() -> TestResult {
    let mut f = Fixture::new();
    let mut cx = f.interp.cx();
    let project = cx.project();
    let dir = project.add_directory(
        DirId::ROOT,
        from_native(&f.source.join("sub")),
        from_native(&f.binary.join("sub")),
        Default::default(),
    )?;
    let target = project.add_target(Target::new(
        "other".into(),
        TargetKind::StaticLibrary,
        TargetOrigin::Normal,
        dir,
        Default::default(),
        Default::default(),
    ))?;

    f.configure(
        "add_custom_command(TARGET other POST_BUILD COMMAND tool)",
        1,
    )?;
    assert!(
        f.host
            .stream_bytes(Stream::Stderr)
            .to_string()
            .contains("was not created in this directory")
    );
    let mut cx = f.interp.cx();
    let project = cx.project();
    assert_eq!(project.commands().count(), 0);
    assert!(
        project
            .target(target)
            .ok_or("missing target")?
            .event_commands
            .is_empty()
    );
    Ok(())
}

#[test]
fn append_to_a_plain_missing_output_fails_at_the_call() -> TestResult {
    let mut f = Fixture::new();
    f.configure(
        "function(f)\n  add_custom_command(OUTPUT missing APPEND COMMAND tool)\nendfunction()\nf()",
        1,
    )?;
    let text = f.host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        text.contains("not already a custom command output"),
        "{text}"
    );
    assert!(!text.contains("DEFERRED"), "{text}");
    Ok(())
}
