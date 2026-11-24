//! `cmake --build` plan for the `Unix Makefiles` generator.

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};

/// The `make` processes of `cmake --build`, in run order.
///
/// - `make -C <dir> [-j [<n>]] [VERBOSE=1] <targets> <native args>`
/// - `-j` without a count means the default parallel level.
/// - With `--clean-first`, a `clean` command comes first (unless the only target is
///   `clean`).
///
/// # Errors
///
/// This function does not fail. The `Result` keeps one signature for all generators.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    let program: BString = if req.make_program.is_empty() {
        "make".into()
    } else {
        req.make_program.into()
    };
    let mut base = vec![program, "-C".into(), req.build_dir.into()];
    match req.jobs {
        Jobs::Count(count) => base.extend(["-j".into(), count.to_string().into()]),
        Jobs::Default => base.push("-j".into()),
        Jobs::None => {}
    }
    if req.verbose {
        base.push("VERBOSE=1".into());
    }
    Ok(rcmake_gen_common::ide::with_clean_first(
        req, &base, "clean",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_model::GeneratorKind;

    #[test]
    fn make_plan_matches_cli_argv() {
        let targets = [BString::from("all")];
        let native = [BString::from("-k")];
        let req = BuildRequest {
            generator: GeneratorKind::UnixMakefiles,
            build_dir: b"/b",
            project_name: b"Demo",
            make_program: b"",
            config: Some(b"Debug"),
            targets: &targets,
            jobs: Jobs::Default,
            clean_first: true,
            verbose: true,
            native_args: &native,
            platform: b"",
        };
        let commands = build_commands(&req).unwrap_or_default();
        let argv: Vec<Vec<BString>> = commands.into_iter().map(|command| command.argv).collect();
        let expected = |target: &str| -> Vec<BString> {
            ["make", "-C", "/b", "-j", "VERBOSE=1", target, "-k"]
                .map(BString::from)
                .into()
        };
        assert_eq!(argv, [expected("clean"), expected("all")]);
    }
}
