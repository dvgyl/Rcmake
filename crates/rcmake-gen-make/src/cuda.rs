// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unix Makefiles rules for CUDA device linking.

#![forbid(unsafe_code)]

use std::{collections::BTreeSet, path::Path};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_resolve::{BuildStep, DeviceLinkMode, Invocation};

use crate::{
    FORCE, MakeDialect, Word, build_path, check_make_path_for, invalid_graph, make_recipe_text,
    make_recipe_text_for, make_word_for, progress, quote_arg, rule_for, shell_invocation_for,
};

/// Render one device-object rule. Return whether this module handled the step.
///
/// Supply the host link's file-only dependencies and the path to this
/// target's build.make file. The cubin commands run in the planned order.
/// Make stops the recipe if a command fails and reruns it if an intermediate
/// file is missing.
///
/// # Errors
///
/// Returns an error if a dependency or a command cannot be written in Make.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    build_dir: &Path,
    step: &BuildStep,
    link_depends: &[BString],
    build_file: &[u8],
    progress_number: Option<usize>,
    host: &dyn Host,
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
    let intermediates = intermediate_paths(mode).map(|path| build_path(path, build_dir));
    let output = build_path(output, build_dir);
    let mut inputs = prerequisites(dialect, inputs.iter().chain(link_depends), build_dir)?;
    let response = response_invocation(host, build_dir, &output, mode)?;
    if let Some((file, _)) = &response {
        inputs.insert(file.clone());
    }
    check_make_path_for(dialect, &output)?;
    check_make_path_for(dialect, build_file)?;
    rule_for(
        dialect,
        out,
        &output,
        inputs
            .iter()
            .map(|value| value.as_slice())
            .chain([build_file]),
    );
    progress::report(
        dialect,
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
        DeviceLinkMode::Driver { invocation, .. } => {
            invocations.push(response.as_ref().map_or(invocation, |(_, command)| command));
        }
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
        if dialect.is_nmake() {
            out.extend_from_slice(b"\t$(SILENT)$(CMAKE_COMMAND) -E make_directory ");
        } else {
            out.extend_from_slice(b"\t$(SILENT)mkdir -p -- ");
        }
        make_recipe_text_for(dialect, out, &quote_arg(dialect, &directory))?;
        out.push(b'\n');
    }
    for invocation in invocations {
        write_invocation(dialect, out, invocation)?;
    }
    for path in intermediates {
        check_make_path_for(dialect, &path)?;
        if dialect.is_nmake() {
            out.extend_from_slice(b"!IF !EXIST(\"");
            make_recipe_text(out, &path)?;
            out.extend_from_slice(b"\")\n");
            rule_for(dialect, out, &output, [FORCE]);
            out.extend_from_slice(b"!ENDIF\n");
        } else {
            out.extend_from_slice(b"ifeq ($(wildcard ");
            make_word_for(dialect, out, &path, Word::Wildcard);
            out.extend_from_slice(b"),)\n");
            rule_for(dialect, out, &output, [FORCE]);
            out.extend_from_slice(b"endif\n");
        }
    }
    Ok(true)
}

fn response_invocation(
    host: &dyn Host,
    build_dir: &Path,
    output: &[u8],
    mode: &DeviceLinkMode,
) -> Result<Option<(BString, Invocation)>, GenError> {
    let DeviceLinkMode::Driver {
        invocation,
        response_file: Some(response),
    } = mode
    else {
        return Ok(None);
    };
    let forced = host
        .env_get(b"CMAKE_CUDA_USE_RESPONSE_FILE_FOR_OBJECTS")
        .is_some_and(|value| rcmake_base::truth::is_on(&value));
    if !forced && invocation.shell.len() <= 7_500 {
        return Ok(None);
    }
    let file: BString = [output, b".rsp"].concat().into();
    let Some((command, args)) = response.rewrite(invocation, &file) else {
        return Ok(None);
    };
    let mut contents = Vec::new();
    for argument in args {
        if !contents.is_empty() {
            contents.push(b' ');
        }
        contents.extend_from_slice(&rcmake_base::shell::quote(&argument));
    }
    crate::write_makefile(host, build_dir, &file, &contents)?;
    Ok(Some((file, command)))
}

/// Return the device object and all intermediate files for Make clean.
pub(crate) fn clean_artifacts(step: &BuildStep, build_dir: &Path) -> Vec<BString> {
    let BuildStep::DeviceLink { output, mode, .. } = step else {
        return Vec::new();
    };
    std::iter::once(output)
        .chain(intermediate_paths(mode))
        .map(|path| build_path(path, build_dir))
        .collect()
}

fn intermediate_paths(mode: &DeviceLinkMode) -> impl Iterator<Item = &BString> {
    let (cubins, headers) = match mode {
        DeviceLinkMode::Explicit {
            cubins,
            fatbin_header,
            register_header,
            ..
        } => (cubins.as_slice(), Some([fatbin_header, register_header])),
        DeviceLinkMode::Driver { .. } => (&[][..], None),
    };
    cubins
        .iter()
        .map(|cubin| &cubin.output)
        .chain(headers.into_iter().flatten())
}

/// Add file-only prerequisites to a host link or archive rule.
///
/// # Errors
///
/// Returns an error if a path cannot be written in Make.
pub(crate) fn emit_link_depends(
    dialect: MakeDialect,
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
        check_make_path_for(dialect, &output)?;
        let inputs = prerequisites(dialect, implicit_inputs, build_dir)?;
        rule_for(
            dialect,
            out,
            &output,
            inputs.iter().map(|value| value.as_slice()),
        );
    }
    Ok(())
}

fn prerequisites<'a>(
    dialect: MakeDialect,
    paths: impl IntoIterator<Item = &'a BString>,
    build_dir: &Path,
) -> Result<BTreeSet<BString>, GenError> {
    paths
        .into_iter()
        .map(|input| {
            let input = build_path(input, build_dir);
            check_make_path_for(dialect, &input)?;
            Ok(input)
        })
        .collect()
}

fn write_invocation(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    invocation: &Invocation,
) -> Result<(), GenError> {
    let command = shell_invocation_for(dialect, invocation)
        .ok_or_else(|| invalid_graph("The device-link command is empty."))?;
    out.extend_from_slice(b"\t$(SILENT)");
    make_recipe_text_for(dialect, out, &command)?;
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

    fn render(mode: DeviceLinkMode) -> Result<(String, Vec<BString>), GenError> {
        let step = BuildStep::DeviceLink {
            lang: "CUDA".into(),
            output: "obj/device link.o".into(),
            inputs: vec!["/build/obj/kernel.o".into()],
            mode,
        };
        let products = clean_artifacts(&step, Path::new("/build"));
        let mut out = Vec::new();
        assert!(emit(
            MakeDialect::Unix,
            &mut out,
            Path::new("/build"),
            &step,
            &["a $file.ld".into(), "a $file.ld".into()],
            b"CMakeFiles/app.dir/build.make",
            None,
            &rcmake_host::PureHost::default(),
        )?);
        Ok((String::from_utf8_lossy(&out).into_owned(), products))
    }

    #[test]
    fn driver_adds_link_depends_only_to_the_rule() -> Result<(), GenError> {
        let (text, products) = render(DeviceLinkMode::Driver {
            response_file: None,
            invocation: invocation(&["nvcc", "-dlink", "obj/kernel.o"]),
        })?;
        assert!(text.starts_with(
            "obj/device\\ link.o: a\\ $$file.ld obj/kernel.o CMakeFiles/app.dir/build.make\n"
        ));
        assert!(text.contains("\t$(SILENT)('nvcc' '-dlink' 'obj/kernel.o')\n"));
        assert_eq!(text.matches("file.ld").count(), 1);
        assert_eq!(products, ["obj/device link.o"]);
        Ok(())
    }

    #[test]
    fn explicit_device_link_runs_one_recipe_in_dependency_order() -> Result<(), GenError> {
        let (text, products) = render(DeviceLinkMode::Explicit {
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
            products,
            [
                "obj/device link.o",
                "obj/sm_90.cubin",
                "obj/sm_75.cubin",
                "obj/fat.h",
                "obj/reg.h"
            ],
        );
        for intermediate in &products[1..] {
            assert!(text.contains(&format!("ifeq ($(wildcard {intermediate}),)\n")));
        }
        Ok(())
    }
}
