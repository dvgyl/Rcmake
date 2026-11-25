use std::path::Path;
use std::time::Duration;

use rcmake_host::{Follow, PureHost, ReadRange};
use rcmake_resolve::{CustomStep, GraphTarget, Invocation};

use super::*;

fn invocation(shell: &str) -> Invocation {
    Invocation {
        shell: shell.into(),
        ..Invocation::default()
    }
}

fn graph_target(name: &str, steps: Vec<usize>, depends: Vec<usize>, in_all: bool) -> GraphTarget {
    GraphTarget {
        name: name.into(),
        steps,
        depends,
        in_all,
    }
}

fn read(host: &dyn Host, path: &Path) -> String {
    let contents = host
        .read(path, ReadRange::All { limit: 128 * 1024 })
        .expect("read file");
    String::from_utf8(contents).expect("read UTF-8")
}

#[test]
fn writes_hierarchy_and_escapes_make_and_shell_words() {
    let root = Path::new("/build");
    let host = PureHost::new(root.into(), "/tools/rcmake".into());
    let mut result = GenerationResult {
        graphs: vec![BuildGraph {
            steps: vec![
                BuildStep::Custom(CustomStep {
                    outputs: vec!["generated $/first.h".into(), "generated $/second.h".into()],
                    byproducts: vec!["generated $/byproduct".into()],
                    depends: vec!["input #1.txt".into()],
                    comment: Some("Create $ headers".into()),
                    invocations: vec![Invocation {
                        working_dir: Some("some dir".into()),
                        ..invocation("printf '%s' '$value'")
                    }],
                    depfile: Some("generated $/custom.d".into()),
                    ..CustomStep::default()
                }),
                BuildStep::Compile {
                    byproducts: Vec::new(),
                    compile_input: None,
                    object: "obj $/foo.o".into(),
                    source: "/src/source file.c".into(),
                    invocation: invocation("cc -o 'obj $/foo.o' -c '/src/source file.c'"),
                    depfile: Some("obj $/foo.d".into()),
                    modmap: None,
                    dyndep: None,
                },
            ],
            targets: vec![
                graph_target("tool $", vec![0, 1], vec![1], true),
                graph_target("generator", vec![0], Vec::new(), false),
                graph_target("manual", Vec::new(), Vec::new(), false),
            ],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    result.metadata.listfiles = vec![
        "/src/CMakeLists.txt".into(),
        "/src/config file.cmake".into(),
    ];
    generate_unix_makefiles(&result, &host, root).expect("generate");

    let top = read(&host, &root.join("Makefile"));
    for target in GLOBAL_TARGETS {
        let name = std::str::from_utf8(target).expect("target is ASCII");
        assert!(top.contains(&format!(".PHONY: {name}\n")), "{name}");
    }
    assert!(top.contains(".PHONY: tool\\ $$\n"));
    assert!(top.contains("$(CMAKE_COMMAND) -P cmake_install.cmake"));
    assert!(top.contains("$(CTEST_COMMAND) --force-new-ctest-process"));
    assert!(top.contains("$(CMAKE_COMMAND) -B . -S '/src' --regenerate-during-build"));
    assert!(top.contains(
        "CMakeFiles/rcmake-regenerate.stamp: /src/CMakeLists.txt /src/config\\ file.cmake"
    ));

    let dispatch = read(&host, &root.join("CMakeFiles/Makefile2"));
    assert!(dispatch.contains("all: CMakeFiles/tool\\ $$.dir/all\n"));
    assert!(dispatch.contains("CMakeFiles/tool\\ $$.dir/all: CMakeFiles/generator.dir/all\n"));
    assert!(dispatch.contains("$(MAKE) $(MAKE_SILENT) -f 'CMakeFiles/tool $$.dir/build.make'"));
    let compile = read(&host, &root.join("CMakeFiles/tool $.dir/build.make"));
    assert!(
        compile
            .contains("obj\\ $$/foo.o: /src/source\\ file.c CMakeFiles/tool\\ $$.dir/build.make")
    );
    assert!(compile.contains("mkdir -p -- 'obj $$'"));
    assert!(compile.contains("-include obj\\ $$/foo.d"));
    assert!(compile.contains("rm -f -- 'obj $$/foo.d'"));
    assert!(!compile.contains("Create $$ headers"));
    let custom = read(&host, &root.join("CMakeFiles/generator.dir/build.make"));
    assert!(custom.contains("generated\\ $$/first.h: input\\ \\#1.txt"));
    assert!(custom.contains("generated\\ $$/second.h: generated\\ $$/first.h"));
    assert!(custom.contains("(CDPATH= cd 'some dir' && printf '%s' '$$value')"));
    assert!(custom.contains("'Create $$ headers'"));
    assert!(custom.contains("-include generated\\ $$/custom.d"));
    assert!(custom.contains("rm -f -- 'generated $$/byproduct'"));

    let metadata = host
        .metadata(&root.join("Makefile"), Follow::Yes)
        .expect("metadata");
    host.sleep(Duration::from_secs(1));
    generate_unix_makefiles(&result, &host, root).expect("regenerate");
    let after = host
        .metadata(&root.join("Makefile"), Follow::Yes)
        .expect("metadata");
    assert_eq!(after.mtime, metadata.mtime);

    result.metadata.cmake_command = Some("/tools/rcmake".into());
    generate_with_model(&result, &host, root, &ProjectModel::default())
        .expect("generate with progress");
    let top = read(&host, &root.join("Makefile"));
    assert!(top.contains("cmake_progress_start CMakeFiles 2"));
    let dispatch = read(&host, &root.join("CMakeFiles/Makefile2"));
    assert!(dispatch.contains("--progress-num=1 'Built target tool $$'"));
    assert!(dispatch.contains("--progress-num=2 'Built target generator'"));
}

#[test]
fn rejects_invalid_dependency_before_writing_files() {
    let root = Path::new("/build");
    let host = PureHost::new(root.into(), "/tools/rcmake".into());
    let result = GenerationResult {
        graphs: vec![BuildGraph {
            targets: vec![graph_target("bad", Vec::new(), vec![5], true)],
            ..BuildGraph::default()
        }],
        ..GenerationResult::default()
    };
    assert!(generate_unix_makefiles(&result, &host, root).is_err());
    assert!(host.metadata(&root.join("Makefile"), Follow::Yes).is_err());
}

#[test]
fn custom_commands_without_a_comment_report_their_outputs() {
    let build = if cfg!(windows) { "C:/build" } else { "/build" };
    let step = CustomStep {
        outputs: vec![
            format!("{build}/out/a.h").into(),
            format!("{build}/b.h").into(),
        ],
        ..CustomStep::default()
    };
    assert_eq!(
        plan_step::default_comment(&step, Path::new(build)),
        Some("Generating out/a.h, b.h".into())
    );
    let commented = CustomStep {
        comment: Some(String::new().into()),
        ..step.clone()
    };
    assert_eq!(
        plan_step::default_comment(&commented, Path::new(build)),
        None
    );
    let symbolic = CustomStep {
        symbolic: true,
        ..step
    };
    assert_eq!(
        plan_step::default_comment(&symbolic, Path::new(build)),
        None
    );
}

#[test]
fn target_messages_follow_the_global_property_and_variable() {
    assert!(target_messages(None));
    let mut model = ProjectModel::default();
    assert!(target_messages(Some(&model)));
    model
        .global
        .props
        .set("TARGET_MESSAGES".into(), "OFF".into());
    assert!(!target_messages(Some(&model)));
}
