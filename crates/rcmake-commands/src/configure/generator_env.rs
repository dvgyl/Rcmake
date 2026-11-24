use std::time::Duration;

use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest, path};
use rcmake_model::{
    CacheType, GeneratorKind, GeneratorSpec, GeneratorSpecError, ParsedToolset, SpecField,
    VsVersion, XcodeBuildSystem, XcodeVersion,
};

use super::enable_language::add_cache;

const TOOLSET: &[u8] = b"CMAKE_GENERATOR_TOOLSET";
const PLATFORM: &[u8] = b"CMAKE_GENERATOR_PLATFORM";
const INSTANCE: &[u8] = b"CMAKE_GENERATOR_INSTANCE";
const CONFIGURATIONS: &[u8] = b"CMAKE_CONFIGURATION_TYPES";
const XCODE_VERSION_PROBE: &[u8] = b"_RCMAKE_XCODE_VERSION_PROBE";

/// Set the IDE generator variables before the language bootstrap runs.
pub(super) fn initialize(cx: &mut Cx<'_>, generator: GeneratorKind) -> Result<(), CmdError> {
    if !generator.is_ide() {
        return Ok(());
    }

    let mut spec = cx.project().generator_spec.clone();
    // The CLI applies cache edits after it loads the raw generator options.
    if let Some(entry) = cx.cache().get(INSTANCE) {
        spec.instance.clone_from(&entry.value);
    }
    if let GeneratorKind::VisualStudio(version) = generator {
        validate_vs_instance(version, &spec.instance)
            .map_err(|message| crate::command_error(cx, message))?;
    }
    let xcode = (generator == GeneratorKind::Xcode)
        .then(|| xcode_version(cx))
        .flatten();
    let spec = GeneratorSpec::parse(
        generator,
        &spec.toolset,
        &spec.platform,
        &spec.instance,
        xcode.as_ref(),
    )
    .map_err(|error| crate::command_error(cx, error.to_string()))?;

    for (key, value, help) in [
        (TOOLSET, &spec.toolset, "Name of generator toolset."),
        (PLATFORM, &spec.platform, "Name of generator platform."),
        (INSTANCE, &spec.instance, "Generator instance identifier."),
    ] {
        add_cache(cx, key, value.clone(), CacheType::Internal, help);
    }
    if !cx.var_defined(CONFIGURATIONS) {
        add_cache(
            cx,
            CONFIGURATIONS,
            generator.default_configuration_types().join(";").into(),
            CacheType::String,
            "Available build configurations.",
        );
    }

    match (generator, &spec.parsed) {
        (
            _,
            ParsedToolset::Xcode {
                toolset,
                build_system,
            },
        ) => {
            let default = xcode
                .as_ref()
                .map_or(XcodeBuildSystem::Twelve, XcodeBuildSystem::default_for);
            cx.set_var(b"XCODE", "1".into());
            cx.set_var(
                b"XCODE_VERSION",
                xcode.map(|version| version.text).unwrap_or_default(),
            );
            cx.set_var(
                b"CMAKE_XCODE_BUILD_SYSTEM",
                build_system
                    .clone()
                    .unwrap_or_else(|| default.as_str().into()),
            );
            if let Some(toolset) = toolset {
                cx.set_var(b"CMAKE_XCODE_PLATFORM_TOOLSET", toolset.clone());
            }
            if !cx
                .var(b"CMAKE_MAKE_PROGRAM")
                .is_some_and(|v| !v.is_empty() && !rcmake_base::truth::is_off(&v))
                && let Some(xcodebuild) = find_xcodebuild(cx)
            {
                add_cache(
                    cx,
                    b"CMAKE_MAKE_PROGRAM",
                    xcodebuild,
                    CacheType::Filepath,
                    "Path to a program.",
                );
            }
        }
        (GeneratorKind::VisualStudio(version), ParsedToolset::Vs(toolset)) => {
            let (default_platform, default_host) = match cx.host().system().machine.as_slice() {
                b"arm64" | b"aarch64" | b"ARM64" => ("ARM64", "ARM64"),
                b"x86_64" | b"AMD64" | b"amd64" | b"x64" => ("x64", "x64"),
                _ => ("Win32", "x86"),
            };
            let platform = spec
                .parsed_platform
                .as_ref()
                .and_then(|platform| platform.name.clone())
                .unwrap_or_else(|| default_platform.into());
            cx.set_var(b"MSVC_IDE", "1".into());
            cx.set_var(b"CMAKE_VS_PLATFORM_NAME_DEFAULT", default_platform.into());
            cx.set_var(b"CMAKE_VS_PLATFORM_NAME", platform.clone());
            add_cache(
                cx,
                b"CMAKE_VS_PLATFORM_NAME",
                platform,
                CacheType::Internal,
                "Name of generator platform.",
            );
            cx.set_var(
                b"CMAKE_VS_PLATFORM_TOOLSET",
                toolset
                    .name
                    .clone()
                    .unwrap_or_else(|| version.default_toolset().into()),
            );
            cx.set_var(
                b"CMAKE_VS_PLATFORM_TOOLSET_HOST_ARCHITECTURE",
                toolset.host.clone().unwrap_or_else(|| default_host.into()),
            );
        }
        _ => {}
    }
    cx.project().generator_spec = spec;
    Ok(())
}

pub(super) fn configure_msbuild(cx: &mut Cx<'_>, version: VsVersion) {
    let program = b"CMAKE_MAKE_PROGRAM";
    let mut selected = cx.var_or_empty(program);
    if rcmake_base::truth::is_off(&selected) {
        let Some(discovered) = find_msbuild(cx, version) else {
            return;
        };
        selected = discovered;
        add_cache(
            cx,
            program,
            selected.clone(),
            CacheType::Filepath,
            "Path to a program.",
        );
    }
    cx.set_var(b"CMAKE_VS_MSBUILD_COMMAND", selected);
}

fn find_msbuild(cx: &mut Cx<'_>, version: VsVersion) -> Option<BString> {
    let instance = cx.project().generator_spec.instance.clone();
    let host = cx.host();
    let is_file = |candidate: &std::path::Path| {
        host.metadata(candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    };
    let from_install = |dir: &[u8]| {
        let directory = path::to_native(dir).join("MSBuild/Current/Bin");
        let suffix = match host.system().machine.as_slice() {
            b"ARM64" | b"arm64" | b"aarch64" => "arm64/MSBuild.exe",
            _ => "MSBuild.exe",
        };
        [directory.join(suffix), directory.join("MSBuild.exe")]
            .into_iter()
            .find(|candidate| is_file(candidate))
            .map(|file| path::from_native_cmake(&file))
    };
    if let Some(dir) = instance
        .split(|&byte| byte == b',')
        .next()
        .filter(|dir| !dir.is_empty() && !dir.contains(&b'='))
    {
        return from_install(dir);
    }
    if let Some(dir) = host.env_get(b"VSINSTALLDIR").filter(|dir| !dir.is_empty())
        && let Some(found) = from_install(&dir)
    {
        return Some(found);
    }
    let windows = cfg!(windows) || host.system().sysname.starts_with(b"Windows");
    if let Some(paths) = host.env_get(b"PATH") {
        let delimiter = if windows { b';' } else { b':' };
        for dir in paths
            .split(|&byte| byte == delimiter)
            .filter(|dir| !dir.is_empty())
        {
            let candidate = host
                .initial_cwd()
                .join(path::to_native(dir))
                .join("MSBuild.exe");
            if is_file(&candidate) {
                return Some(path::from_native_cmake(&candidate));
            }
        }
    }
    let installer = host
        .env_get(b"ProgramFiles(x86)")
        .filter(|dir| !dir.is_empty())?;
    let locator = path::to_native(&installer).join("Microsoft Visual Studio/Installer/vswhere.exe");
    if !is_file(&locator) {
        return None;
    }
    let range = match version {
        VsVersion::Vs17 => "[17.0,18.0)",
        VsVersion::Vs18 => "[18.0,19.0)",
    };
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(&locator),
            args: [
                "-latest",
                "-products",
                "*",
                "-version",
                range,
                "-requires",
                "Microsoft.Component.MSBuild",
                "-property",
                "installationPath",
                "-utf8",
            ]
            .into_iter()
            .map(BString::from)
            .collect(),
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(5)),
        tee: false,
    };
    let output = host.run_bounded(&request, 64 * 1024).ok()?;
    if output.results != [ProcessResult::Exited(0)] {
        return None;
    }
    output.stdout.split(|&byte| byte == b'\n').find_map(|line| {
        let dir = line
            .trim_ascii()
            .strip_prefix(b"\xef\xbb\xbf")
            .unwrap_or(line.trim_ascii());
        (!dir.is_empty()).then(|| from_install(dir)).flatten()
    })
}

fn find_xcodebuild(cx: &Cx<'_>) -> Option<BString> {
    let host = cx.host();
    if let Some(path_env) = host.env_get(b"PATH") {
        let sep = if cfg!(windows) { b';' } else { b':' };
        for dir in path_env.split(|&b| b == sep) {
            if dir.is_empty() {
                continue;
            }
            let candidate = rcmake_host::path::to_native(dir).join("xcodebuild");
            if host
                .metadata(&candidate, rcmake_host::Follow::Yes)
                .is_ok_and(|meta| meta.kind == rcmake_host::FileKind::File)
            {
                return Some(rcmake_host::path::from_native(&candidate));
            }
        }
    }
    let fallback = std::path::Path::new("/usr/bin/xcodebuild");
    if host
        .metadata(fallback, rcmake_host::Follow::Yes)
        .is_ok_and(|meta| meta.kind == rcmake_host::FileKind::File)
    {
        return Some(rcmake_host::path::from_native(fallback));
    }
    None
}

fn xcode_version(cx: &mut Cx<'_>) -> Option<XcodeVersion> {
    if let Some(version) = cx.global().props.get(XCODE_VERSION_PROBE) {
        return (!version.is_empty()).then(|| XcodeVersion::parse(version));
    }
    let host = cx.host();
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: "xcodebuild".into(),
            args: vec!["-version".into()],
        }],
        cwd: cx.current_source_dir(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Null,
        timeout: Some(Duration::from_secs(5)),
        tee: false,
    };
    let version = host
        .run(&request, &mut |_, _| {})
        .ok()
        .filter(|result| result.results == [ProcessResult::Exited(0)])
        .and_then(|result| {
            result.stdout.split(|&byte| byte == b'\n').find_map(|line| {
                let version = line.strip_prefix(b"Xcode ")?.trim_ascii();
                version
                    .first()?
                    .is_ascii_digit()
                    .then(|| BString::from(version))
            })
        })
        .unwrap_or_default();
    let parsed = (!version.is_empty()).then(|| XcodeVersion::parse(&version));
    cx.global().props.set(XCODE_VERSION_PROBE.into(), version);
    parsed
}

// The shared parser checks whether the generator accepts an instance. Check
// the Visual Studio field syntax before it checks the platform and toolset.
fn validate_vs_instance(version: VsVersion, spec: &[u8]) -> Result<(), String> {
    let generator = GeneratorKind::VisualStudio(version);
    let field = SpecField::Instance;
    let mut items = spec
        .split(|&byte| byte == b',')
        .filter(|item| !item.is_empty())
        .peekable();
    if items.peek().is_some_and(|item| !item.contains(&b'=')) {
        items.next();
    }

    let mut build_version = None;
    for item in items {
        let mut pair = item.splitn(2, |&byte| byte == b'=');
        match (pair.next(), pair.next()) {
            (Some(b"version"), Some(value)) if build_version.is_none() => {
                build_version = Some(value);
            }
            (Some(b"version"), Some(_)) => {
                return Err(GeneratorSpecError::DuplicateKey {
                    generator,
                    field,
                    spec: spec.into(),
                    key: "version".into(),
                }
                .to_string());
            }
            (_, Some(_)) => {
                return Err(GeneratorSpecError::InvalidField {
                    generator,
                    field,
                    spec: spec.into(),
                    item: item.into(),
                }
                .to_string());
            }
            (_, None) => {
                return Err(GeneratorSpecError::FieldWithoutEquals {
                    generator,
                    field,
                    spec: spec.into(),
                }
                .to_string());
            }
        }
    }

    if let Some(value) = build_version.filter(|value| !value.is_empty()) {
        let major = match version {
            VsVersion::Vs17 => "17",
            VsVersion::Vs18 => "18",
        };
        let mut parts = value.split(|&byte| byte == b'.');
        let valid = parts.next() == Some(major.as_bytes())
            && parts.clone().count() == 3
            && parts.all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit));
        if !valid {
            let spec = String::from_utf8_lossy(spec);
            return Err(format!(
                "Generator\n  {}\ngiven instance specification\n  {spec}\n\
                 but the version field is not 4 integer components starting in {major}.",
                generator.name()
            ));
        }
    }
    Ok(())
}
