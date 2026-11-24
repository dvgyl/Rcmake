//! `cmake --build` plan for the Ninja generators.

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};
use rcmake_model::GeneratorKind;

/// The `ninja` processes of `cmake --build`, in run order.
///
/// - `ninja -C <dir> [-f build-<config>.ninja] [-j <n>] [-v] <targets> <native args>`
/// - The `-f` file is used only by `Ninja Multi-Config` with `--config`.
/// - With `--clean-first`, a `clean` command comes first (unless the only target is
///   `clean`).
///
/// # Errors
///
/// This function does not fail. The `Result` keeps one signature for all generators.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let program: BString = if req.make_program.is_empty() {
        "ninja".into()
    } else {
        req.make_program.into()
    };
    let mut base = vec![program, "-C".into(), req.build_dir.into()];
    if req.generator == GeneratorKind::NinjaMultiConfig
        && let Some(config) = req.config.filter(|config| !config.is_empty())
    {
        let mut file = BString::from("build-");
        file.extend_from_slice(config);
        file.extend_from_slice(b".ninja");
        base.extend(["-f".into(), file]);
    }
    if let Jobs::Count(count) = req.jobs {
        base.extend(["-j".into(), count.to_string().into()]);
    }
    if req.verbose {
        base.push("-v".into());
    }
    Ok(rcmake_gen_common::ide::with_clean_first(
        req, &base, "clean",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(generator: GeneratorKind, targets: &'a [BString]) -> BuildRequest<'a> {
        BuildRequest {
            generator,
            build_dir: b"/b",
            project_name: b"Demo",
            make_program: b"/bin/ninja",
            config: Some(b"Release"),
            targets,
            jobs: Jobs::Count(4),
            clean_first: true,
            verbose: true,
            native_args: &[],
            platform: b"",
        }
    }

    fn argv(commands: &[BuildCommand]) -> Vec<String> {
        commands
            .iter()
            .map(|command| {
                command
                    .argv
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    #[test]
    fn multi_config_selects_config_file_and_cleans_first() {
        let targets = [BString::from("app")];
        let commands = build_commands(&request(GeneratorKind::NinjaMultiConfig, &targets));
        let commands = commands.unwrap_or_default();
        assert_eq!(
            argv(&commands),
            [
                "/bin/ninja -C /b -f build-Release.ninja -j 4 -v clean",
                "/bin/ninja -C /b -f build-Release.ninja -j 4 -v app",
            ]
        );
        assert!(commands.iter().all(|command| command.working_dir == "/b"));
    }

    #[test]
    fn empty_config_uses_default_manifest() {
        let mut req = request(GeneratorKind::NinjaMultiConfig, &[]);
        req.config = Some(b"");
        req.clean_first = false;
        assert_eq!(
            argv(&build_commands(&req).unwrap_or_default()),
            ["/bin/ninja -C /b -j 4 -v"]
        );
    }

    #[test]
    fn single_config_ignores_config_and_default_jobs() {
        let targets = [BString::from("clean")];
        let mut req = request(GeneratorKind::Ninja, &targets);
        req.make_program = b"";
        req.jobs = Jobs::Default;
        req.verbose = false;
        let native = [BString::from("-k0")];
        req.native_args = &native;
        assert_eq!(
            argv(&build_commands(&req).unwrap_or_default()),
            ["ninja -C /b clean -k0"]
        );
    }
}
