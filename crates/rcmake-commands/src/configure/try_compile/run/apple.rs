use std::{collections::HashMap, path::Path, time::Duration};

use rcmake_base::{BString, list};
use rcmake_eval::Cx;
use rcmake_genex::ConfigId;
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, WriteMode, path};
use rcmake_model::{DirId, ProjectModel};

const SYSROOT: &[u8] = b"CMAKE_OSX_SYSROOT";
const ARCHS: &[u8] = b"CMAKE_OSX_ARCHITECTURES";

pub(super) fn resolve(
    child: &Cx<'_>,
    model: &mut ProjectModel,
    directory: &Path,
    config: &ConfigId,
) -> Result<(), String> {
    if !matches!(
        child.var_or_empty(b"CMAKE_SYSTEM_NAME").as_slice(),
        b"Darwin" | b"iOS" | b"tvOS" | b"watchOS" | b"visionOS"
    ) {
        return Ok(());
    }
    let host = child.host();
    let sdk = child.var_or_empty(SYSROOT);
    let default_xcode_sdk = sdk.is_empty() && child.var_or_empty(b"CMAKE_GENERATOR") == "Xcode";
    let sdk_path = if (sdk.is_empty() && !default_xcode_sdk) || sdk.contains(&b'/') {
        sdk.clone()
    } else {
        let resolved = child.var_or_empty(b"_CMAKE_OSX_SYSROOT_PATH");
        if resolved.starts_with(b"/") {
            resolved
        } else {
            let name = if default_xcode_sdk {
                "macosx".into()
            } else {
                sdk.clone()
            };
            run(
                host,
                directory,
                "/usr/bin/xcrun",
                &["--sdk".into(), name, "--show-sdk-path".into()],
            )?
            .as_slice()
            .trim_ascii()
            .into()
        }
    };
    if sdk_path != sdk {
        let dirs = model.dirs().map(|(id, _)| id).collect::<Vec<_>>();
        for id in dirs {
            let dir = model.dir_mut(id);
            dir.vars.insert(SYSROOT.into(), sdk_path.clone());
            dir.props.set(SYSROOT.into(), sdk_path.clone());
        }
    }
    let configured = [
        b"OSX_ARCHITECTURES_".as_slice(),
        &config.0.to_ascii_uppercase(),
    ]
    .concat();
    let mut expressions = model
        .targets()
        .flat_map(|(id, target)| {
            [b"OSX_ARCHITECTURES".as_slice(), &configured]
                .into_iter()
                .filter_map(move |key| {
                    target
                        .props
                        .get(key)
                        .filter(|value| symbolic(value))
                        .map(|value| (id, key, value.clone()))
                })
        })
        .collect::<Vec<_>>();
    let default = child.var_or_empty(ARCHS);
    if expressions.is_empty() && !symbolic(&default) {
        return Ok(());
    }
    let project = directory.join("CMakeFiles/RcmakeAppleProbe.xcodeproj");
    host.create_dir_all(&project)
        .map_err(|error| error.to_string())?;
    host.write(
        &project.join("project.pbxproj"),
        PROJECT.as_bytes(),
        WriteMode::Truncate,
    )
    .map_err(|error| error.to_string())?;
    let mut resolved = HashMap::new();
    let mut resolve = |value: &BString| -> Result<BString, String> {
        if let Some(arches) = resolved.get(value) {
            return Ok(BString::clone(arches));
        }
        let mut args = vec![
            "-project".into(),
            path::from_native(&project),
            "-target".into(),
            "Probe".into(),
            "-configuration".into(),
            "Debug".into(),
            "-showBuildSettings".into(),
        ];
        if !sdk_path.is_empty() {
            args.extend(["-sdk".into(), sdk_path.clone()]);
        }
        let expression = list::split(value, list::Empty::Drop);
        args.push(
            [
                b"ARCHS=".as_slice(),
                &expression
                    .iter()
                    .map(|arch| arch.as_slice())
                    .collect::<Vec<_>>()
                    .join(&b' '),
            ]
            .concat()
            .into(),
        );
        let output = run(host, directory, "/usr/bin/xcodebuild", &args)?;
        let arches = output
            .split(|byte| *byte == b'\n')
            .find_map(|line| {
                line.trim_ascii()
                    .strip_prefix(b"ARCHS = ")
                    .filter(|arches| !arches.is_empty() && !symbolic(arches))
            })
            .ok_or_else(|| format!("Cannot resolve Xcode architectures {value}."))?;
        let arches = list::join(
            arches
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|word| !word.is_empty()),
        );
        resolved.insert(value.clone(), arches.clone());
        Ok(arches)
    };
    if symbolic(&default) {
        let resolved = resolve(&default)?;
        model
            .dir_mut(DirId::ROOT)
            .vars
            .insert(ARCHS.into(), resolved.clone());
        model.dir_mut(DirId::ROOT).props.set(ARCHS.into(), resolved);
    }
    for (id, key, value) in expressions.drain(..) {
        let arches = resolve(&value)?;
        if let Some(target) = model.target_mut(id) {
            target.props.set(key.into(), arches);
        }
    }
    Ok(())
}

fn symbolic(value: &[u8]) -> bool {
    value.windows(2).any(|part| part == b"$(" || part == b"${")
}

fn run(host: &dyn Host, cwd: &Path, tool: &str, args: &[BString]) -> Result<BString, String> {
    let result = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: tool.into(),
                    args: args.to_vec(),
                }],
                cwd: cwd.to_owned(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(20)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .map_err(|error| error.to_string())?;
    if result.results == [ProcessResult::Exited(0)] {
        Ok(result.stdout.into())
    } else {
        Err(format!("{tool} failed: {}", BString::from(result.stderr)))
    }
}

const PROJECT: &str = r#"// !$*UTF8*$!
{
archiveVersion = 1;
objectVersion = 46;
objects = {
100000000000000000000001 = {isa = PBXProject; buildConfigurationList = 100000000000000000000004; compatibilityVersion = "Xcode 3.2"; mainGroup = 100000000000000000000002; targets = (100000000000000000000003); };
100000000000000000000002 = {isa = PBXGroup; children = (); sourceTree = "<group>"; };
100000000000000000000003 = {isa = PBXAggregateTarget; name = Probe; productName = Probe; buildConfigurationList = 100000000000000000000005; buildPhases = (); dependencies = (); };
100000000000000000000004 = {isa = XCConfigurationList; buildConfigurations = (100000000000000000000006); defaultConfigurationName = Debug; };
100000000000000000000005 = {isa = XCConfigurationList; buildConfigurations = (100000000000000000000007); defaultConfigurationName = Debug; };
100000000000000000000006 = {isa = XCBuildConfiguration; name = Debug; buildSettings = {}; };
100000000000000000000007 = {isa = XCBuildConfiguration; name = Debug; buildSettings = {ONLY_ACTIVE_ARCH = NO;}; };
};
rootObject = 100000000000000000000001;
}
"#;
