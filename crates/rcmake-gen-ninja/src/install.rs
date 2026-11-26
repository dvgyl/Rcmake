use super::{BString, Edge, Graph, Ninja, Programs, clean::clean_command, qualified};

pub(super) fn emit_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
    scope: Scope<'_, '_>,
) {
    let root_path = rcmake_host::path::from_native(graph.build_dir);
    let root = graph.commands.path(&root_path);
    emit_directory_utilities(ninja, graph, programs, rebuild, scope);
    if graph.config.is_none() {
        let clean_name = graph.node(b"clean");
        if !graph.has_target(&clean_name) {
            if graph.clean_files.is_empty() {
                ninja.edge(
                    b"CLEAN",
                    Edge {
                        outputs: std::slice::from_ref(&clean_name),
                        ..Edge::default()
                    },
                );
                if entry != b"build.ninja" {
                    let mut file_arg = b"-f ".to_vec();
                    graph.commands.word(&graph.node(entry), &mut file_arg);
                    ninja.binding(b"FILE_ARG", &file_arg);
                }
            } else {
                let mut clean = programs.ninja.clone();
                if entry != b"build.ninja" {
                    clean.extend_from_slice(b" -f ");
                    graph.commands.word(&graph.node(entry), &mut clean);
                }
                clean.extend_from_slice(b" -t clean");
                let clean = clean_command(graph, &programs.cmake, clean, &graph.clean_files);
                ninja.edge(
                    b"CLEAN_CUSTOM",
                    Edge {
                        outputs: std::slice::from_ref(&clean_name),
                        ..Edge::default()
                    },
                );
                ninja.binding(b"COMMAND", &clean);
            }
        }
    }
    let help_name = graph.node(b"help");
    if !graph.has_target(&help_name) {
        ninja.edge(
            b"HELP",
            Edge {
                outputs: std::slice::from_ref(&help_name),
                ..Edge::default()
            },
        );
    }
    for (name, config_file, enabled, inputs) in [
        (
            "package",
            b"./CPackConfig.cmake".as_slice(),
            programs.package,
            programs.package_inputs.as_slice(),
        ),
        (
            "package_source",
            b"./CPackSourceConfig.cmake",
            programs.package_source,
            &[],
        ),
    ] {
        if enabled {
            let mut command = programs.cpack.clone();
            if let Some(config) = graph.config {
                command.extend_from_slice(b" -C ");
                graph.commands.word(config, &mut command);
            }
            command.extend_from_slice(b" --config ");
            graph.commands.word(config_file, &mut command);
            emit_utility(
                ninja,
                graph,
                Utility {
                    dir_prefix: "",
                    utility: name,
                    name,
                    command: &command,
                    cwd: Some(root.as_slice()),
                    inputs,
                    pool: b"console",
                    rule: b"CUSTOM_COMMAND",
                    edit_fallback: false,
                },
            );
        }
    }
}

/// How a manifest writes the utilities that depend on a configuration.
#[derive(Clone, Copy)]
pub(super) enum Scope<'a, 'g> {
    /// A single-configuration manifest writes all utilities.
    Single,
    /// A Ninja Multi-Config entry file aliases the configuration utilities.
    Entry {
        /// The configurations of the plain names.
        aliases: &'a [&'a Graph<'g>],
        /// The configurations of the `:all` names.
        all: &'a [&'a Graph<'g>],
    },
}

/// Write the configuration-qualified `test` and `install` utilities of
/// `graph` into a Ninja Multi-Config implementation file.
pub(super) fn emit_config_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    programs: &Programs,
    rebuild: &[u8],
) {
    for spec in directory_utilities(graph, programs, rebuild) {
        if spec.per_config && !graph.has_target(&graph.node(spec.name.as_bytes())) {
            emit_qualified_utility(ninja, graph, &spec);
        }
    }
}

fn emit_directory_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    programs: &Programs,
    rebuild: &[u8],
    scope: Scope<'_, '_>,
) {
    let specs = directory_utilities(graph, programs, rebuild);
    let mut parallel = Vec::new();
    for spec in &specs {
        if spec.name.ends_with("install/local") {
            parallel.push(graph.node(spec.name.as_bytes()));
        }
        match scope {
            Scope::Entry { aliases, all } if spec.per_config => {
                let name = graph.node(spec.name.as_bytes());
                if graph.has_target(&name) {
                    continue;
                }
                let inputs = |configs: &[&Graph<'_>]| {
                    configs
                        .iter()
                        .map(|config| qualified(&name, config.config))
                        .collect::<Vec<_>>()
                };
                ninja.phony(&name, &inputs(aliases));
                if !all.is_empty() {
                    ninja.phony(&qualified(&name, Some(b"all")), &inputs(all));
                }
            }
            _ if spec.rule == b"phony" => {
                let name = graph.node(spec.name.as_bytes());
                if !graph.has_target(&name) {
                    ninja.phony(&name, &[]);
                }
            }
            _ => emit_utility(
                ninja,
                graph,
                Utility {
                    dir_prefix: &spec.dir_prefix,
                    utility: &spec.utility,
                    name: &spec.name,
                    command: &spec.command,
                    cwd: spec.cwd.as_ref().map(|value| value.as_slice()),
                    inputs: &spec.inputs,
                    pool: spec.pool,
                    rule: spec.rule,
                    edit_fallback: spec.edit_fallback,
                },
            ),
        }
    }
    if programs.install && programs.parallel_install {
        let name = graph.node(b"install/parallel");
        ninja.phony(&name, &parallel);
        if graph.config.is_some() {
            let qualified_inputs: Vec<_> = parallel
                .iter()
                .map(|input| qualified(input, graph.config))
                .collect();
            ninja.phony(&qualified(&name, graph.config), &qualified_inputs);
        }
    }
}

/// One utility of a directory.
struct Spec {
    dir_prefix: String,
    utility: String,
    name: String,
    command: Vec<u8>,
    cwd: Option<BString>,
    inputs: Vec<BString>,
    pool: &'static [u8],
    rule: &'static [u8],
    /// The command uses the configuration of the manifest.
    per_config: bool,
    /// Fallback echo command when `CMAKE_EDIT_COMMAND` is not configured.
    edit_fallback: bool,
}

fn directory_utilities(graph: &Graph<'_>, programs: &Programs, rebuild: &[u8]) -> Vec<Spec> {
    let root_path = rcmake_host::path::from_native(graph.build_dir);
    let root = graph.commands.path(&root_path);
    let dirs =
        std::iter::once(BString::default()).chain(programs.directories.iter().filter_map(|dir| {
            graph
                .commands
                .beneath(&graph.commands.path(dir), &root)
                .map(BString::from)
        }));
    let mut specs = Vec::new();
    for directory in dirs {
        let prefix = if directory.is_empty() {
            ""
        } else {
            std::str::from_utf8(&directory).unwrap_or("")
        };
        if !directory.is_empty() && prefix.is_empty() {
            continue;
        }
        let name = |name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            }
        };
        let cwd = if directory.is_empty() {
            Some(root.clone())
        } else {
            Some(
                graph
                    .commands
                    .path(&[root.as_slice(), b"/", &directory].concat()),
            )
        };
        let mut test = programs.ctest.clone();
        for argument in &programs.ctest_arguments {
            test.push(b' ');
            graph.commands.word(argument, &mut test);
        }
        if let Some(config) = graph.config {
            test.extend_from_slice(b" -C ");
            graph.commands.word(config, &mut test);
        }
        let (edit_cache, edit_fallback) =
            if let (Some(editor), Some(source)) = (&programs.edit_cache, &programs.source_dir) {
                let mut command = editor.clone();
                for (option, dir) in [(b" -S".as_slice(), source.as_slice()), (b" -B", &root)] {
                    command.extend_from_slice(option);
                    graph.commands.word(dir, &mut command);
                }
                (command, false)
            } else {
                let mut command = programs.cmake.clone();
                command.extend_from_slice(b" -E echo ");
                if graph.commands.windows() {
                    graph
                        .commands
                        .word(b"No interactive CMake dialog available.", &mut command);
                } else {
                    command.extend_from_slice(br"No\ interactive\ CMake\ dialog\ available.");
                }
                (command, true)
            };

        let all = BString::from(name("all"));
        for (utility, command, enabled, per_config, pool, fallback) in [
            (
                "test",
                test,
                programs.testing,
                true,
                b"console".as_slice(),
                false,
            ),
            (
                "edit_cache",
                edit_cache,
                true,
                false,
                if edit_fallback {
                    b"".as_slice()
                } else {
                    b"console".as_slice()
                },
                edit_fallback,
            ),
            (
                "rebuild_cache",
                rebuild.to_vec(),
                true,
                false,
                b"console".as_slice(),
                false,
            ),
        ] {
            if enabled {
                specs.push(Spec {
                    dir_prefix: prefix.to_owned(),
                    utility: utility.to_owned(),
                    name: name(utility),
                    command,
                    cwd: cwd.clone(),
                    inputs: if utility == "test" && programs.test_depends_all {
                        vec![all.clone()]
                    } else {
                        Vec::new()
                    },
                    pool,
                    rule: b"CUSTOM_COMMAND",
                    per_config,
                    edit_fallback: fallback,
                });
            }
        }
        if !programs.install
            || (!directory.is_empty()
                && !programs.install_directories.iter().any(|dir| {
                    graph.commands.beneath(&graph.commands.path(dir), &root)
                        == Some(directory.as_slice())
                }))
        {
            continue;
        }
        specs.push(Spec {
            dir_prefix: prefix.to_owned(),
            utility: "list_install_components".to_owned(),
            name: name("list_install_components"),
            command: Vec::new(),
            cwd: None,
            inputs: Vec::new(),
            pool: b"",
            rule: b"phony",
            per_config: false,
            edit_fallback: false,
        });
        let mut install = programs.cmake.clone();
        if let Some(config) = graph.config {
            install.push(b' ');
            if graph.commands.windows() {
                graph.commands.word(
                    &[b"-DCMAKE_INSTALL_CONFIG_NAME=", config].concat(),
                    &mut install,
                );
            } else {
                install.extend_from_slice(b"-DCMAKE_INSTALL_CONFIG_NAME=");
                graph.commands.word(config, &mut install);
            }
        }
        for (utility, flags, enabled) in [
            ("install", b"".as_slice(), true),
            ("install/local", b" -DCMAKE_INSTALL_LOCAL_ONLY=1", true),
            (
                "install/strip",
                b" -DCMAKE_INSTALL_DO_STRIP=1",
                programs.strip,
            ),
        ] {
            if !enabled {
                continue;
            }
            let mut command = install.clone();
            command.extend_from_slice(flags);
            command.extend_from_slice(b" -P cmake_install.cmake");
            specs.push(Spec {
                dir_prefix: prefix.to_owned(),
                utility: utility.to_owned(),
                name: name(utility),
                command,
                cwd: cwd.clone(),
                inputs: vec![all.clone()],
                pool: if utility == "install/local" && programs.parallel_install {
                    b""
                } else {
                    b"console"
                },
                rule: b"CUSTOM_COMMAND",
                per_config: true,
                edit_fallback: false,
            });
        }
    }
    specs
}

fn utility_output_path(dir_prefix: &str, config: Option<&[u8]>, utility: &str) -> Vec<u8> {
    let mut path = Vec::new();
    if !dir_prefix.is_empty() {
        path.extend_from_slice(dir_prefix.as_bytes());
        path.push(b'/');
    }
    path.extend_from_slice(b"CMakeFiles/");
    if let Some(config) = config {
        path.extend_from_slice(config);
        path.push(b'/');
    }
    path.extend_from_slice(utility.as_bytes());
    path.extend_from_slice(b".util");
    path
}

/// Write a utility with the output `<dir>/CMakeFiles/<config>/<utility>.util` and the
/// alias `<name>:<config>`.
fn emit_qualified_utility(ninja: &mut Ninja, graph: &Graph<'_>, spec: &Spec) {
    let name = graph.node(spec.name.as_bytes());
    let output = graph.node(&utility_output_path(
        &spec.dir_prefix,
        graph.config,
        &spec.utility,
    ));
    let inputs: Vec<_> = spec
        .inputs
        .iter()
        .map(|input| qualified(&graph.node(input), graph.config))
        .collect();
    ninja.edge(
        spec.rule,
        Edge {
            outputs: std::slice::from_ref(&output),
            inputs: &inputs,
            ..Edge::default()
        },
    );
    write_command(
        ninja,
        graph,
        &spec.command,
        spec.cwd.as_ref().map(|value| value.as_slice()),
    );
    if !spec.pool.is_empty() {
        ninja.binding(b"pool", spec.pool);
    }
    write_description(ninja, &spec.utility, &name, spec.edit_fallback);
    ninja.binding(b"restat", b"1");
    ninja.phony(&qualified(&name, graph.config), &[output]);
}

struct Utility<'a> {
    dir_prefix: &'a str,
    utility: &'a str,
    name: &'a str,
    command: &'a [u8],
    cwd: Option<&'a [u8]>,
    inputs: &'a [BString],
    pool: &'a [u8],
    rule: &'a [u8],
    edit_fallback: bool,
}

fn emit_utility(ninja: &mut Ninja, graph: &Graph<'_>, utility: Utility<'_>) {
    let name = graph.node(utility.name.as_bytes());
    if graph.has_target(&name) {
        return;
    }
    let output = graph.node(&utility_output_path(
        utility.dir_prefix,
        None,
        utility.utility,
    ));
    let inputs: Vec<_> = utility
        .inputs
        .iter()
        .map(|input| graph.node(input))
        .collect();
    ninja.edge(
        utility.rule,
        Edge {
            outputs: std::slice::from_ref(&output),
            inputs: &inputs,
            ..Edge::default()
        },
    );
    write_command(ninja, graph, utility.command, utility.cwd);
    if !utility.pool.is_empty() {
        ninja.binding(b"pool", utility.pool);
    }
    if utility.rule == b"CUSTOM_COMMAND" {
        write_description(ninja, utility.utility, &name, utility.edit_fallback);
        ninja.binding(b"restat", b"1");
    }
    ninja.phony(&name, std::slice::from_ref(&output));
    if graph.config.is_some() {
        ninja.phony(&qualified(&name, graph.config), &[output]);
    }
}

fn write_command(ninja: &mut Ninja, graph: &Graph<'_>, command: &[u8], cwd: Option<&[u8]>) {
    if command.is_empty() {
        return;
    }
    if let Some(directory) = cwd {
        ninja.binding(
            b"COMMAND",
            &graph.commands.in_cmake_directory(command, directory),
        );
    } else {
        ninja.binding(b"COMMAND", &graph.in_build_dir(command));
    }
}

fn write_description(ninja: &mut Ninja, utility: &str, node: &[u8], edit_fallback: bool) {
    let description: &[u8] = match utility {
        "test" => b"Running tests...",
        "edit_cache" if edit_fallback => b"No interactive CMake dialog available...",
        "edit_cache" => b"Running CMake cache editor...",
        "rebuild_cache" => b"Running CMake to regenerate build system...",
        "install" => b"Install the project...",
        "install/local" => b"Installing only the local directory...",
        "install/strip" => b"Installing the project stripped...",
        "package" => b"Run CPack packaging tool...",
        "package_source" => b"Run CPack packaging tool for source...",
        _ => node,
    };
    ninja.binding(b"DESC", description);
}
