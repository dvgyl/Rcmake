use super::{BString, Edge, Graph, Ninja, Programs, clean::clean_command, qualified};

pub(super) fn emit_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
) {
    emit_directory_utilities(ninja, graph, programs, rebuild);
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

fn emit_directory_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    programs: &Programs,
    rebuild: &[u8],
) {
    let root_path = rcmake_host::path::from_native(graph.build_dir);
    let root = graph.commands.path(&root_path);
    let dirs =
        std::iter::once(BString::default()).chain(programs.directories.iter().filter_map(|dir| {
            graph
                .commands
                .beneath(&graph.commands.path(dir), &root)
                .map(BString::from)
        }));
    let mut parallel = Vec::new();
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
        if let Some(config) = graph.config {
            test.extend_from_slice(b" -C ");
            graph.commands.word(config, &mut test);
        }
        let mut edit_cache = programs.cmake.clone();
        edit_cache.extend_from_slice(b" -E echo ");
        graph
            .commands
            .word(b"No interactive CMake dialog available.", &mut edit_cache);

        for (utility, cmd, enabled) in [
            ("test", test.as_slice(), programs.testing),
            ("rebuild_cache", rebuild, true),
            ("edit_cache", edit_cache.as_slice(), true),
        ] {
            if enabled {
                emit_utility(
                    ninja,
                    graph,
                    Utility {
                        name: &name(utility),
                        command: cmd,
                        cwd: cwd
                            .as_ref()
                            .map(|value| value.as_slice())
                            .filter(|_| utility != "rebuild_cache"),
                        inputs: &[],
                        pool: b"console",
                        rule: b"CUSTOM_COMMAND",
                    },
                );
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
            let mut cmd = install.clone();
            cmd.extend_from_slice(flags);
            cmd.extend_from_slice(b" -P cmake_install.cmake");
            emit_utility(
                ninja,
                graph,
                Utility {
                    name: &name(utility),
                    command: &cmd,
                    cwd: cwd.as_ref().map(|value| value.as_slice()),
                    inputs: std::slice::from_ref(&all),
                    pool: if utility == "install/local" && programs.parallel_install {
                        b""
                    } else {
                        b"console"
                    },
                    rule: b"CUSTOM_COMMAND",
                },
            );
        }
        parallel.push(graph.node(name("install/local").as_bytes()));
    }
    if programs.install && programs.parallel_install {
        let name = graph.node(b"install/parallel");
        ninja.phony(&name, &parallel);
        if graph.config.is_some() {
            ninja.phony(&qualified(&name, graph.config), &parallel);
        }
    }
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
    if !utility.command.is_empty() {
        if matches!(utility.name, "clean" | "help") {
            ninja.binding(b"COMMAND", utility.command);
        } else if let Some(directory) = utility.cwd {
            ninja.binding(
                b"COMMAND",
                &graph.commands.in_directory(utility.command, directory),
            );
        } else {
            ninja.binding(b"COMMAND", &graph.in_build_dir(utility.command));
        }
    }
    if !utility.pool.is_empty() {
        ninja.binding(b"pool", utility.pool);
    }
    if utility.rule == b"CUSTOM_COMMAND" {
        let description = if utility.name.ends_with("install/local") {
            b"Installing only the local directory...".as_slice()
        } else {
            name.as_slice()
        };
        ninja.binding(b"DESC", description);
    }
    ninja.phony(&name, std::slice::from_ref(&output));
    if graph.config.is_some() {
        ninja.phony(&qualified(&name, graph.config), &[output]);
    }
}
