//! `cmake --build` plan for the `Unix Makefiles` and `NMake Makefiles` generators.

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_gen_common::ide::{BuildCommand, BuildRequest, Jobs};

use crate::MakeDialect;

/// The `make` processes of `cmake --build` for `Unix Makefiles`, in run order.
///
/// - Run `make -f Makefile [-j [<n>]] [VERBOSE=1] <targets> <native args>` in the build directory.
/// - `-j` without a count means the default parallel level.
/// - With `--clean-first`, a `clean` command comes first (unless the only target is
///   `clean`).
///
/// # Errors
///
/// This function does not fail. The `Result` keeps one signature for all generators.
pub fn build_commands(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    build_commands_for_dialect(req, MakeDialect::Unix)
}

/// The `nmake` processes of `cmake --build` for `NMake Makefiles`, in run order.
///
/// - Run `nmake /nologo -f Makefile [VERBOSE=1] <targets> <native args>` in the build directory.
/// - Parallel job flags (`-j`) are ignored because NMake does not support parallel builds.
/// - With `--clean-first`, a `clean` command comes first (unless the only target is
///   `clean`).
///
/// # Errors
///
/// This function does not fail. The `Result` keeps one signature for all generators.
pub fn build_commands_nmake(req: &BuildRequest<'_>) -> Result<Vec<BuildCommand>, GenError> {
    build_commands_for_dialect(req, MakeDialect::NMake)
}

/// The build-tool processes of `cmake --build` for `dialect`, in run order.
///
/// # Errors
///
/// This function does not fail. The `Result` keeps one signature for all generators.
pub fn build_commands_for_dialect(
    req: &BuildRequest<'_>,
    dialect: MakeDialect,
) -> Result<Vec<BuildCommand>, GenError> {
    let default_program = match dialect {
        MakeDialect::Unix => "make",
        MakeDialect::NMake => "nmake",
    };
    let program: BString = if req.make_program.is_empty() {
        default_program.into()
    } else {
        req.make_program.into()
    };
    let mut base = vec![program];
    if dialect.is_nmake() {
        base.push("/nologo".into());
    }
    base.extend(["-f".into(), "Makefile".into()]);
    if !dialect.is_nmake() {
        match req.jobs {
            Jobs::Count(count) => base.extend(["-j".into(), count.to_string().into()]),
            Jobs::Default => base.push("-j".into()),
            Jobs::None => {}
        }
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
            ["make", "-f", "Makefile", "-j", "VERBOSE=1", target, "-k"]
                .map(BString::from)
                .into()
        };
        assert_eq!(argv, [expected("clean"), expected("all")]);
    }

    #[test]
    fn nmake_plan_uses_nologo_and_ignores_parallel_jobs() {
        let targets = [BString::from("app")];
        let native = [BString::from("/I")];
        let req = BuildRequest {
            generator: GeneratorKind::UnixMakefiles,
            build_dir: b"D:/b",
            project_name: b"Demo",
            make_program: b"",
            config: Some(b"Release"),
            targets: &targets,
            jobs: Jobs::Count(8),
            clean_first: true,
            verbose: true,
            native_args: &native,
            platform: b"",
        };
        let commands = build_commands_nmake(&req).unwrap_or_default();
        let argv: Vec<Vec<BString>> = commands.into_iter().map(|command| command.argv).collect();
        let expected = |target: &str| -> Vec<BString> {
            [
                "nmake",
                "/nologo",
                "-f",
                "Makefile",
                "VERBOSE=1",
                target,
                "/I",
            ]
            .map(BString::from)
            .into()
        };
        assert_eq!(argv, [expected("clean"), expected("app")]);
    }
}
