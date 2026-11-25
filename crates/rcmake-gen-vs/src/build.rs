//! Build command plans for MSBuild and devenv.

use rcmake_base::truth::is_on;
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};
use rcmake_host::{Host, ReadRange, path::to_native};
use rcmake_model::{Cache, GeneratorKind, VsVersion};

use crate::{CPP_TYPE, default_platform, solution_ext, string, windows, xml};

/// Suffix of the `STATIC` cache entry that marks a project for package restore.
pub const RESTORE_SUFFIX: &str = "_REQUIRES_VS_PACKAGE_RESTORE";

/// Package restore mode of `--resolve-package-references` or the build preset
/// `resolvePackageReferences` field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestoreMode {
    /// No mode given: restore unless `CMAKE_VS_NUGET_PACKAGE_RESTORE` is off.
    #[default]
    Default,
    /// `on`: restore the packages, then build.
    On,
    /// `off`: build without a restore.
    Off,
    /// `only`: restore the packages and do not build.
    Only,
}

/// Return the native processes for a Visual Studio build request, in run order.
///
/// This plan adds no package restore arguments. Use
/// [`build_commands_with_restore`] to apply a restore mode.
///
/// # Errors
///
/// Returns an error if the requested generator is not Visual Studio.
pub fn build_commands(
    req: &BuildRequest<'_>,
    host: &dyn Host,
) -> Result<Vec<BuildCommand>, GenError> {
    build_commands_with_restore(req, RestoreMode::Default, &Cache::default(), host)
}

/// Return the native processes for a Visual Studio build request with a
/// package restore mode, in run order.
///
/// A restore applies only to an MSBuild project whose
/// `<target>_REQUIRES_VS_PACKAGE_RESTORE` cache entry is on. `only` adds
/// `/t:Restore`. `on` adds `/restore`. The default mode adds `/restore`
/// unless the cache sets `CMAKE_VS_NUGET_PACKAGE_RESTORE` to a false value.
/// `devenv` and `clean` commands do not change.
///
/// # Errors
///
/// Returns an error if the requested generator is not Visual Studio.
pub fn build_commands_with_restore(
    req: &BuildRequest<'_>,
    restore: RestoreMode,
    cache: &Cache,
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
                if let Some(flag) = restore_flag(restore, cache, target) {
                    argv.push(flag.into());
                }
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

/// Select the restore argument for one MSBuild project.
fn restore_flag(mode: RestoreMode, cache: &Cache, target: &str) -> Option<&'static str> {
    let cached = |name: &[u8]| cache.get(name).map(|entry| is_on(&entry.value));
    if mode == RestoreMode::Off
        || cached(format!("{target}{RESTORE_SUFFIX}").as_bytes()) != Some(true)
    {
        return None;
    }
    match mode {
        RestoreMode::Only => Some("/t:Restore"),
        RestoreMode::Default if cached(b"CMAKE_VS_NUGET_PACKAGE_RESTORE") == Some(false) => None,
        _ => Some("/restore"),
    }
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

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_gen_common::ide::{BuildRequest, Jobs};
    use rcmake_host::{Host, PureHost, WriteMode, path::to_native};
    use rcmake_model::{Cache, CacheEntry, CacheType, GeneratorKind, VsVersion};

    use super::{RestoreMode, build_commands, build_commands_with_restore};
    use crate::string;

    fn cache(entries: &[(&str, &str)]) -> Cache {
        let mut cache = Cache::default();
        for (name, value) in entries {
            cache.insert(
                (*name).into(),
                CacheEntry {
                    value: (*value).into(),
                    ty: CacheType::Static,
                    help: BString::default(),
                    advanced: false,
                    props: Default::default(),
                },
            );
        }
        cache
    }

    struct Case<'a> {
        version: VsVersion,
        make_program: &'a [u8],
        targets: &'a [BString],
        clean_first: bool,
    }

    fn plan(
        host: &PureHost,
        case: &Case<'_>,
        mode: RestoreMode,
        cache: &Cache,
    ) -> Vec<Vec<String>> {
        let request = BuildRequest {
            generator: GeneratorKind::VisualStudio(case.version),
            build_dir: b"/out",
            project_name: b"Demo",
            make_program: case.make_program,
            config: Some(b"Release"),
            targets: case.targets,
            jobs: Jobs::None,
            clean_first: case.clean_first,
            verbose: false,
            native_args: &["/nologo".into()],
            platform: b"x64",
        };
        build_commands_with_restore(&request, mode, cache, host)
            .expect("plan build")
            .iter()
            .map(|command| command.argv.iter().map(|arg| string(arg)).collect())
            .collect()
    }

    /// The argument after the project path, or `None` for no restore flag.
    fn restore_arg(command: &[String]) -> Option<&str> {
        command
            .get(2)
            .map(String::as_str)
            .filter(|arg| !arg.starts_with("/p:"))
    }

    #[test]
    fn msbuild_restore_mode_uses_cached_target_requirement() {
        let host = PureHost::default();
        let required = cache(&[("app_REQUIRES_VS_PACKAGE_RESTORE", "ON")]);
        let nuget_off = cache(&[
            ("app_REQUIRES_VS_PACKAGE_RESTORE", "ON"),
            ("CMAKE_VS_NUGET_PACKAGE_RESTORE", "OFF"),
        ]);
        let nuget_on = cache(&[
            ("app_REQUIRES_VS_PACKAGE_RESTORE", "ON"),
            ("CMAKE_VS_NUGET_PACKAGE_RESTORE", "ON"),
        ]);
        let app = ["app".into()];
        for version in [VsVersion::Vs17, VsVersion::Vs18] {
            let case = Case {
                version,
                make_program: b"MSBuild.exe",
                targets: &app,
                clean_first: false,
            };
            for (mode, cache, expected) in [
                (RestoreMode::Default, &required, Some("/restore")),
                (RestoreMode::Default, &nuget_off, None),
                (RestoreMode::Default, &nuget_on, Some("/restore")),
                (RestoreMode::On, &required, Some("/restore")),
                (RestoreMode::On, &nuget_off, Some("/restore")),
                (RestoreMode::Off, &required, None),
                (RestoreMode::Only, &required, Some("/t:Restore")),
                (RestoreMode::Only, &nuget_off, Some("/t:Restore")),
                (RestoreMode::On, &Cache::default(), None),
            ] {
                let commands = plan(&host, &case, mode, cache);
                let [command] = commands.as_slice() else {
                    panic!("one command expected: {commands:?}");
                };
                assert_eq!(restore_arg(command), expected, "{mode:?} {version:?}");
                assert_eq!(command.get(1).map(String::as_str), Some("app.vcxproj"));
                assert_eq!(command.last().map(String::as_str), Some("/nologo"));
            }

            // A target with no requirement and a requirement that is off.
            let mixed = cache(&[
                ("app_REQUIRES_VS_PACKAGE_RESTORE", "ON"),
                ("off_REQUIRES_VS_PACKAGE_RESTORE", "OFF"),
            ]);
            let targets = ["plain".into(), "off".into(), "app".into()];
            let case = Case {
                targets: &targets,
                ..case
            };
            let commands = plan(&host, &case, RestoreMode::On, &mixed);
            let flags: Vec<_> = commands
                .iter()
                .map(|command| restore_arg(command))
                .collect();
            assert_eq!(flags, [None, None, Some("/restore")]);

            // The clean command of `--clean-first` has no restore flag.
            let case = Case {
                targets: &app,
                clean_first: true,
                ..case
            };
            let commands = plan(&host, &case, RestoreMode::On, &required);
            assert_eq!(commands.len(), 2);
            assert_eq!(commands[0].get(2).map(String::as_str), Some("/t:Clean"));
            assert!(!commands[0].iter().any(|arg| arg.contains("estore")));
            assert_eq!(restore_arg(&commands[1]), Some("/restore"));

            // The default target uses the `ALL_BUILD` requirement.
            let all = cache(&[("ALL_BUILD_REQUIRES_VS_PACKAGE_RESTORE", "ON")]);
            let case = Case {
                targets: &[],
                clean_first: false,
                ..case
            };
            let commands = plan(&host, &case, RestoreMode::Only, &all);
            assert_eq!(
                commands[0].get(1).map(String::as_str),
                Some("ALL_BUILD.vcxproj")
            );
            assert_eq!(restore_arg(&commands[0]), Some("/t:Restore"));

            // `devenv` builds do not restore.
            let case = Case {
                make_program: br"C:\Tools\devenv.com",
                targets: &app,
                ..case
            };
            let commands = plan(&host, &case, RestoreMode::On, &required);
            assert!(!commands[0].iter().any(|arg| arg.contains("estore")));
        }
    }

    #[test]
    fn msbuild_restore_applies_to_subdirectory_projects() {
        let host = PureHost::default();
        let dir = to_native(b"/out");
        host.create_dir_all(&dir).expect("create build directory");
        host.write(
            &dir.join("Demo.sln"),
            b"Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"app\", \"sub\\app.vcxproj\", \"{00000000-0000-0000-0000-000000000000}\"\r\nEndProject\r\n",
            WriteMode::Truncate,
        )
        .expect("write solution");
        host.write(
            &dir.join("Demo.slnx"),
            b"<Solution>\n  <Project Path=\"sub/app.vcxproj\" />\n</Solution>\n",
            WriteMode::Truncate,
        )
        .expect("write solution");
        let required = cache(&[("app_REQUIRES_VS_PACKAGE_RESTORE", "ON")]);
        let app = ["app".into()];
        for version in [VsVersion::Vs17, VsVersion::Vs18] {
            let case = Case {
                version,
                make_program: b"",
                targets: &app,
                clean_first: false,
            };
            let commands = plan(&host, &case, RestoreMode::On, &required);
            assert_eq!(
                commands[0].get(1).map(String::as_str),
                Some(r"sub\app.vcxproj")
            );
            assert_eq!(restore_arg(&commands[0]), Some("/restore"));
        }
    }

    #[test]
    fn build_commands_never_restores() {
        let host = PureHost::default();
        let targets = ["app".into()];
        let request = BuildRequest {
            generator: GeneratorKind::VisualStudio(VsVersion::Vs17),
            build_dir: b"/out",
            project_name: b"Demo",
            make_program: b"",
            config: None,
            targets: &targets,
            jobs: Jobs::Default,
            clean_first: false,
            verbose: false,
            native_args: &[],
            platform: b"x64",
        };
        let commands = build_commands(&request, &host).expect("plan build");
        assert!(
            !commands[0]
                .argv
                .iter()
                .any(|arg| string(arg).contains("estore"))
        );
    }
}
