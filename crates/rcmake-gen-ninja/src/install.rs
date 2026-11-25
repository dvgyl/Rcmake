use super::{BString, Edge, Graph, Ninja, Programs, clean::clean_command, qualified};

pub(super) fn emit_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
    scope: Scope<'_, '_>,
) {
    emit_directory_utilities(ninja, graph, programs, rebuild, scope);
    let mut clean = programs.ninja.clone();
    if entry != b"build.ninja" {
        clean.extend_from_slice(b" -f ");
        graph.commands.word(&graph.node(entry), &mut clean);
    }
    let mut help = clean.clone();
    clean.extend_from_slice(b" -t clean");
    let clean = clean_command(graph, &programs.cmake, clean, &graph.clean_files);
    help.extend_from_slice(b" -t targets");
    for (name, command, inputs, pool, rule) in [
        ("clean", clean, vec![], b"".as_slice(), b"CLEAN".as_slice()),
        ("help", help, vec![], b"".as_slice(), b"HELP".as_slice()),
    ] {
        if name == "clean" && graph.config.is_some() {
            continue;
        }
        emit_utility(
            ninja,
            graph,
            Utility {
                name,
                command: &command,
                cwd: None,
                inputs: &inputs,
                pool,
                rule,
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
                    name,
                    command: &command,
                    cwd: None,
                    inputs,
                    pool: b"console",
                    rule: b"CUSTOM_COMMAND",
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
            _ => emit_utility(
                ninja,
                graph,
                Utility {
                    name: &spec.name,
                    command: &spec.command,
                    cwd: spec.cwd.as_ref().map(|value| value.as_slice()),
                    inputs: &spec.inputs,
                    pool: spec.pool,
                    rule: spec.rule,
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
    name: String,
    command: Vec<u8>,
    cwd: Option<BString>,
    inputs: Vec<BString>,
    pool: &'static [u8],
    rule: &'static [u8],
    /// The command uses the configuration of the manifest.
    per_config: bool,
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
        let cwd = (!directory.is_empty()).then(|| {
            if graph.prefix.is_empty() {
                directory.clone()
            } else {
                graph
                    .commands
                    .path(&[root.as_slice(), b"/", &directory].concat())
            }
        });
        let mut test = programs.ctest.clone();
        test.extend_from_slice(b" --force-new-ctest-process");
        for argument in &programs.ctest_arguments {
            test.push(b' ');
            graph.commands.word(argument, &mut test);
        }
        if let Some(config) = graph.config {
            test.extend_from_slice(b" -C ");
            graph.commands.word(config, &mut test);
        }
        let mut edit_cache = programs.cmake.clone();
        edit_cache.extend_from_slice(b" -E echo ");
        graph
            .commands
            .word(b"No interactive CMake dialog available.", &mut edit_cache);

        for (utility, command, enabled, per_config) in [
            ("test", test, programs.testing, true),
            ("rebuild_cache", rebuild.to_vec(), true, false),
            ("edit_cache", edit_cache, true, false),
        ] {
            if enabled {
                specs.push(Spec {
                    name: name(utility),
                    command,
                    cwd: cwd.clone().filter(|_| utility != "rebuild_cache"),
                    inputs: Vec::new(),
                    pool: b"console",
                    rule: b"CUSTOM_COMMAND",
                    per_config,
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
        let all = BString::from(name("all"));
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
            });
        }
    }
    specs
}

/// Write a utility with the output `CMakeFiles/<config>/<name>.util` and the
/// alias `<name>:<config>`.
fn emit_qualified_utility(ninja: &mut Ninja, graph: &Graph<'_>, spec: &Spec) {
    let config = graph.config.unwrap_or_default();
    let name = graph.node(spec.name.as_bytes());
    let mut output_path = b"CMakeFiles/".to_vec();
    output_path.extend_from_slice(config);
    output_path.push(b'/');
    output_path.extend_from_slice(spec.name.as_bytes());
    output_path.extend_from_slice(b".util");
    let output = graph.node(&output_path);
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
        &spec.name,
        &spec.command,
        spec.cwd.as_ref().map(|value| value.as_slice()),
    );
    if !spec.pool.is_empty() {
        ninja.binding(b"pool", spec.pool);
    }
    write_description(ninja, &spec.name, &name);
    ninja.phony(&qualified(&name, graph.config), &[output]);
}

struct Utility<'a> {
    name: &'a str,
    command: &'a [u8],
    cwd: Option<&'a [u8]>,
    inputs: &'a [BString],
    pool: &'a [u8],
    rule: &'a [u8],
}

fn emit_utility(ninja: &mut Ninja, graph: &Graph<'_>, utility: Utility<'_>) {
    let name = graph.node(utility.name.as_bytes());
    if graph.has_target(&name) {
        return;
    }
    let output = graph.node(format!("CMakeFiles/{}.util", utility.name).as_bytes());
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
    write_command(ninja, graph, utility.name, utility.command, utility.cwd);
    if !utility.pool.is_empty() {
        ninja.binding(b"pool", utility.pool);
    }
    if utility.rule == b"CUSTOM_COMMAND" {
        write_description(ninja, utility.name, &name);
    }
    ninja.phony(&name, std::slice::from_ref(&output));
    if graph.config.is_some() {
        ninja.phony(&qualified(&name, graph.config), &[output]);
    }
}

fn write_command(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    name: &str,
    command: &[u8],
    cwd: Option<&[u8]>,
) {
    if command.is_empty() {
        return;
    }
    if matches!(name, "clean" | "help") {
        ninja.binding(b"COMMAND", command);
    } else if let Some(directory) = cwd {
        ninja.binding(b"COMMAND", &graph.commands.in_directory(command, directory));
    } else {
        ninja.binding(b"COMMAND", &graph.in_build_dir(command));
    }
}

fn write_description(ninja: &mut Ninja, name: &str, node: &[u8]) {
    let description = if name.ends_with("install/local") {
        b"Installing only the local directory...".as_slice()
    } else {
        node
    };
    ninja.binding(b"DESC", description);
}
