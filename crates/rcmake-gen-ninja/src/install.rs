use rcmake_base::path_lex::Lexical;

use super::{BString, Edge, Graph, Ninja, Programs, qualified, shell_word};

pub(super) fn emit_utilities(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    entry: &[u8],
    programs: &Programs,
    rebuild: &[u8],
) {
    emit_directory_utilities(ninja, graph, programs, rebuild);
    let mut clean = b"ninja".to_vec();
    if entry != b"build.ninja" {
        clean.extend_from_slice(b" -f ");
        shell_word(&graph.node(entry), &mut clean);
    }
    let mut help = clean.clone();
    clean.extend_from_slice(b" -t clean");
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
                shell_word(config, &mut command);
            }
            command.extend_from_slice(b" --config ");
            shell_word(config_file, &mut command);
            emit_utility(
                ninja,
                graph,
                Utility {
                    name,
                    command: &command,
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
    let root = Lexical::new(&root_path);
    let dirs = std::iter::once(BString::default()).chain(
        programs
            .directories
            .iter()
            .filter_map(|dir| Lexical::new(dir).relative(root)),
    );
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
        let command = |program: &[u8]| {
            let mut result = Vec::new();
            if !directory.is_empty() {
                result.extend_from_slice(b"cd ");
                shell_word(&directory, &mut result);
                result.extend_from_slice(b" && ");
            }
            result.extend_from_slice(program);
            result
        };
        let mut test = command(&programs.ctest);
        test.extend_from_slice(b" --force-new-ctest-process");
        if let Some(config) = graph.config {
            test.extend_from_slice(b" -C ");
            shell_word(config, &mut test);
        }
        let mut edit_cache = command(&programs.cmake);
        edit_cache.extend_from_slice(b" -E echo ");
        shell_word(b"No interactive CMake dialog available.", &mut edit_cache);

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
                        inputs: &[],
                        pool: b"console",
                        rule: b"CUSTOM_COMMAND",
                    },
                );
            }
        }
        if !programs.install
            || (!directory.is_empty()
                && !programs
                    .install_directories
                    .iter()
                    .any(|dir| Lexical::new(dir).relative(root).as_ref() == Some(&directory)))
        {
            continue;
        }
        let mut install = command(&programs.cmake);
        if let Some(config) = graph.config {
            install.extend_from_slice(b" -DCMAKE_INSTALL_CONFIG_NAME=");
            shell_word(config, &mut install);
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
