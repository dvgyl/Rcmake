// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ninja edges for device linking.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::slice::from_ref;

use rcmake_base::BString;
use rcmake_host::path;
use rcmake_resolve::{BuildStep, DeviceLinkMode, Invocation};

use crate::graph::Graph;
use crate::syntax::{Edge, Ninja, shell_word};

/// Rules shared by the device-link edges of all configurations.
pub(crate) const RULES: &str = "
rule CUDA_DEVICE_LINK
  command = $COMMAND
  description = $DESC

rule CUDA_DEVICE_LINK_CUBIN
  command = $COMMAND
  description = Linking CUDA device image $out

rule CUDA_DEVICE_LINK_FATBINARY
  command = $COMMAND
  description = Generating CUDA fatbinary $out

rule CUDA_DEVICE_LINK_STUB
  command = $COMMAND
  description = Compiling CUDA device link stub $out
";

/// Write the edges for a device-link step. Return whether this module handled it.
///
/// Pass the file-only link dependencies and normalized order-only inputs.
/// A driver
/// invocation keeps the resolver's argument order. For explicit linking, the
/// first planned cubin owns the registration header even if its edge sorts later.
pub(crate) fn emit(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    step: &BuildStep,
    link_depends: &[BString],
    order_only: &[&BString],
) -> bool {
    let BuildStep::DeviceLink {
        lang,
        output,
        inputs,
        mode,
    } = step
    else {
        return false;
    };
    let inputs: Vec<_> = inputs
        .iter()
        .map(|input| graph.node(input))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let implicit_inputs: Vec<_> = link_depends
        .iter()
        .map(|input| graph.node(input))
        .filter(|input| inputs.binary_search(input).is_err())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let order_only: Vec<_> = order_only
        .iter()
        .copied()
        .filter(|input| {
            inputs.binary_search(input).is_err() && implicit_inputs.binary_search(input).is_err()
        })
        .collect();
    let output = graph.node(output);
    match mode {
        DeviceLinkMode::Driver { invocation } => {
            ninja.edge(
                b"CUDA_DEVICE_LINK",
                Edge {
                    outputs: from_ref(&output),
                    inputs: &inputs,
                    implicit_inputs: &implicit_inputs,
                    order_only: &order_only,
                    ..Edge::default()
                },
            );
            ninja.invocation(graph, invocation);
            ninja.binding(
                b"DESC",
                format!("Linking {lang} device code {output}").as_bytes(),
            );
        }
        DeviceLinkMode::Explicit {
            cubins,
            fatbinary,
            fatbin_header,
            register_header,
            compile,
        } => {
            let register_header = graph.node(register_header);
            let fatbin_header = graph.node(fatbin_header);
            let mut ordered: Vec<_> = cubins.iter().enumerate().collect();
            ordered.sort_by(|(_, lhs), (_, rhs)| {
                lhs.arch
                    .cmp(&rhs.arch)
                    .then_with(|| lhs.output.cmp(&rhs.output))
            });
            let mut images = Vec::with_capacity(ordered.len());
            for (index, cubin) in ordered {
                let image = graph.node(&cubin.output);
                ninja.edge(
                    b"CUDA_DEVICE_LINK_CUBIN",
                    Edge {
                        outputs: from_ref(&image),
                        implicit_outputs: if index == 0 {
                            from_ref(&register_header)
                        } else {
                            &[]
                        },
                        inputs: &inputs,
                        implicit_inputs: &implicit_inputs,
                        order_only: &order_only,
                        ..Edge::default()
                    },
                );
                ninja.invocation(graph, &cubin.invocation);
                images.push(image);
            }
            ninja.edge(
                b"CUDA_DEVICE_LINK_FATBINARY",
                Edge {
                    outputs: from_ref(&fatbin_header),
                    inputs: &images,
                    ..Edge::default()
                },
            );
            ninja.invocation(graph, fatbinary);
            ninja.edge(
                b"CUDA_DEVICE_LINK_STUB",
                Edge {
                    outputs: from_ref(&output),
                    inputs: &[fatbin_header, register_header],
                    ..Edge::default()
                },
            );
            ninja.invocation(graph, compile);
        }
    }
    true
}

impl Ninja {
    /// Bind a lowered command and run it from the build directory.
    pub(crate) fn invocation(&mut self, graph: &Graph<'_>, invocation: &Invocation) {
        if cfg!(windows) {
            let build_dir = (!graph.prefix.is_empty()).then(|| path::from_native(graph.build_dir));
            self.binding(
                b"COMMAND",
                &windows_invocation(invocation, build_dir.as_ref()),
            );
            return;
        }
        let mut command = Vec::new();
        if let Some(directory) = &invocation.working_dir {
            command.extend_from_slice(b"cd -- ");
            shell_word(directory, &mut command);
            command.extend_from_slice(b" && ");
        }
        if invocation.shell.is_empty() {
            if let Some((program, args)) = invocation.argv.split_first() {
                shell_word(program, &mut command);
                for arg in args {
                    command.push(b' ');
                    shell_word(arg, &mut command);
                }
            } else {
                command.push(b':');
            }
        } else {
            if invocation.working_dir.is_some() {
                command.push(b'(');
            }
            command.extend_from_slice(&invocation.shell);
            if invocation.working_dir.is_some() {
                command.push(b')');
            }
        }
        self.binding(b"COMMAND", &graph.in_build_dir(&command));
    }
}

/// Quote one argument for the Windows executable or response-file parser.
pub(crate) fn windows_argument(word: &[u8], out: &mut Vec<u8>) {
    if !word.is_empty()
        && word
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=@\\".contains(byte))
    {
        out.extend_from_slice(word);
        return;
    }
    out.push(b'"');
    let mut slashes = 0;
    for &byte in word {
        if byte == b'\\' {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            b'\\',
            if byte == b'"' {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        out.push(byte);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\', 2 * slashes));
    out.push(b'"');
}

fn windows_invocation(invocation: &Invocation, build_dir: Option<&BString>) -> Vec<u8> {
    let mut script = Vec::new();
    for directory in build_dir.into_iter().chain(&invocation.working_dir) {
        // CD parses the quoted path itself, including a trailing backslash.
        script.extend_from_slice(b"cd /D \"");
        script.extend_from_slice(directory);
        script.extend_from_slice(b"\" && ");
    }
    let chain =
        invocation
            .argv
            .starts_with(&["cmd.exe".into(), "/D".into(), "/S".into(), "/C".into()]);
    let uses_shell = chain || invocation.argv.is_empty();
    let needs_shell = uses_shell || !script.is_empty();
    if uses_shell {
        if invocation.shell.is_empty() {
            script.extend_from_slice(b"ver >NUL");
        } else {
            script.extend_from_slice(&invocation.shell);
        }
    } else {
        for (index, word) in invocation.argv.iter().enumerate() {
            if index != 0 {
                script.push(b' ');
            }
            windows_argument(word, &mut script);
        }
    }
    if needs_shell {
        [b"cmd.exe /D /S /V:OFF /C \"".as_slice(), &script, b"\""].concat()
    } else {
        script
    }
}

#[cfg(test)]
mod tests {
    use rcmake_gen_common::GenError;
    use rcmake_resolve::{BuildGraph, CubinLink};
    use std::{collections::BTreeMap, path::Path};

    use super::*;

    fn invocation(args: &[&str]) -> Invocation {
        Invocation {
            argv: args.iter().copied().map(BString::from).collect(),
            ..Invocation::default()
        }
    }

    #[test]
    fn device_driver_preserves_argument_order_and_escapes_paths() -> Result<(), GenError> {
        let graph = BuildGraph::default();
        let graph = Graph::new(&graph, &BTreeMap::new(), Path::new("/build"), None)?;
        let step = BuildStep::DeviceLink {
            lang: "CUDA".into(),
            output: "obj/device link.o".into(),
            inputs: vec!["z.a".into(), "a $.o".into(), "z.a".into()],
            mode: DeviceLinkMode::Driver {
                invocation: invocation(&["cuda nvcc", "-dlink", "z.a", "a $.o"]),
            },
        };
        let mut ninja = Ninja::default();
        assert!(emit(
            &mut ninja,
            &graph,
            &step,
            &["device script.ld".into(), "z.a".into()],
            &[&"order".into()]
        ));
        let text = String::from_utf8_lossy(&ninja.0);
        assert!(text.contains(
            "build obj/device$ link.o: CUDA_DEVICE_LINK a$ $$.o z.a | device$ script.ld || order\n"
        ));
        let argv = if cfg!(windows) {
            "  COMMAND = \"cuda$ nvcc\"$ -dlink$ z.a$ \"a$ $$.o\"\n"
        } else {
            "  COMMAND = 'cuda$ nvcc'$ -dlink$ z.a$ 'a$ $$.o'\n"
        };
        assert!(text.contains(argv));
        Ok(())
    }

    #[test]
    fn explicit_edges_connect_cubins_headers_and_stub() -> Result<(), GenError> {
        let graph = BuildGraph::default();
        let graph = Graph::new(&graph, &BTreeMap::new(), Path::new("/build"), None)?;
        let step = BuildStep::DeviceLink {
            lang: "CUDA".into(),
            output: "device.o".into(),
            inputs: vec!["kernel.o".into()],
            mode: DeviceLinkMode::Explicit {
                cubins: vec![
                    CubinLink {
                        arch: "90".into(),
                        output: "sm_90.cubin".into(),
                        invocation: invocation(&["nvlink", "--register-link-binaries=reg.h"]),
                    },
                    CubinLink {
                        arch: "75".into(),
                        output: "sm_75.cubin".into(),
                        invocation: invocation(&["nvlink", "-arch=sm_75"]),
                    },
                ],
                fatbinary: invocation(&["fatbinary"]),
                fatbin_header: "fat.h".into(),
                register_header: "reg.h".into(),
                compile: invocation(&["clang", "link.stub"]),
            },
        };
        let mut ninja = Ninja::default();
        assert!(emit(&mut ninja, &graph, &step, &["device.ld".into()], &[]));
        let text = String::from_utf8_lossy(&ninja.0);
        let edges: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("build "))
            .collect();
        assert_eq!(
            edges,
            [
                "build sm_75.cubin: CUDA_DEVICE_LINK_CUBIN kernel.o | device.ld",
                "build sm_90.cubin | reg.h: CUDA_DEVICE_LINK_CUBIN kernel.o | device.ld",
                "build fat.h: CUDA_DEVICE_LINK_FATBINARY sm_75.cubin sm_90.cubin",
                "build device.o: CUDA_DEVICE_LINK_STUB fat.h reg.h",
            ]
        );
        assert_eq!(text.matches("device.ld").count(), 2);
        Ok(())
    }

    #[test]
    fn windows_quotes_program_arguments_and_working_directory() {
        let mut call = invocation(&[r"C:\Program Files\NVIDIA\nvcc.exe", r"obj\a b.o"]);
        call.working_dir = Some(r"D:\build files\".into());
        assert_eq!(
            windows_invocation(&call, None),
            br#"cmd.exe /D /S /V:OFF /C "cd /D "D:\build files\" && "C:\Program Files\NVIDIA\nvcc.exe" "obj\a b.o"""#
        );
    }
}
