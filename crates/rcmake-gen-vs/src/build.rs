//! Build command plans for MSBuild and devenv.

use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};
use rcmake_host::{Host, ReadRange, path::to_native};
use rcmake_model::{GeneratorKind, VsVersion};

use crate::{CPP_TYPE, default_platform, solution_ext, string, windows, xml};

/// Return the native processes for a Visual Studio build request, in run order.
///
/// # Errors
///
/// Returns an error if the requested generator is not Visual Studio.
pub fn build_commands(
    req: &BuildRequest<'_>,
    host: &dyn Host,
) -> Result<Vec<BuildCommand>, GenError> {
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
    let devenv = program.to_ascii_lowercase().contains("devenv");
    let config = req
        .config
        .filter(|value| !value.is_empty())
        .map_or_else(|| "Debug".into(), string);
    let platform = if req.platform.is_empty() {
        default_platform(&host.system()).into()
    } else {
        string(req.platform)
    };
    let project = if req.project_name.is_empty() {
        "Project".into()
    } else {
        string(req.project_name)
    };
    let solution = format!("{project}.{}", solution_ext(version));
    let needs_project_lookup = !devenv
        && req.targets.iter().any(|target| {
            !target.is_empty()
                && target.as_slice() != b"clean"
                && !target.contains(&b'/')
                && !target.contains(&b'\\')
        });
    let solution_data = if needs_project_lookup {
        let path = to_native(req.build_dir).join(&solution);
        host.read(
            &path,
            ReadRange::All {
                limit: 64 * 1024 * 1024,
            },
        )
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
    } else {
        None
    };
    let command = |target: &str, clean_first: bool| {
        let target = if target.is_empty() {
            "ALL_BUILD"
        } else {
            target
        };
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
                let path = if target.contains(['/', '\\']) {
                    windows(target)
                } else {
                    let name = target.strip_suffix(".vcxproj").unwrap_or(target);
                    solution_data
                        .as_deref()
                        .and_then(|data| project_path(data, version, name))
                        .unwrap_or_else(|| target.into())
                };
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
        if !clean_first {
            argv.extend_from_slice(req.native_args);
        }
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
        commands.push(command("clean", true));
    }
    if req.targets.is_empty() {
        commands.push(command("ALL_BUILD", false));
    } else {
        commands.extend(
            req.targets
                .iter()
                .map(|target| command(&string(target), false)),
        );
    }
    Ok(commands)
}

fn project_path(solution: &str, version: VsVersion, target: &str) -> Option<String> {
    if version.writes_slnx() {
        return solution.lines().find_map(|line| {
            // The solution writer puts the Path attribute first.
            let (path, _) = line
                .trim_start()
                .strip_prefix("<Project Path=\"")?
                .split_once('"')?;
            let path = xml::unescape_attribute(path);
            let (name, extension) = path.rsplit(['/', '\\']).next()?.rsplit_once('.')?;
            (extension.eq_ignore_ascii_case("vcxproj") && name.eq_ignore_ascii_case(target))
                .then(|| windows(&path))
        });
    }
    solution.lines().find_map(|line| {
        let (kind, fields) = line.strip_prefix("Project(\"")?.split_once("\") = \"")?;
        if !kind.eq_ignore_ascii_case(CPP_TYPE) {
            return None;
        }
        let (name, fields) = fields.split_once("\", \"")?;
        let (path, _) = fields.rsplit_once("\", \"")?;
        let (_, extension) = path.rsplit_once('.')?;
        (extension.eq_ignore_ascii_case("vcxproj") && name.eq_ignore_ascii_case(target))
            .then(|| windows(&path.replace("\"\"", "\"")))
    })
}
