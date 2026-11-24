// SPDX-License-Identifier: MIT OR Apache-2.0
//! CUDA and assembler items for Visual Studio projects.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::{msbuild, windows, xml::Writer};

/// CUDA compiler properties for one project configuration.
#[derive(Default)]
pub(crate) struct CompileOptions<'a> {
    /// The Visual Studio platform, such as x64.
    pub(crate) platform: &'a str,
    /// Whether the compiler must generate relocatable device code.
    pub(crate) separable: bool,
    /// Ordered include directories.
    pub(crate) includes: &'a [String],
    /// Preprocessor definitions.
    pub(crate) defines: &'a [String],
    /// Architecture flags produced by the CUDA architecture lowerer.
    pub(crate) architecture_flags: &'a [String],
    /// Other CUDA compiler options, quoted for the CUDA command line.
    pub(crate) additional_options: &'a str,
}

/// CUDA linker properties for one project configuration.
#[derive(Default)]
pub(crate) struct LinkOptions<'a> {
    /// Whether MSBuild must run the device linker.
    pub(crate) perform_device_link: bool,
    /// Extra object files and libraries for the device linker.
    pub(crate) dependencies: &'a [String],
    /// Device-link options, quoted for the CUDA command line.
    pub(crate) additional_options: &'a str,
}

/// Write the CUDA compile settings in an ItemDefinitionGroup.
pub(crate) fn compile_options(xml: &mut Writer, options: &CompileOptions<'_>) {
    xml.start("CudaCompile", &[]);
    // NVIDIA's targets can add virtual code to CodeGeneration entries.
    // Pass the complete architecture flags to nvcc through AdditionalOptions.
    xml.text("CodeGeneration", &[], "");
    if options.separable {
        xml.text("GenerateRelocatableDeviceCode", &[], "true");
    }
    if options.platform.eq_ignore_ascii_case("x64") {
        xml.text("TargetMachinePlatform", &[], "64");
    }
    include_paths(xml, "Include", options.includes, false);
    defines(xml, "Defines", options.defines);
    let flags = options
        .architecture_flags
        .iter()
        .map(String::as_str)
        .chain([options.additional_options]);
    additional_options(xml, flags, true);
    xml.end();
}

/// Write the CUDA link settings in an ItemDefinitionGroup.
pub(crate) fn link_options(xml: &mut Writer, options: &LinkOptions<'_>) {
    xml.start("CudaLink", &[]);
    let required = if options.perform_device_link {
        "true"
    } else {
        "false"
    };
    xml.text("PerformDeviceLink", &[], required);
    if options.perform_device_link && !options.dependencies.is_empty() {
        let values = options
            .dependencies
            .iter()
            .map(|path| msbuild(&windows(path)))
            .collect::<Vec<_>>();
        xml.text("AdditionalDependencies", &[], &values.join(";"));
    }
    additional_options(xml, [options.additional_options], false);
    xml.end();
}

/// The extension import group to which an import belongs.
#[derive(Clone, Copy)]
pub(crate) enum ImportKind {
    /// Import a props file in ExtensionSettings.
    Props,
    /// Import a targets file in ExtensionTargets.
    Targets,
}

impl ImportKind {
    fn extension(self) -> &'static str {
        match self {
            Self::Props => "props",
            Self::Targets => "targets",
        }
    }
}

const CUSTOMIZATIONS: &str = r"$(VCTargetsPath)\BuildCustomizations";

/// Write a CUDA build customization import in an open ImportGroup.
///
/// If set, `integration_dir` is the resolved directory that contains the
/// toolkit's MSBuild extension files.
pub(crate) fn cuda_import(
    xml: &mut Writer,
    kind: ImportKind,
    version: &str,
    integration_dir: Option<&str>,
) {
    let directory = integration_dir.filter(|dir| !dir.is_empty()).map_or_else(
        || CUSTOMIZATIONS.to_owned(),
        |dir| msbuild(&windows(dir.trim_end_matches(['/', '\\']))),
    );
    let project = format!(
        "{directory}\\CUDA {}.{}",
        msbuild(version),
        kind.extension()
    );
    xml.empty("Import", &[("Project", &project)]);
}

/// Write a MASM customization import in an open ImportGroup.
pub(crate) fn masm_import(xml: &mut Writer, kind: ImportKind) {
    let project = format!("{CUSTOMIZATIONS}\\masm.{}", kind.extension());
    xml.empty("Import", &[("Project", &project)]);
}

/// The selected assembler for a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Assembler {
    /// The Microsoft Macro Assembler.
    Masm,
    /// The Netwide Assembler.
    Nasm,
}

impl Assembler {
    /// Map the CMake language to a supported MSBuild assembler item.
    pub(crate) fn from_language(language: &[u8]) -> Option<Self> {
        match language {
            b"ASM_MASM" => Some(Self::Masm),
            b"ASM_NASM" => Some(Self::Nasm),
            _ => None,
        }
    }

    /// Return the MSBuild item name.
    pub(crate) fn item_name(self) -> &'static str {
        match self {
            Self::Masm => "MASM",
            Self::Nasm => "NASM",
        }
    }
}

/// Per-source metadata for a CUDA or assembler item.
#[derive(Default)]
pub(crate) struct SourceOptions<'a> {
    /// Project-relative file path.
    pub(crate) path: &'a str,
    /// Conditions for configurations that exclude this source.
    pub(crate) excluded_conditions: &'a [String],
    /// Source-level include directories.
    pub(crate) includes: &'a [String],
    /// Source-level preprocessor definitions.
    pub(crate) defines: &'a [String],
    /// Source-level command-line flags, already quoted as needed.
    pub(crate) additional_options: &'a str,
    /// Explicit object dependencies for a NASM source.
    pub(crate) dependencies: &'a [String],
}

/// Write a CUDA source item in an ItemGroup.
pub(crate) fn cuda_item(xml: &mut Writer, source: &SourceOptions<'_>) {
    source_item(xml, "CudaCompile", "Include", "Defines", false, source);
}

/// Write a MASM or NASM source item in an ItemGroup.
pub(crate) fn assembler_item(xml: &mut Writer, assembler: Assembler, source: &SourceOptions<'_>) {
    source_item(
        xml,
        assembler.item_name(),
        "IncludePaths",
        "PreprocessorDefinitions",
        assembler == Assembler::Nasm,
        source,
    );
}

fn source_item(
    xml: &mut Writer,
    item: &str,
    include_property: &str,
    define_property: &str,
    nasm: bool,
    source: &SourceOptions<'_>,
) {
    let path = msbuild(&windows(source.path));
    xml.start(item, &[("Include", &path)]);
    for condition in source.excluded_conditions {
        xml.text("ExcludedFromBuild", &[("Condition", condition)], "true");
    }
    include_paths(xml, include_property, source.includes, nasm);
    defines(xml, define_property, source.defines);
    additional_options(xml, [source.additional_options], true);
    if nasm && !source.dependencies.is_empty() {
        let inputs = source
            .dependencies
            .iter()
            .map(|path| msbuild(&windows(path)))
            .collect::<Vec<_>>();
        xml.text("AdditionalDependencies", &[], &inputs.join(";"));
    }
    xml.end();
}

fn include_paths(xml: &mut Writer, name: &str, paths: &[String], trailing_slash: bool) {
    if paths.is_empty() {
        return;
    }
    let values = paths
        .iter()
        .map(|path| {
            let mut path = windows(path);
            if trailing_slash && !path.ends_with('\\') {
                path.push('\\');
            }
            msbuild(&path)
        })
        .chain([format!("%({name})")])
        .collect::<Vec<_>>();
    xml.text(name, &[], &values.join(";"));
}

fn defines(xml: &mut Writer, name: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let mut seen = BTreeSet::new();
    let values = std::iter::once(format!("%({name})"))
        .chain(
            values
                .iter()
                .filter(|value| seen.insert(*value))
                .map(|value| msbuild(value)),
        )
        .collect::<Vec<_>>();
    xml.text(name, &[], &values.join(";"));
}

fn additional_options<'a>(
    xml: &mut Writer,
    flags: impl IntoIterator<Item = &'a str>,
    inherit: bool,
) {
    let flags = flags
        .into_iter()
        .filter(|flag| !flag.is_empty())
        .map(msbuild)
        .collect::<Vec<_>>();
    if flags.is_empty() {
        return;
    }
    let flags = flags.join(" ");
    let value = if inherit {
        format!("%(AdditionalOptions) {flags}")
    } else {
        flags
    };
    xml.text("AdditionalOptions", &[], &value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_compile_emits_exact_architecture_flags_and_inherits_metadata() {
        let mut xml = Writer::new(false, "utf-8");
        compile_options(
            &mut xml,
            &CompileOptions {
                platform: "x64",
                separable: true,
                includes: &["inc/a;b".into()],
                defines: &["SIZE=2".into(), "A&B".into(), "SIZE=2".into()],
                architecture_flags: &["--generate-code=arch=compute_75,code=sm_75".into()],
                additional_options: "-lineinfo",
            },
        );
        let text = xml.finish();
        assert!(text.contains("<CodeGeneration></CodeGeneration>"));
        assert!(
            text.contains("<GenerateRelocatableDeviceCode>true</GenerateRelocatableDeviceCode>")
        );
        assert!(text.contains("<TargetMachinePlatform>64</TargetMachinePlatform>"));
        assert!(text.contains(r"<Include>inc\a%3Bb;%(Include)</Include>"));
        assert!(text.contains("<Defines>%(Defines);SIZE=2;A&amp;B</Defines>"));
        assert!(text.contains("<AdditionalOptions>%(AdditionalOptions) --generate-code=arch=compute_75,code=sm_75 -lineinfo</AdditionalOptions>"));
    }

    #[test]
    fn link_options_override_the_msbuild_default() {
        let mut xml = Writer::new(false, "utf-8");
        link_options(&mut xml, &LinkOptions::default());
        link_options(
            &mut xml,
            &LinkOptions {
                perform_device_link: true,
                dependencies: &["obj/kernel.obj".into()],
                additional_options: "-Xnvlink=-v",
            },
        );
        let text = xml.finish();
        assert!(text.contains("<PerformDeviceLink>false</PerformDeviceLink>"));
        assert!(text.contains("<PerformDeviceLink>true</PerformDeviceLink>"));
        assert!(text.contains(r"<AdditionalDependencies>obj\kernel.obj</AdditionalDependencies>"));
        assert!(text.contains("<AdditionalOptions>-Xnvlink=-v</AdditionalOptions>"));
    }

    #[test]
    fn cuda_and_assembler_items_use_the_msbuild_tool_names() {
        let mut xml = Writer::new(false, "utf-8");
        cuda_import(&mut xml, ImportKind::Props, "12.6", None);
        cuda_import(
            &mut xml,
            ImportKind::Targets,
            "12.6",
            Some("C:/CUDA extensions/a&b"),
        );
        masm_import(&mut xml, ImportKind::Props);
        cuda_item(
            &mut xml,
            &SourceOptions {
                path: "gpu/kernel.cu",
                ..SourceOptions::default()
            },
        );
        assembler_item(
            &mut xml,
            Assembler::Masm,
            &SourceOptions {
                path: "asm/test.asm",
                ..SourceOptions::default()
            },
        );
        assembler_item(
            &mut xml,
            Assembler::Nasm,
            &SourceOptions {
                path: "asm/test.nasm",
                includes: &["asm/include".into()],
                dependencies: &["asm/table.inc".into()],
                ..SourceOptions::default()
            },
        );
        let text = xml.finish();
        assert!(text.contains(r#"Project="$(VCTargetsPath)\BuildCustomizations\CUDA 12.6.props""#));
        assert!(text.contains(r#"Project="C:\CUDA extensions\a&amp;b\CUDA 12.6.targets""#));
        assert!(text.contains(r#"Project="$(VCTargetsPath)\BuildCustomizations\masm.props""#));
        assert!(text.contains(r#"<CudaCompile Include="gpu\kernel.cu">"#));
        assert!(text.contains(r#"<MASM Include="asm\test.asm">"#));
        assert!(text.contains(r#"<NASM Include="asm\test.nasm">"#));
        assert!(text.contains(r"<IncludePaths>asm\include\;%(IncludePaths)</IncludePaths>"));
        assert!(text.contains(r"<AdditionalDependencies>asm\table.inc</AdditionalDependencies>"));
        assert_eq!(Assembler::from_language(b"ASM_NASM"), Some(Assembler::Nasm));
        assert_eq!(Assembler::from_language(b"ASM"), None);
    }
}
