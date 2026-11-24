//! Build command planning for Xcode.

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};

/// Return the `xcodebuild` commands in execution order.
///
/// # Errors
///
/// Returns an error when the request combines `clean` with other targets.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
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
        let mut project = BString::from(req.project_name);
        project.extend_from_slice(b".xcodeproj");
        let mut argv = vec![
            if req.make_program.is_empty() {
                "/usr/bin/xcodebuild".into()
            } else {
                req.make_program.into()
            },
            "-project".into(),
            project,
            action.into(),
            "-configuration".into(),
            req.config.unwrap_or(b"Debug").into(),
            "-parallelizeTargets".into(),
        ];
        if let Jobs::Count(count) = req.jobs {
            argv.extend(["-jobs".into(), count.to_string().into()]);
        }
        argv.push("-hideShellScriptEnvironment".into());
        if targets.is_empty() {
            argv.extend(["-target".into(), "ALL_BUILD".into()]);
        } else {
            for target in targets {
                if !target.is_empty() {
                    argv.extend(["-target".into(), target.clone()]);
                }
            }
        }
        argv.extend_from_slice(native_args);
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
