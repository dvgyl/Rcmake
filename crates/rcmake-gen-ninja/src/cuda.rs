// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ninja edges for device linking.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::slice::from_ref;

use rcmake_base::BString;
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
/// Pass the normalized order-only prerequisites from the graph. A driver
/// invocation keeps the resolver's argument order. For explicit linking, the
/// first planned cubin owns the registration header even if its edge sorts later.
pub(crate) fn emit(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    step: &BuildStep,
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
    let output = graph.node(output);
    match mode {
        DeviceLinkMode::Driver { invocation } => {
            ninja.edge(
                b"CUDA_DEVICE_LINK",
                Edge {
                    outputs: from_ref(&output),
                    inputs: &inputs,
                    order_only,
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
                        order_only,
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
        assert!(emit(&mut ninja, &graph, &step, &[&"order".into()]));
        let text = String::from_utf8_lossy(&ninja.0);
        assert!(text.contains("build obj/device$ link.o: CUDA_DEVICE_LINK a$ $$.o z.a || order\n"));
        assert!(text.contains("  COMMAND = 'cuda$ nvcc'$ -dlink$ z.a$ 'a$ $$.o'\n"));
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
        assert!(emit(&mut ninja, &graph, &step, &[]));
        let text = String::from_utf8_lossy(&ninja.0);
        let edges: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("build "))
            .collect();
        assert_eq!(
            edges,
            [
                "build sm_75.cubin: CUDA_DEVICE_LINK_CUBIN kernel.o",
                "build sm_90.cubin | reg.h: CUDA_DEVICE_LINK_CUBIN kernel.o",
                "build fat.h: CUDA_DEVICE_LINK_FATBINARY sm_75.cubin sm_90.cubin",
                "build device.o: CUDA_DEVICE_LINK_STUB fat.h reg.h",
            ]
        );
        Ok(())
    }
}
