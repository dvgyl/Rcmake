// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unix Makefiles rules for CUDA device linking.

#![forbid(unsafe_code)]

use std::{collections::BTreeSet, path::Path};

use rcmake_base::{BString, path_lex::Lexical, shell::quote_always};
use rcmake_gen_common::GenError;
use rcmake_resolve::{BuildStep, DeviceLinkMode, Invocation};

use crate::{
    build_path, check_make_path, invalid_graph, make_recipe_text, progress, rule, shell_invocation,
};

/// Render one device-object rule. Return whether this module handled the step.
///
/// Supply the host link's file-only dependencies and the path to this
/// target's build.make file. The cubin commands run in the planned order.
/// Make stops the recipe if one of the commands fails.
///
/// # Errors
///
/// Returns an error if a dependency or a command cannot be written in Make.
pub(crate) fn emit(
    out: &mut Vec<u8>,
    build_dir: &Path,
    step: &BuildStep,
    link_depends: &[BString],
    build_file: &[u8],
    progress_number: Option<usize>,
) -> Result<bool, GenError> {
    let BuildStep::DeviceLink {
        lang,
        output,
        inputs,
        mode,
    } = step
    else {
        return Ok(false);
    };
    let output = build_path(output, build_dir);
    let inputs = prerequisites(inputs.iter().chain(link_depends), build_dir)?;
    check_make_path(&output)?;
    check_make_path(build_file)?;
    rule(
        out,
        &output,
        inputs
            .iter()
            .map(|value| value.as_slice())
            .chain([build_file]),
    );
    progress::report(
        out,
        format!("Linking {lang} device code {output}").as_bytes(),
        progress_number,
    )?;

    let mut directories = BTreeSet::new();
    let mut add_directory = |path: &[u8]| {
        let directory = Lexical::new(path).parent();
        if !directory.is_empty() {
            directories.insert(BString::from(directory));
        }
    };
    add_directory(&output);
    let mut invocations = Vec::new();
    match mode {
        DeviceLinkMode::Driver { invocation } => invocations.push(invocation),
        DeviceLinkMode::Explicit {
            cubins,
            fatbinary,
            fatbin_header,
            register_header,
            compile,
        } => {
            for cubin in cubins {
                add_directory(&cubin.output);
                invocations.push(&cubin.invocation);
            }
            add_directory(fatbin_header);
            add_directory(register_header);
            invocations.extend([fatbinary, compile]);
        }
    }
    for directory in directories {
        out.extend_from_slice(b"\t$(SILENT)mkdir -p -- ");
        make_recipe_text(out, &quote_always(&directory))?;
        out.push(b'\n');
    }
    for invocation in invocations {
        write_invocation(out, invocation)?;
    }
    Ok(true)
}

/// Add file-only prerequisites to a host link or archive rule.
///
/// # Errors
///
/// Returns an error if a path cannot be written in Make.
pub(crate) fn emit_link_depends(
    out: &mut Vec<u8>,
    build_dir: &Path,
    step: &BuildStep,
) -> Result<(), GenError> {
    if let BuildStep::Link {
        step,
        implicit_inputs,
        ..
    } = step
        && !implicit_inputs.is_empty()
    {
        let output = build_path(&step.output, build_dir);
        check_make_path(&output)?;
        let inputs = prerequisites(implicit_inputs, build_dir)?;
        rule(out, &output, inputs.iter().map(|value| value.as_slice()));
    }
    Ok(())
}

fn prerequisites<'a>(
    paths: impl IntoIterator<Item = &'a BString>,
    build_dir: &Path,
) -> Result<BTreeSet<BString>, GenError> {
    paths
        .into_iter()
        .map(|input| {
            let input = build_path(input, build_dir);
            check_make_path(&input)?;
            Ok(input)
        })
        .collect()
}

fn write_invocation(out: &mut Vec<u8>, invocation: &Invocation) -> Result<(), GenError> {
    let command = shell_invocation(invocation)
        .ok_or_else(|| invalid_graph("The device-link command is empty."))?;
    out.extend_from_slice(b"\t$(SILENT)");
    make_recipe_text(out, &command)?;
    out.push(b'\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use rcmake_resolve::CubinLink;

    use super::*;

    fn invocation(words: &[&str]) -> Invocation {
        Invocation {
            argv: words.iter().copied().map(BString::from).collect(),
            ..Invocation::default()
        }
    }

    fn render(mode: DeviceLinkMode) -> Result<String, GenError> {
        let step = BuildStep::DeviceLink {
            lang: "CUDA".into(),
            output: "obj/device link.o".into(),
            inputs: vec!["/build/obj/kernel.o".into()],
            mode,
        };
        let mut out = Vec::new();
        assert!(emit(
            &mut out,
            Path::new("/build"),
            &step,
            &["a $file.ld".into(), "a $file.ld".into()],
            b"CMakeFiles/app.dir/build.make",
            None,
        )?);
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn driver_adds_link_depends_only_to_the_rule() -> Result<(), GenError> {
        let text = render(DeviceLinkMode::Driver {
            invocation: invocation(&["nvcc", "-dlink", "obj/kernel.o"]),
        })?;
        assert!(text.starts_with(
            "obj/device\\ link.o: a\\ $$file.ld obj/kernel.o CMakeFiles/app.dir/build.make\n"
        ));
        assert!(text.contains("\t$(SILENT)('nvcc' '-dlink' 'obj/kernel.o')\n"));
        assert_eq!(text.matches("file.ld").count(), 1);
        Ok(())
    }

    #[test]
    fn explicit_device_link_runs_one_recipe_in_dependency_order() -> Result<(), GenError> {
        let text = render(DeviceLinkMode::Explicit {
            cubins: vec![
                CubinLink {
                    arch: "90".into(),
                    output: "obj/sm_90.cubin".into(),
                    invocation: invocation(&["nvlink", "sm90", "reg.h"]),
                },
                CubinLink {
                    arch: "75".into(),
                    output: "obj/sm_75.cubin".into(),
                    invocation: invocation(&["nvlink", "sm75"]),
                },
            ],
            fatbinary: invocation(&["fatbinary"]),
            fatbin_header: "obj/fat.h".into(),
            register_header: "obj/reg.h".into(),
            compile: invocation(&["clang", "link.stub"]),
        })?;
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("\t$(SILENT)("))
                .collect::<Vec<_>>(),
            [
                "\t$(SILENT)('nvlink' 'sm90' 'reg.h')",
                "\t$(SILENT)('nvlink' 'sm75')",
                "\t$(SILENT)('fatbinary')",
                "\t$(SILENT)('clang' 'link.stub')",
            ],
        );
        assert_eq!(
            text.lines().filter(|line| !line.starts_with('\t')).count(),
            1
        );
        Ok(())
    }
}
