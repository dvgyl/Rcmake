use rcmake_base::{BString, truth};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_model::{ExportDependencyMode, ExportDependencySetup};

use super::fail;

pub(super) fn enabled(cx: &mut Cx<'_>) -> bool {
    crate::policy_misc::experimental::feature_enabled(cx, b"ExportPackageDependencies")
        .unwrap_or_default()
}

pub(super) fn setup(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [set, options @ ..] = args else {
        return Err(fail("SETUP requires an export set name."));
    };
    let mut options = options;
    while let Some((&key, tail)) = options.split_first() {
        let (section, rest) = tail.split_at(
            tail.iter()
                .position(|value| *value == b"PACKAGE_DEPENDENCY")
                .unwrap_or(tail.len()),
        );
        options = rest;
        if key != b"PACKAGE_DEPENDENCY" {
            return Err(super::unknown(key));
        }
        let [name, settings @ ..] = section else {
            return Err(fail("PACKAGE_DEPENDENCY requires a package name."));
        };
        let mut setup = ExportDependencySetup::default();
        let mut words = settings.iter().copied().peekable();
        while let Some(option) = words.next() {
            match option {
                b"ENABLED" => {
                    setup.mode = match words.next() {
                        Some(b"AUTO") => ExportDependencyMode::Auto,
                        Some(value) if truth::is_on(value) => ExportDependencyMode::On,
                        Some(value) if truth::is_off(value) => ExportDependencyMode::Off,
                        _ => {
                            return Err(fail(
                                "PACKAGE_DEPENDENCY ENABLED requires ON, OFF, or AUTO.",
                            ));
                        }
                    };
                }
                b"EXTRA_ARGS" => {
                    while let Some(arg) =
                        words.next_if(|word| !matches!(*word, b"ENABLED" | b"EXTRA_ARGS"))
                    {
                        setup.args.push(BString::from(arg));
                    }
                }
                _ => return Err(super::unknown(option)),
            }
        }
        cx.project()
            .export_dependencies
            .entry(BString::from(*set))
            .or_default()
            .insert(BString::from(*name), setup);
    }
    Ok(Flow::Normal)
}
