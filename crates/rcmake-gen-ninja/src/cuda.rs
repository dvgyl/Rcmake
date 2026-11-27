// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ninja edges for device linking.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::slice::from_ref;

use rcmake_base::BString;
use rcmake_resolve::{BuildStep, DeviceLinkMode};

use crate::graph::Graph;
use crate::syntax::{Edge, Ninja};

/// Rules shared by the device-link edges of all configurations.
pub(crate) const RULES: &str = "
rule CUDA_DEVICE_LINK
  command = $COMMAND
  description = $DESC

rule CUDA_DEVICE_LINK_RSP
  command = $COMMAND
  description = $DESC
  rspfile = $RSP_FILE
  rspfile_content = $RSP_ARGS

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
    let raw_output = output;
    let output = graph.node(output);
    match mode {
        DeviceLinkMode::Driver {
            invocation,
            response_file,
        } => {
            // Match `cmNinjaNormalTargetGenerator.cxx:1114-1133`: ignore
            // `CMAKE_NINJA_FORCE_RESPONSE_FILE` on CUDA device links and trigger a
            // response file only when the full rendered command (including the fixed
            // driver prefix and flags) exceeds the platform command-line length budget.
            let rendered = graph.command_line(from_ref(invocation), false);
            let rsp: BString = [raw_output.as_slice(), b".rsp"].concat().into();
            let response = response_file
                .as_ref()
                .filter(|_| graph.command_exceeds_budget(&rendered))
                .and_then(|response| response.rewrite(invocation, &rsp));
            let rule: &[u8] = if response.is_some() {
                b"CUDA_DEVICE_LINK_RSP"
            } else {
                b"CUDA_DEVICE_LINK"
            };
            ninja.edge(
                rule,
                Edge {
                    outputs: from_ref(&output),
                    inputs: &inputs,
                    implicit_inputs: &implicit_inputs,
                    order_only: &order_only,
                    ..Edge::default()
                },
            );
            if let Some((command, args)) = response {
                let mut content = Vec::new();
                for arg in args {
                    if !content.is_empty() {
                        content.push(b' ');
                    }
                    graph.commands.word(&arg, &mut content);
                }
                ninja.invocation(graph, &command);
                ninja.binding(b"RSP_FILE", &graph.node(&rsp));
                ninja.binding(b"RSP_ARGS", &content);
            } else {
                ninja.invocation(graph, invocation);
            }
            if let Some(pool) = graph.pools.get(raw_output) {
                ninja.binding(b"pool", pool);
            }
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

#[cfg(test)]
mod tests {
    use rcmake_gen_common::GenError;
    use rcmake_resolve::{BuildGraph, CubinLink, Invocation};
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
                response_file: None,
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
        let argv = "  COMMAND = 'cuda nvcc' -dlink z.a 'a $$.o'\n";
        assert!(text.contains(argv), "{text}");
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
        let command = crate::commands::Commands::for_windows(b"cmd.exe").line(
            &[call],
            false,
            b"C:/build",
            false,
        );
        let script = String::from_utf8_lossy(&command);
        assert!(script.starts_with("cmd.exe /D /S /V:OFF /C "));
        assert!(script.contains(r#""C:\Program Files\NVIDIA\nvcc.exe" "obj\a b.o""#));
        assert!(script.contains("cd /D "));
        assert!(script.contains("build files"));
    }

    #[test]
    fn device_response_file_ignores_global_force_and_triggers_on_command_budget()
    -> Result<(), GenError> {
        let build = Path::new(if cfg!(windows) { "C:/build" } else { "/build" });
        let input = BuildGraph::default();
        let mut graph = Graph::new(&input, &BTreeMap::new(), build, None)?;
        graph.command_settings.force = true;
        let step = BuildStep::DeviceLink {
            lang: "CUDA".into(),
            output: "device.o".into(),
            inputs: vec!["a b.o".into(), "c.o".into()],
            mode: DeviceLinkMode::Driver {
                invocation: invocation(&[
                    "nvcc", "-dlink", "a b.o", "c.o", "-o", "device.o", "-lcudart",
                ]),
                response_file: Some(rcmake_resolve::DeviceLinkResponseFile {
                    flag: "--options-file ".into(),
                    objects: vec!["a b.o".into(), "c.o".into()],
                    libraries: vec!["-lcudart".into()],
                }),
            },
        };
        // Short device link stays inline even when `CMAKE_NINJA_FORCE_RESPONSE_FILE` is set.
        let mut short_ninja = Ninja::default();
        assert!(emit(&mut short_ninja, &graph, &step, &[], &[]));
        let short_text = String::from_utf8_lossy(&short_ninja.0);
        assert!(short_text.contains("CUDA_DEVICE_LINK "), "{short_text}");
        assert!(!short_text.contains("CUDA_DEVICE_LINK_RSP"), "{short_text}");

        // Exceeding the platform command-line budget (deducting driver command prefix)
        // switches to `CUDA_DEVICE_LINK_RSP` and preserves input order and flag.
        graph.command_settings.force = false;
        graph.commands.posix_limit = 40;
        let mut long_ninja = Ninja::default();
        assert!(emit(&mut long_ninja, &graph, &step, &[], &[]));
        let long_text = String::from_utf8_lossy(&long_ninja.0);
        assert!(long_text.contains("CUDA_DEVICE_LINK_RSP"), "{long_text}");
        assert!(
            long_text.contains("COMMAND = nvcc -dlink --options-file device.o.rsp -o device.o"),
            "{long_text}"
        );
        assert!(
            long_text.contains("RSP_ARGS = 'a b.o' c.o -lcudart"),
            "{long_text}"
        );
        Ok(())
    }
}
