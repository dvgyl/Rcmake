//! Shared Xcode schemes and action settings.

use std::{fmt::Write as _, path::Path};

use rcmake_gen_common::GenError;
use rcmake_host::Host;
use rcmake_model::Target;

use crate::{
    file_name,
    object::Reference,
    settings::{on, strings},
    text,
    workspace::{XML, escape},
    write_file,
};

pub(crate) fn write(
    host: &dyn Host,
    project: &Path,
    target: &Target,
    reference: &Reference,
    product: &str,
    config: &str,
    upgrade: &str,
) -> Result<(), GenError> {
    let property = |key: &str| target.props.get(format!("XCODE_SCHEME_{key}").as_bytes());
    let cfg = |key| escape(&property(key).map_or_else(|| config.to_owned(), |value| text(value)));
    let container = project
        .file_name()
        .map_or_else(String::new, |s| escape(&s.to_string_lossy()));
    let blueprint = escape(&reference.comment);
    let product = escape(product);
    let id = &reference.id;
    let build_ref = format!(
        "<BuildableReference BuildableIdentifier=\"primary\" BlueprintIdentifier=\"{id}\" BuildableName=\"{product}\" BlueprintName=\"{blueprint}\" ReferencedContainer=\"container:{container}\"></BuildableReference>"
    );
    let mut out = format!(
        "{XML}<Scheme LastUpgradeVersion=\"{upgrade}\" version=\"1.3\">\n  <BuildAction parallelizeBuildables=\"YES\" buildImplicitDependencies=\"YES\">\n    <BuildActionEntries>\n      <BuildActionEntry buildForTesting=\"YES\" buildForRunning=\"YES\" buildForProfiling=\"YES\" buildForArchiving=\"YES\" buildForAnalyzing=\"YES\">\n        {build_ref}\n      </BuildActionEntry>\n    </BuildActionEntries>\n  </BuildAction>\n"
    );
    let test_config = cfg("TEST_CONFIGURATION");
    let launch_config = cfg("LAUNCH_CONFIGURATION");
    let _ = writeln!(
        out,
        "  <TestAction buildConfiguration=\"{test_config}\" selectedDebuggerIdentifier=\"Xcode.DebuggerFoundation.Debugger.LLDB\" selectedLauncherIdentifier=\"Xcode.DebuggerFoundation.Launcher.LLDB\" shouldUseLaunchSchemeArgsEnv=\"YES\">\n    <Testables></Testables>\n  </TestAction>"
    );
    let _ = write!(
        out,
        "  <LaunchAction buildConfiguration=\"{launch_config}\" selectedDebuggerIdentifier=\"Xcode.DebuggerFoundation.Debugger.LLDB\" selectedLauncherIdentifier=\"Xcode.DebuggerFoundation.Launcher.LLDB\""
    );
    let enabled = |prop| property(prop).is_some_and(|value| rcmake_base::truth::is_on(value));
    let asan = enabled("ADDRESS_SANITIZER");
    let tsan = !asan && enabled("THREAD_SANITIZER");
    let launch_style = if property("LAUNCH_MODE").is_some_and(|mode| mode == b"WAIT") {
        "1"
    } else {
        "0"
    };
    let document_versioning = if property("DEBUG_DOCUMENT_VERSIONING")
        .is_none_or(|value| rcmake_base::truth::is_on(value))
    {
        "YES"
    } else {
        "NO"
    };
    let _ = write!(
        out,
        " launchStyle=\"{launch_style}\" ignoresPersistentStateOnLaunch=\"NO\" debugDocumentVersioning=\"{document_versioning}\" debugServiceExtension=\"internal\" allowLocationSimulation=\"YES\""
    );
    for (prop, name) in [
        ("ADDRESS_SANITIZER", "enableAddressSanitizer"),
        (
            "ADDRESS_SANITIZER_USE_AFTER_RETURN",
            "enableASanStackUseAfterReturn",
        ),
        ("THREAD_SANITIZER_STOP", "stopOnEveryThreadSanitizerIssue"),
        ("UNDEFINED_BEHAVIOUR_SANITIZER", "enableUBSanitizer"),
        (
            "UNDEFINED_BEHAVIOUR_SANITIZER_STOP",
            "stopOnEveryUBSanitizerIssue",
        ),
        ("DISABLE_MAIN_THREAD_CHECKER", "disableMainThreadChecker"),
        (
            "MAIN_THREAD_CHECKER_STOP",
            "stopOnEveryMainThreadCheckerIssue",
        ),
        ("ENABLE_GPU_API_VALIDATION", "enableGPUValidationMode"),
        (
            "ENABLE_GPU_SHADER_VALIDATION",
            "enableGPUShaderValidationMode",
        ),
        ("DEBUG_AS_ROOT", "debugAsRoot"),
    ] {
        if property(prop).is_some() {
            let enabled = if on(target, format!("XCODE_SCHEME_{prop}").as_bytes()) {
                "YES"
            } else {
                "NO"
            };
            let _ = write!(out, " {name}=\"{enabled}\"");
        }
    }
    if tsan {
        out.push_str(" enableThreadSanitizer=\"YES\"");
    }
    if let Some(value) = property("LLDB_INIT_FILE") {
        let _ = write!(out, " customLLDBInitFile=\"{}\"", escape(&text(value)));
    }
    if let Some(value) = property("ENABLE_GPU_FRAME_CAPTURE_MODE") {
        let value = if value.eq_ignore_ascii_case(b"Metal") {
            "1".into()
        } else if value.eq_ignore_ascii_case(b"Disabled") {
            "3".into()
        } else {
            text(value)
        };
        let _ = write!(out, " enableGPUFrameCaptureMode=\"{}\"", escape(&value));
    }
    if let Some(path) = property("WORKING_DIRECTORY") {
        let _ = write!(
            out,
            " useCustomWorkingDirectory=\"YES\" customWorkingDirectory=\"{}\"",
            escape(&text(path))
        );
    } else {
        out.push_str(" useCustomWorkingDirectory=\"NO\"");
    }
    out.push_str(">\n");
    if let Some(path) = property("EXECUTABLE").filter(|path| !path.is_empty()) {
        let _ = writeln!(
            out,
            "    <PathRunnable runnableDebuggingMode=\"0\" FilePath=\"{}\"></PathRunnable>",
            escape(&text(path))
        );
    } else if target.kind == rcmake_model::TargetKind::Executable {
        let _ = writeln!(
            out,
            "    <BuildableProductRunnable runnableDebuggingMode=\"0\">{build_ref}</BuildableProductRunnable>"
        );
    }
    if let Some(values) = property("ARGUMENTS") {
        out.push_str("    <CommandLineArguments>\n");
        for arg in strings(values) {
            let _ = writeln!(
                out,
                "      <CommandLineArgument argument=\"{}\" isEnabled=\"YES\"></CommandLineArgument>",
                escape(&arg)
            );
        }
        out.push_str("    </CommandLineArguments>\n");
    }
    let env = property("ENVIRONMENT").map_or_else(Vec::new, |value| strings(value));
    if !env.is_empty() {
        out.push_str("    <EnvironmentVariables>\n");
        for entry in env {
            let (key, value) = entry.split_once('=').unwrap_or((&entry, ""));
            let _ = writeln!(
                out,
                "      <EnvironmentVariable key=\"{}\" value=\"{}\" isEnabled=\"YES\"></EnvironmentVariable>",
                escape(key),
                escape(value)
            );
        }
        out.push_str("    </EnvironmentVariables>\n");
    }
    out.push_str("    <AdditionalOptions>\n");
    for (prop, key, value, allowed) in [
        ("MALLOC_SCRIBBLE", "MallocScribble", "", !tsan),
        ("MALLOC_GUARD_EDGES", "MallocGuardEdges", "", !tsan && !asan),
        (
            "GUARD_MALLOC",
            "DYLD_INSERT_LIBRARIES",
            "/usr/lib/libgmalloc.dylib",
            !tsan && !asan,
        ),
        ("ZOMBIE_OBJECTS", "NSZombieEnabled", "YES", true),
        ("MALLOC_STACK", "MallocStackLogging", "", !tsan && !asan),
        ("DYNAMIC_LINKER_API_USAGE", "DYLD_PRINT_APIS", "", true),
        ("DYNAMIC_LIBRARY_LOADS", "DYLD_PRINT_LIBRARIES", "", true),
    ] {
        if allowed && enabled(prop) {
            let _ = writeln!(
                out,
                "      <AdditionalOption key=\"{key}\" value=\"{value}\" isEnabled=\"YES\"></AdditionalOption>"
            );
        }
    }
    out.push_str("    </AdditionalOptions>\n");
    out.push_str("  </LaunchAction>\n");
    let default = escape(config);
    let _ = writeln!(
        out,
        "  <ProfileAction buildConfiguration=\"{launch_config}\" shouldUseLaunchSchemeArgsEnv=\"YES\"></ProfileAction>\n  <AnalyzeAction buildConfiguration=\"{default}\"></AnalyzeAction>\n  <ArchiveAction buildConfiguration=\"Release\" revealArchiveInOrganizer=\"YES\"></ArchiveAction>\n</Scheme>"
    );
    let name = text(&target.name);
    write_file(
        host,
        &project
            .join("xcshareddata/xcschemes")
            .join(format!("{}.xcscheme", file_name(&name)?)),
        &out,
    )
}
