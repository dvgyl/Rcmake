// SPDX-License-Identifier: MIT OR Apache-2.0
//! Ninja rules and edges for Fortran dependency discovery.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::slice::from_ref;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_resolve::{BuildStep, DependInfo, FortranTargetInfo, Invocation};

use crate::commands::windows_argument;
use crate::graph::Graph;
use crate::syntax::{Edge, Ninja, shell_word};

pub(crate) fn uses_fortran(graph: &rcmake_resolve::BuildGraph) -> bool {
    graph
        .steps
        .iter()
        .any(|step| matches!(step, BuildStep::Collate { lang, .. } if lang == "Fortran"))
}

/// Rules for scanning and collation.
///
/// The scanner uses a depfile without a deps log. Ninja cannot use a deps
/// log for the preprocessing edge because the edge has two outputs.
pub(crate) const RULES: &str = "
rule Fortran_PREPROCESS_SCAN
  command = $COMMAND
  description = Building Fortran preprocessed $out
  depfile = $DEP_FILE

rule Fortran_SCAN
  command = $COMMAND
  description = Generating Fortran dependencies for $in
  depfile = $DEP_FILE

rule Fortran_DYNDEP
  command = $COMMAND
  description = Generating Fortran dyndep file $out
  rspfile = $RSP_FILE
  rspfile_content = $RSP_ARGS
  restat = 1
";

/// Render a Fortran scan step. Return whether this module handled it.
///
/// Pass the owning target's information and the compile prerequisites of
/// the original source. The invocation already contains the scanner command
/// and any preprocessing commands.
pub(crate) fn emit_scan(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    step: &BuildStep,
    info: &FortranTargetInfo,
    implicit_inputs: &[BString],
    order_only: &[&BString],
) -> bool {
    let BuildStep::Scan {
        lang,
        source,
        ddi,
        preprocessed,
        invocation,
        depfile,
        ..
    } = step
    else {
        return false;
    };
    if lang != "Fortran" {
        return false;
    }
    let source = graph.node(source);
    let ddi = graph.node(ddi);
    let preprocessed = preprocessed.as_ref().map(|path| graph.node(path));
    let (rule, primary, byproduct) = match &preprocessed {
        Some(path) => (b"Fortran_PREPROCESS_SCAN".as_slice(), path, from_ref(&ddi)),
        None => (b"Fortran_SCAN".as_slice(), &ddi, [].as_slice()),
    };
    let tdi = graph.node(&info.depend_info);
    let implicit_inputs = unique(std::iter::once(tdi).chain(implicit_inputs.iter().cloned()));
    ninja.edge(
        rule,
        Edge {
            outputs: from_ref(primary),
            implicit_outputs: byproduct,
            inputs: from_ref(&source),
            implicit_inputs: &implicit_inputs,
            order_only,
        },
    );
    ninja.invocation(graph, invocation);
    if let Some(depfile) = depfile {
        ninja.binding(b"DEP_FILE", &graph.node(depfile));
    }
    true
}

/// Render a Fortran dyndep edge. Return whether this module handled it.
///
/// Pass the path to the configured CMake command as a raw path. An optional
/// module map format is emitted only if the step requests one. The Fortran
/// collation step normally has no module maps.
pub(crate) fn emit_collate(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    step: &BuildStep,
    cmake: &[u8],
) -> bool {
    let BuildStep::Collate {
        lang,
        ddis,
        dd,
        tdi,
        modmap_format,
        implicit_inputs,
        depend_info,
        ..
    } = step
    else {
        return false;
    };
    if lang != "Fortran" || !matches!(depend_info.as_ref(), DependInfo::Fortran(_)) {
        return false;
    }
    let modules = Lexical::new(Lexical::new(dd).parent()).append(b"FortranModules.json");
    let output = graph.node(dd);
    let modules = graph.node(&modules);
    let inputs = unique(ddis.iter().map(|path| graph.node(path)));
    let implicit_inputs = unique(
        std::iter::once(graph.node(tdi)).chain(implicit_inputs.iter().map(|path| graph.node(path))),
    );
    ninja.edge(
        b"Fortran_DYNDEP",
        Edge {
            outputs: from_ref(&output),
            implicit_outputs: from_ref(&modules),
            inputs: &inputs,
            implicit_inputs: &implicit_inputs,
            ..Edge::default()
        },
    );

    let rspfile = [dd.as_slice(), b".rsp"].concat();
    let mut argv = vec![
        cmake.into(),
        "-E".into(),
        "cmake_ninja_dyndep".into(),
        option(b"--tdi=", tdi),
        "--lang=Fortran".into(),
        option(b"--dd=", dd),
        option(b"@", &rspfile),
    ];
    if let Some(format) = modmap_format {
        argv.push(option(b"--modmapfmt=", format.as_str().as_bytes()));
    }
    ninja.invocation(
        graph,
        &Invocation {
            argv,
            ..Invocation::default()
        },
    );
    ninja.binding(b"RSP_FILE", &graph.node(&rspfile));

    // The command opens the response file and the scans from the physical
    // build directory. Scan records carry the separate manifest object names.
    let mut response = Vec::new();
    let mut seen = BTreeSet::new();
    for ddi in ddis.iter().filter(|ddi| seen.insert(*ddi)) {
        if !response.is_empty() {
            response.push(b' ');
        }
        response_argument(ddi, &mut response, graph.commands.windows());
    }
    ninja.binding(b"RSP_ARGS", &response);
    true
}

/// Render a Fortran object edge after its dyndep file is ready.
///
/// Pass the normalized implicit and order-only prerequisites from the graph.
/// The collation tool marks objects that write modules with `restat = 1` in
/// the dyndep file. The compile rule retains its own compiler depfile.
pub(crate) fn emit_compile(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    step: &BuildStep,
    implicit_inputs: &[BString],
    order_only: &[&BString],
) -> bool {
    let BuildStep::Compile {
        object,
        source,
        compile_input,
        invocation,
        depfile,
        modmap: None,
        dyndep: Some(dd),
        ..
    } = step
    else {
        return false;
    };
    let scan_only = compile_input
        .is_none()
        .then(|| graph.node(&[object.as_slice(), b".ddi"].concat()));
    let implicit_inputs = unique(implicit_inputs.iter().cloned().chain(scan_only));
    let object = graph.node(object);
    let source = graph.node(compile_input.as_ref().unwrap_or(source));
    let dd = graph.node(dd);
    let order_only: Vec<_> = std::iter::once(&dd)
        .chain(
            order_only
                .iter()
                .copied()
                .filter(|path| **path != dd && !implicit_inputs.contains(path)),
        )
        .collect();
    let rule = if depfile.is_some() {
        b"COMPILE_DEPFILE".as_slice()
    } else {
        b"COMPILE"
    };
    ninja.edge(
        rule,
        Edge {
            outputs: from_ref(&object),
            inputs: from_ref(&source),
            implicit_inputs: &implicit_inputs,
            order_only: &order_only,
            ..Edge::default()
        },
    );
    ninja.invocation(graph, invocation);
    ninja.binding(
        b"DESC",
        &rcmake_resolve::progress::compile_description(Some(b"Fortran"), &object),
    );
    ninja.binding(b"dyndep", &dd);
    if let Some(depfile) = depfile {
        ninja.binding(b"DEP_FILE", &graph.node(depfile));
    }
    true
}

fn option(flag: &[u8], path: &[u8]) -> BString {
    [flag, path].concat().into()
}

fn response_argument(path: &[u8], out: &mut Vec<u8>, windows: bool) {
    if windows {
        windows_argument(path, out, false);
    } else {
        shell_word(path, out);
    }
}

fn unique(paths: impl IntoIterator<Item = BString>) -> Vec<BString> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use rcmake_gen_common::GenError;
    use rcmake_resolve::BuildGraph;
    use std::{collections::BTreeMap, path::Path};

    use super::*;

    #[test]
    fn scan_and_compile_keep_distinct_dependencies() -> Result<(), GenError> {
        let graph = BuildGraph::default();
        let graph = Graph::new(&graph, &BTreeMap::new(), Path::new("/build"), None)?;
        let info = FortranTargetInfo {
            depend_info: "obj/FortranDependInfo.json".into(),
            dyndep: "obj/Fortran.dd".into(),
            ..FortranTargetInfo::default()
        };
        let mut ninja = Ninja::default();
        let command = Invocation {
            shell: "fc -E input && cmake -E cmake_ninja_depends".into(),
            ..Invocation::default()
        };
        let scan = BuildStep::Scan {
            lang: "Fortran".into(),
            source: "/src/a $.F90".into(),
            ddi: "obj/a.o.ddi".into(),
            object: "obj/a.o".into(),
            preprocessed: Some("obj/a.F90-pp.f90".into()),
            invocation: command.clone(),
            depfile: Some("obj/a.F90-pp.f90.d".into()),
        };
        assert!(emit_scan(&mut ninja, &graph, &scan, &info, &[], &[]));
        let compile = BuildStep::Compile {
            byproducts: Vec::new(),
            object: "obj/a.o".into(),
            source: "/src/a $.F90".into(),
            compile_input: Some("obj/a.F90-pp.f90".into()),
            invocation: command,
            depfile: Some("obj/a.o.d".into()),
            modmap: None,
            dyndep: Some(info.dyndep),
        };
        assert!(emit_compile(&mut ninja, &graph, &compile, &[], &[]));
        let text = String::from_utf8_lossy(&ninja.0);
        assert!(text.contains("build obj/a.F90-pp.f90 | obj/a.o.ddi: Fortran_PREPROCESS_SCAN /src/a$ $$.F90 | obj/FortranDependInfo.json"));
        assert!(text.contains("  DEP_FILE = obj/a.F90-pp.f90.d\n"));
        assert!(text.contains("build obj/a.o: COMPILE_DEPFILE obj/a.F90-pp.f90 || obj/Fortran.dd"));
        assert!(text.contains("  dyndep = obj/Fortran.dd\n  DEP_FILE = obj/a.o.d\n"));
        Ok(())
    }

    #[test]
    fn scan_only_object_has_a_normal_ddi_prerequisite() -> Result<(), GenError> {
        let input = BuildGraph::default();
        let graph = Graph::with_prefix(
            &input,
            &BTreeMap::new(),
            Path::new("/build/prefix"),
            None,
            b"prefix",
            crate::commands::Commands::default(),
        )?;
        let mut ninja = Ninja::default();
        let compile = BuildStep::Compile {
            object: "obj/a.o".into(),
            source: "src/a.f90".into(),
            byproducts: Vec::new(),
            compile_input: None,
            invocation: Invocation::default(),
            depfile: None,
            modmap: None,
            dyndep: Some("obj/Fortran.dd".into()),
        };
        assert!(emit_compile(&mut ninja, &graph, &compile, &[], &[]));
        let text = String::from_utf8_lossy(&ninja.0);
        assert!(text.contains(
            "build prefix/obj/a.o: COMPILE prefix/src/a.f90 | prefix/obj/a.o.ddi || prefix/obj/Fortran.dd\n"
        ));
        Ok(())
    }

    #[test]
    fn collate_declares_the_module_summary_and_linked_target_inputs()
    -> Result<(), Box<dyn std::error::Error>> {
        let input = rcmake_resolve::BuildGraph::default();
        let step = BuildStep::Collate {
            lang: "Fortran".into(),
            target: "app".into(),
            ddis: vec!["obj/a $.ddi".into()],
            dd: "obj/Fortran.dd".into(),
            modmaps: Vec::new(),
            tdi: "obj/FortranDependInfo.json".into(),
            implicit_inputs: vec!["lib/FortranModules.json".into()],
            modmap_format: None,
            depend_info: Box::new(DependInfo::Fortran(rcmake_fortran::FortranBuildTdi {
                dir_cur_src: "/src".into(),
                dir_top_src: "/src".into(),
                forward_modules_from_target_dirs: Vec::new(),
                building_intrinsic_modules: false,
                source_formats: BTreeMap::new(),
                defines: Vec::new(),
                scan: rcmake_fortran::FortranTdi {
                    dir_cur_bld: "/build".into(),
                    dir_top_bld: "/build".into(),
                    include_dirs: Vec::new(),
                    module_dir: None,
                    linked_target_dirs: vec!["lib".into()],
                    compiler_id: "GNU".into(),
                    submodule_sep: "@".into(),
                    submodule_ext: ".smod".into(),
                    module_case: rcmake_fortran::ModuleCase::Lower,
                    sources: Vec::new(),
                },
            })),
        };
        let graph = Graph::new(&input, &BTreeMap::new(), Path::new("/build"), None)?;
        let mut ninja = Ninja::default();
        assert!(emit_collate(&mut ninja, &graph, &step, b"/my tools/cmake"));
        let text = String::from_utf8_lossy(&ninja.0);
        assert!(text.contains("build obj/Fortran.dd | obj/FortranModules.json: Fortran_DYNDEP obj/a$ $$.ddi | obj/FortranDependInfo.json lib/FortranModules.json"));
        assert!(text.contains("cmake_ninja_dyndep --tdi=obj/FortranDependInfo.json --lang=Fortran --dd=obj/Fortran.dd @obj/Fortran.dd.rsp"));
        // The graph targets a POSIX system on every host.
        assert!(text.contains("  RSP_ARGS = 'obj/a $$.ddi'\n"), "{text}");
        assert!(RULES.contains("restat = 1"));
        Ok(())
    }

    #[test]
    fn windows_response_file_preserves_spaces_backslashes_and_quotes() {
        let mut response = Vec::new();
        response_argument(br#"C:\build files\a.ddi"#, &mut response, true);
        assert_eq!(response, br#""C:\build files\a.ddi""#);
        response.clear();
        response_argument(br#"C:\some folder\"#, &mut response, true);
        assert_eq!(response, br#""C:\some folder\\""#);
    }
}
