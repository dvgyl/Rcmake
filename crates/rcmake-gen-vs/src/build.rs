//! Build command plans for MSBuild and devenv.

use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};
use rcmake_model::GeneratorKind;

use crate::{default_platform, solution_ext, string, windows};

/// Return the native processes for a Visual Studio build request, in run order.
///
/// # Errors
///
/// Returns an error if the requested generator is not Visual Studio.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let GeneratorKind::VisualStudio(version) = req.generator else {
        return Err(GenError::Invalid(
            "A Visual Studio generator is required.".into(),
        ));
    };
    let program = if req.make_program.is_empty() {
        "MSBuild.exe".into()
    } else {
        string(req.make_program)
    };
    let devenv = program.rsplit(['/', '\\']).next().is_some_and(|file| {
        matches!(
            file.to_ascii_lowercase().as_str(),
            "devenv" | "devenv.com" | "devenv.exe"
        )
    });
    let config = req
        .config
        .filter(|value| !value.is_empty())
        .map_or_else(|| "Debug".into(), string);
    let platform = if req.platform.is_empty() {
        default_platform().into()
    } else {
        string(req.platform)
    };
    let project = if req.project_name.is_empty() {
        "Project".into()
    } else {
        string(req.project_name)
    };
    let solution = format!("{project}.{}", solution_ext(version));
    let command = |target: &str| {
        let clean = target == "clean";
        let mut argv = vec![program.as_str().into()];
        if devenv {
            argv.push(solution.as_str().into());
            argv.push(if clean { "/Clean" } else { "/Build" }.into());
            argv.push(format!("{config}|{platform}").into());
            if !clean {
                argv.extend(["/Project".into(), target.into()]);
            }
        } else {
            if clean {
                argv.extend([solution.as_str().into(), "/t:Clean".into()]);
            } else {
                let path = windows(target);
                argv.push(if path.to_ascii_lowercase().ends_with(".vcxproj") {
                    path.into()
                } else {
                    format!("{path}.vcxproj").into()
                });
            }
            argv.extend([
                format!("/p:Configuration={config}").into(),
                format!("/p:Platform={platform}").into(),
                format!("/p:VisualStudioVersion={}", version.tools_version()).into(),
            ]);
            match req.jobs {
                Jobs::None => {}
                Jobs::Default => argv.push("/m".into()),
                Jobs::Count(count) => argv.push(format!("/m:{count}").into()),
            }
            argv.push(if req.verbose { "/v:n" } else { "/v:m" }.into());
        }
        argv.extend_from_slice(req.native_args);
        BuildCommand {
            argv,
            working_dir: req.build_dir.into(),
        }
    };
    let mut commands = Vec::new();
    if req.clean_first
        && !req
            .targets
            .first()
            .is_some_and(|target| target.as_slice() == b"clean")
    {
        commands.push(command("clean"));
    }
    if req.targets.is_empty() {
        commands.push(command("ALL_BUILD"));
    } else {
        commands.extend(req.targets.iter().map(|target| command(&string(target))));
    }
    Ok(commands)
}
