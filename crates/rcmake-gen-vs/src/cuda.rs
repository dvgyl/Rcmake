// SPDX-License-Identifier: MIT OR Apache-2.0
//! CUDA and assembler items for Visual Studio projects.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use rcmake_model::TargetKind;

use crate::{msbuild, windows, xml::Writer};

/// Command-line text with an explicit MSBuild expansion policy.
#[derive(Clone, Copy)]
pub(crate) enum OptionText<'a> {
    /// Escape all MSBuild syntax in the option text.
    Literal(&'a str),
    /// Allow simple MSBuild property references in the option text.
    WithProperties(&'a str),
}

impl Default for OptionText<'_> {
    fn default() -> Self {
        Self::Literal("")
    }
}

impl OptionText<'_> {
    pub(crate) fn escaped(self) -> String {
        let mut text = match self {
            Self::Literal(text) => return msbuild(text),
            Self::WithProperties(text) => text,
        };
        let mut output = String::new();
        while let Some((prefix, remainder)) = text.split_once("$(") {
            output.push_str(&msbuild(prefix));
            if let Some((name, rest)) = remainder.split_once(')')
                && !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            {
                output.push_str("$(");
                output.push_str(name);
                output.push(')');
                text = rest;
            } else {
                output.push_str("%24%28");
                text = remainder;
            }
        }
        output.push_str(&msbuild(text));
        output
    }
}

/// A CUDA output mode and its file extension.
#[derive(Clone, Copy)]
pub(crate) struct CompileMode {
    /// Value of the MSBuild CUDA compilation property.
    pub(crate) nvcc: &'static str,
    /// File extension without the period.
    pub(crate) extension: &'static str,
}

/// CUDA compiler properties for one project configuration.
#[derive(Default)]
pub(crate) struct CompileOptions<'a> {
    /// Optional condition for this configuration.
    pub(crate) condition: Option<&'a str>,
    /// The evaluated CUDA_RUNTIME_LIBRARY value.
    pub(crate) runtime: Option<&'a str>,
    /// CUDA optimization selected in the compiler flag table.
    pub(crate) optimization: Option<&'a str>,
    /// The Visual Studio platform, such as x64.
    pub(crate) platform: &'a str,
    /// Whether the compiler must generate relocatable device code.
    pub(crate) separable: bool,
    /// Alternate output mode, if the target requests one.
    pub(crate) mode: Option<CompileMode>,
    /// Ordered include directories.
    pub(crate) includes: &'a [String],
    /// Preprocessor definitions.
    pub(crate) defines: &'a [String],
    /// Architecture flags produced by the CUDA architecture lowerer.
    pub(crate) architecture_flags: &'a [String],
    /// Other CUDA compiler options, quoted for the CUDA command line.
    pub(crate) additional_options: OptionText<'a>,
}

/// CUDA linker properties for one project configuration.
#[derive(Default)]
pub(crate) struct LinkOptions<'a> {
    /// Optional condition for this configuration.
    pub(crate) condition: Option<&'a str>,
    /// The project target kind.
    pub(crate) target_kind: Option<TargetKind>,
    /// Whether MSBuild must run the device linker.
    pub(crate) perform_device_link: bool,
    /// Extra object files and libraries for the device linker.
    pub(crate) dependencies: &'a [String],
    /// Device-link options, quoted for the CUDA command line.
    pub(crate) additional_options: OptionText<'a>,
}

/// Write the CUDA compile settings in an ItemDefinitionGroup.
pub(crate) fn compile_options(xml: &mut Writer, options: &CompileOptions<'_>) {
    let condition = options.condition.map(|value| ("Condition", value));
    xml.start("CudaCompile", condition.as_slice());
    // NVIDIA's targets can add virtual code to CodeGeneration entries.
    // Pass the complete architecture flags to nvcc through AdditionalOptions.
    xml.text("CodeGeneration", &[], "");
    xml.text(
        "GenerateRelocatableDeviceCode",
        &[],
        if options.separable { "true" } else { "false" },
    );
    xml.text("UseHostInclude", &[], "false");
    if let Some(mode) = options.mode {
        xml.text("NvccCompilation", &[], mode.nvcc);
    }
    let extension = options.mode.map_or("obj", |mode| mode.extension);
    xml.text(
        "CompileOut",
        &[],
        &format!("$(IntDir)%(Filename).{extension}"),
    );
    xml.text(
        "Optimization",
        &[],
        options.optimization.unwrap_or_default(),
    );
    if let Some(runtime) = match options.runtime {
        Some("STATIC") => Some("Static"),
        Some("SHARED") => Some("Shared"),
        Some("NONE") => Some("None"),
        _ => None,
    } {
        xml.text("CudaRuntime", &[], runtime);
    }
    if options.platform.eq_ignore_ascii_case("x64") {
        xml.text("TargetMachinePlatform", &[], "64");
    }
    include_paths(xml, "Include", options.includes, false);
    defines(xml, "Defines", options.defines);
    let flags = options
        .architecture_flags
        .iter()
        .map(|flag| OptionText::Literal(flag))
        .chain([options.additional_options]);
    additional_options(xml, flags, true);
    xml.end();
}

/// Write the CUDA link settings in an ItemDefinitionGroup.
pub(crate) fn link_options(xml: &mut Writer, options: &LinkOptions<'_>) {
    let condition = options.condition.map(|value| ("Condition", value));
    xml.start("CudaLink", condition.as_slice());
    let perform =
        options.perform_device_link && options.target_kind != Some(TargetKind::ObjectLibrary);
    let required = if perform { "true" } else { "false" };
    xml.text("PerformDeviceLink", &[], required);
    if perform && !options.dependencies.is_empty() {
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

/// Write the toolkit root in the project's Globals property group.
pub(crate) fn cuda_toolkit_custom_dir(xml: &mut Writer, toolkit_root: &str) {
    if !toolkit_root.is_empty() {
        let root = windows(toolkit_root);
        let root = format!("{}\\", root.trim_end_matches('\\'));
        xml.text("CudaToolkitCustomDir", &[], &msbuild(&root));
    }
}

/// Write the NASM import in the open ExtensionSettings or ExtensionTargets group.
///
/// Pass the directory with the configured props file and the directory with
/// the bundled targets file.
pub(crate) fn nasm_import(xml: &mut Writer, kind: ImportKind, props_dir: &str, targets_dir: &str) {
    let directory = match kind {
        ImportKind::Props => props_dir,
        ImportKind::Targets => targets_dir,
    };
    let filename = format!("nasm.{}", kind.extension());
    let project = if directory.is_empty() {
        filename
    } else {
        format!("{}\\{filename}", directory.trim_end_matches(['/', '\\']))
    };
    xml.empty("Import", &[("Project", &msbuild(&windows(&project)))]);
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
    /// Configuration condition for this source item.
    pub(crate) condition: Option<&'a str>,
    /// Conditions for configurations that exclude this source.
    pub(crate) excluded_conditions: &'a [String],
    /// Source-level include directories.
    pub(crate) includes: &'a [String],
    /// Source-level preprocessor definitions.
    pub(crate) defines: &'a [String],
    /// Source-level command-line flags, already quoted as needed.
    pub(crate) additional_options: OptionText<'a>,
    /// Explicit object dependencies for a NASM source.
    pub(crate) dependencies: &'a [String],
    /// Custom NASM output path. The import supplies the default.
    pub(crate) nasm_output: Option<OptionText<'a>>,
    /// Object file path for a CUDA or MASM source.
    pub(crate) object_output: Option<&'a str>,
    /// NASM output switch from the Visual Studio flag table.
    pub(crate) nasm_output_switch: Option<&'a str>,
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
    let mut attrs = vec![("Include", path.as_str())];
    if let Some(condition) = source.condition {
        attrs.push(("Condition", condition));
    }
    xml.start(item, &attrs);
    for condition in source.excluded_conditions {
        xml.text("ExcludedFromBuild", &[("Condition", condition)], "true");
    }
    include_paths(xml, include_property, source.includes, nasm);
    defines(xml, define_property, source.defines);
    additional_options(xml, [source.additional_options], true);
    if let Some(output) = source.object_output {
        let property = if item == "CudaCompile" {
            "CompileOut"
        } else {
            "ObjectFileName"
        };
        xml.text(property, &[], &msbuild(output));
    }
    if nasm {
        if let Some(output) = source.nasm_output {
            xml.text("OutputFormat", &[], &output.escaped());
        }
        if let Some(switch) = source.nasm_output_switch {
            xml.text("Outputswitch", &[], &msbuild(switch));
        }
        if !source.dependencies.is_empty() {
            let inputs = source
                .dependencies
                .iter()
                .map(|path| msbuild(&windows(path)))
                .collect::<Vec<_>>();
            xml.text("AdditionalDependencies", &[], &inputs.join(";"));
        }
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
    flags: impl IntoIterator<Item = OptionText<'a>>,
    inherit: bool,
) {
    let flags = flags
        .into_iter()
        .map(OptionText::escaped)
        .filter(|flag| !flag.is_empty())
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
                additional_options: OptionText::Literal("-lineinfo"),
                runtime: Some("SHARED"),
                ..CompileOptions::default()
            },
        );
        let text = xml.finish();
        assert!(text.contains("<CodeGeneration></CodeGeneration>"));
        assert!(
            text.contains("<GenerateRelocatableDeviceCode>true</GenerateRelocatableDeviceCode>")
        );
        assert!(text.contains("<TargetMachinePlatform>64</TargetMachinePlatform>"));
        assert!(text.contains("<CudaRuntime>Shared</CudaRuntime>"));
        assert!(text.contains("<UseHostInclude>false</UseHostInclude>"));
        assert!(text.contains("<Optimization></Optimization>"));
        assert!(text.contains(r"<Include>inc\a%3Bb;%(Include)</Include>"));
        assert!(text.contains("<Defines>%(Defines);SIZE=2;A&amp;B</Defines>"));
        assert!(text.contains("<AdditionalOptions>%(AdditionalOptions) --generate-code=arch=compute_75,code=sm_75 -lineinfo</AdditionalOptions>"));
    }

    #[test]
    fn link_options_override_the_msbuild_default() {
        let mut xml = Writer::new(false, "utf-8");
        link_options(
            &mut xml,
            &LinkOptions {
                condition: Some("'$(Configuration)'=='Debug'"),
                perform_device_link: true,
                target_kind: Some(TargetKind::ObjectLibrary),
                ..LinkOptions::default()
            },
        );
        link_options(
            &mut xml,
            &LinkOptions {
                perform_device_link: true,
                dependencies: &["obj/kernel.obj".into()],
                additional_options: OptionText::Literal("-Xnvlink=-v"),
                ..LinkOptions::default()
            },
        );
        let text = xml.finish();
        assert!(text.contains("<CudaLink Condition=\"'$(Configuration)'=='Debug'\">\r\n  <PerformDeviceLink>false</PerformDeviceLink>"));
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
        cuda_toolkit_custom_dir(&mut xml, "C:/toolkit/a&b");
        masm_import(&mut xml, ImportKind::Props);
        nasm_import(
            &mut xml,
            ImportKind::Props,
            "project data",
            "C:/CMake/Templates/MSBuild",
        );
        nasm_import(
            &mut xml,
            ImportKind::Targets,
            "project data",
            "C:/CMake/Templates/MSBuild",
        );
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
                nasm_output: Some(OptionText::WithProperties(r"$(IntDir)custom.obj")),
                nasm_output_switch: Some("1"),
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
        assert!(
            text.contains(r#"<CudaToolkitCustomDir>C:\toolkit\a&amp;b\</CudaToolkitCustomDir>"#)
        );
        assert!(text.contains(r#"Project="project data\nasm.props""#));
        assert!(text.contains(r#"Project="C:\CMake\Templates\MSBuild\nasm.targets""#));
        assert!(text.contains("<OutputFormat>$(IntDir)custom.obj</OutputFormat>"));
        assert!(text.contains("<Outputswitch>1</Outputswitch>"));
        assert!(text.contains(r"<IncludePaths>asm\include\;%(IncludePaths)</IncludePaths>"));
        assert!(text.contains(r"<AdditionalDependencies>asm\table.inc</AdditionalDependencies>"));
        assert_eq!(Assembler::from_language(b"ASM_NASM"), Some(Assembler::Nasm));
        assert_eq!(Assembler::from_language(b"ASM"), None);
    }
    #[test]
    fn options_preserve_only_explicit_property_references() {
        let mut xml = Writer::new(false, "utf-8");
        additional_options(
            &mut xml,
            [
                OptionText::Literal(r#"-DNAME="a&b;$(Literal)""#),
                OptionText::WithProperties(r#"-I"$(IntDir)one;two""#),
            ],
            false,
        );
        assert!(xml.finish().contains(
            r#"<AdditionalOptions>-DNAME="a&amp;b%3B%24%28Literal%29" -I"$(IntDir)one%3Btwo"</AdditionalOptions>"#
        ));
    }
}
