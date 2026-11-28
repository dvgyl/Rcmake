//! Build command planning for Xcode.

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};
use rcmake_host::{FileKind, Follow, Host, path::to_native};

/// Return the `xcodebuild` commands in execution order.
///
/// # Errors
///
/// Returns an error when the request combines `clean` with other targets.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    build_project_commands(req)
}

/// Return the `xcodebuild` commands in execution order, using `-workspace` and
/// `-scheme` when `<project>.xcworkspace` exists in the build directory.
///
/// # Errors
///
/// Returns an error when the request combines `clean` with other targets.
pub fn build_commands_with_workspace(
    req: &BuildRequest<'_>,
    sysroot: &[u8],
    host: &dyn Host,
) -> Result<Vec<BuildCommand>, GenError> {
    if !req.project_name.is_empty() {
        let mut workspace = BString::from(req.project_name);
        workspace.extend_from_slice(b".xcworkspace");
        let workspace_path = to_native(req.build_dir).join(to_native(&workspace));
        if host
            .metadata(&workspace_path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            return build_workspace_commands(req, &workspace, sysroot);
        }
    }
    build_project_commands(req)
}

fn xcode_destination_platform(sysroot: &[u8]) -> &'static [u8] {
    let sdk = Lexical::new(sysroot).filename().to_ascii_lowercase();
    if sdk.starts_with(b"iphoneos") {
        b"iOS"
    } else if sdk.starts_with(b"iphonesimulator") {
        b"iOS Simulator"
    } else if sdk.starts_with(b"appletvos") {
        b"tvOS"
    } else if sdk.starts_with(b"appletvsimulator") {
        b"tvOS Simulator"
    } else if sdk.starts_with(b"watchos") {
        b"watchOS"
    } else if sdk.starts_with(b"watchsimulator") {
        b"watchOS Simulator"
    } else if sdk.starts_with(b"xros") {
        b"visionOS"
    } else if sdk.starts_with(b"xrsimulator") {
        b"visionOS Simulator"
    } else {
        b"MacOS"
    }
}

fn explicit_destination_args(native_args: &[BString]) -> Vec<BString> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(arg) = native_args.get(index) {
        if arg.as_slice() == b"-destination" {
            out.push(arg.clone());
            if let Some(value) = native_args.get(index + 1) {
                out.push(value.clone());
                index += 2;
                continue;
            }
        } else if arg.starts_with(b"-destination=") {
            out.push(arg.clone());
        }
        index += 1;
    }
    out
}

fn build_workspace_commands(
    req: &BuildRequest<'_>,
    workspace: &BString,
    sysroot: &[u8],
) -> Result<Vec<BuildCommand>, GenError> {
    let clean = req
        .targets
        .iter()
        .any(|target| target.as_slice() == b"clean");
    if clean && req.targets.len() > 1 {
        return Err(GenError::Invalid(
            "Building 'clean' and other targets together is not supported.".into(),
        ));
    }
    let platform = xcode_destination_platform(sysroot);
    let command = |action: &str, scheme: &[u8], native_args: &[BString]| {
        let has_destination = native_args
            .iter()
            .any(|arg| arg.as_slice() == b"-destination" || arg.starts_with(b"-destination="));
        let mut argv = vec![
            if req.make_program.is_empty() {
                "/usr/bin/xcodebuild".into()
            } else {
                req.make_program.into()
            },
            "-workspace".into(),
            workspace.clone(),
            action.into(),
            "-configuration".into(),
            req.config
                .filter(|config| !config.is_empty())
                .unwrap_or(b"Debug")
                .into(),
        ];
        if !has_destination {
            let mut dest = BString::from("generic/platform=");
            dest.extend_from_slice(platform);
            argv.extend(["-destination".into(), dest]);
        }
        argv.push("-parallelizeTargets".into());
        if let Jobs::Count(count) = req.jobs {
            argv.extend(["-jobs".into(), count.to_string().into()]);
        }
        argv.push("-hideShellScriptEnvironment".into());
        argv.extend_from_slice(native_args);
        argv.extend(["-scheme".into(), scheme.into()]);
        BuildCommand {
            argv,
            working_dir: req.build_dir.into(),
        }
    };
    let mut out = Vec::with_capacity(req.targets.len().max(1) + usize::from(req.clean_first));
    if req.clean_first {
        let clean_native = explicit_destination_args(req.native_args);
        out.push(command("clean", b"ALL_BUILD", &clean_native));
    }
    if clean {
        out.push(command("clean", b"ALL_BUILD", req.native_args));
    } else {
        let schemes: Vec<&[u8]> = req
            .targets
            .iter()
            .map(|target| target.as_slice())
            .filter(|target| !target.is_empty())
            .collect();
        if schemes.is_empty() {
            out.push(command("build", b"ALL_BUILD", req.native_args));
        } else {
            for scheme in schemes {
                out.push(command("build", scheme, req.native_args));
            }
        }
    }
    Ok(out)
}

fn build_project_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let clean = req
        .targets
        .iter()
        .any(|target| target.as_slice() == b"clean");
    if clean && req.targets.len() > 1 {
        return Err(GenError::Invalid(
            "Building 'clean' and other targets together is not supported.".into(),
        ));
    }
    let command = |action: &str, targets: &[BString], native_args: &[BString]| {
        let mut argv = vec![if req.make_program.is_empty() {
            "/usr/bin/xcodebuild".into()
        } else {
            req.make_program.into()
        }];
        if !req.project_name.is_empty() {
            let mut project = BString::from(req.project_name);
            project.extend_from_slice(b".xcodeproj");
            argv.extend(["-project".into(), project]);
        }
        argv.extend([
            action.into(),
            "-configuration".into(),
            req.config
                .filter(|config| !config.is_empty())
                .unwrap_or(b"Debug")
                .into(),
            "-parallelizeTargets".into(),
        ]);
        if let Jobs::Count(count) = req.jobs {
            argv.extend(["-jobs".into(), count.to_string().into()]);
        }
        argv.push("-hideShellScriptEnvironment".into());
        argv.extend_from_slice(native_args);
        if targets.is_empty() || matches!(targets, [target] if target.is_empty()) {
            argv.extend(["-target".into(), "ALL_BUILD".into()]);
        } else {
            for target in targets {
                if !target.is_empty() {
                    argv.extend(["-target".into(), target.clone()]);
                }
            }
        }
        BuildCommand {
            argv,
            working_dir: req.build_dir.into(),
        }
    };
    let mut out = Vec::with_capacity(2);
    if req.clean_first {
        out.push(command("clean", &[], &[]));
    }
    out.push(if clean {
        command("clean", &[], req.native_args)
    } else {
        command("build", req.targets, req.native_args)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_base::BString;
    use rcmake_gen_common::ide::{BuildRequest, Jobs};
    use rcmake_host::{Host, PureHost};
    use rcmake_model::GeneratorKind;

    use super::build_commands_with_workspace;

    #[test]
    fn workspace_build_commands_use_workspace_destination_and_schemes() {
        let host = PureHost::default();
        let req = BuildRequest {
            generator: GeneratorKind::Xcode,
            build_dir: b"/work/build",
            project_name: b"XcodeWorkspace",
            make_program: b"xcodebuild",
            config: Some(b"Debug"),
            targets: &[],
            jobs: Jobs::None,
            clean_first: false,
            verbose: false,
            native_args: &[],
            platform: b"",
        };
        // Without a workspace, keep -project/-target.
        let project_cmds =
            build_commands_with_workspace(&req, b"", &host).expect("project commands");
        assert_eq!(project_cmds.len(), 1);
        assert!(project_cmds[0].argv.contains(&BString::from("-project")));
        assert!(project_cmds[0].argv.contains(&BString::from("-target")));

        // With <project>.xcworkspace present, use -workspace, -destination, and -scheme.
        host.create_dir_all(Path::new("/work/build/XcodeWorkspace.xcworkspace"))
            .expect("workspace dir");
        let ws_cmds = build_commands_with_workspace(&req, b"", &host).expect("workspace commands");
        assert_eq!(ws_cmds.len(), 1);
        assert_eq!(
            ws_cmds[0].argv,
            vec![
                BString::from("xcodebuild"),
                "-workspace".into(),
                "XcodeWorkspace.xcworkspace".into(),
                "build".into(),
                "-configuration".into(),
                "Debug".into(),
                "-destination".into(),
                "generic/platform=MacOS".into(),
                "-parallelizeTargets".into(),
                "-hideShellScriptEnvironment".into(),
                "-scheme".into(),
                "ALL_BUILD".into(),
            ]
        );

        // Multiple targets emit one command per scheme, and sysroot sets platform.
        let targets = [BString::from("custom1"), BString::from("custom2")];
        let multi = BuildRequest {
            targets: &targets,
            ..req
        };
        let ios_cmds = build_commands_with_workspace(&multi, b"iphoneos", &host)
            .expect("ios workspace commands");
        assert_eq!(ios_cmds.len(), 2);
        assert!(
            ios_cmds[0]
                .argv
                .contains(&BString::from("generic/platform=iOS"))
        );
        assert_eq!(ios_cmds[0].argv.last(), Some(&BString::from("custom1")));
        assert_eq!(ios_cmds[1].argv.last(), Some(&BString::from("custom2")));
    }

    #[test]
    fn workspace_clean_first_preserves_destination_args() {
        let host = PureHost::default();
        host.create_dir_all(Path::new("/work/build/XcodeWorkspace.xcworkspace"))
            .expect("workspace dir");

        let explicit_native = [
            BString::from("-destination"),
            BString::from("platform=macOS,arch=arm64"),
            BString::from("-quiet"),
        ];
        let req_explicit = BuildRequest {
            generator: GeneratorKind::Xcode,
            build_dir: b"/work/build",
            project_name: b"XcodeWorkspace",
            make_program: b"xcodebuild",
            config: Some(b"Debug"),
            targets: &[],
            jobs: Jobs::None,
            clean_first: true,
            verbose: false,
            native_args: &explicit_native,
            platform: b"",
        };
        let cmds =
            build_commands_with_workspace(&req_explicit, b"", &host).expect("explicit clean-first");
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].argv.contains(&BString::from("clean")));
        assert!(cmds[0].argv.contains(&BString::from("-destination")));
        assert!(
            cmds[0]
                .argv
                .contains(&BString::from("platform=macOS,arch=arm64"))
        );
        assert!(
            !cmds[0]
                .argv
                .contains(&BString::from("generic/platform=MacOS"))
        );
        assert!(!cmds[0].argv.contains(&BString::from("-quiet")));
        assert!(cmds[1].argv.contains(&BString::from("build")));
        assert!(cmds[1].argv.contains(&BString::from("-quiet")));
        assert!(
            cmds[1]
                .argv
                .contains(&BString::from("platform=macOS,arch=arm64"))
        );

        let default_native = [BString::from("-quiet")];
        let req_default = BuildRequest {
            native_args: &default_native,
            ..req_explicit
        };
        let ios_cmds = build_commands_with_workspace(&req_default, b"iphoneos", &host)
            .expect("default clean-first");
        assert_eq!(ios_cmds.len(), 2);
        assert!(
            ios_cmds[0]
                .argv
                .contains(&BString::from("generic/platform=iOS"))
        );
        assert!(
            ios_cmds[1]
                .argv
                .contains(&BString::from("generic/platform=iOS"))
        );
    }
}
