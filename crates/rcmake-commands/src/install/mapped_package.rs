use rcmake_base::{BString, list};
use rcmake_eval::Cx;
use rcmake_expr::configure::ConfigureOptions;
use rcmake_model::{InstallKind, InstallOptions, InstallRule};

use super::{Result, destination, fail, metadata};
use crate::policy_misc::experimental::feature_enabled;
use crate::string_math::configure_text;

const VARIABLE: &[u8] = b"CMAKE_INSTALL_EXPORTS_AS_PACKAGE_INFO";

pub(super) fn rules(
    cx: &mut Cx<'_>,
    set: &[u8],
    options: &InstallOptions,
) -> Result<Vec<InstallRule>> {
    if !feature_enabled(cx, b"MappedPackageInfo")? {
        return Ok(Vec::new());
    }
    let mapping = cx.var_or_empty(VARIABLE);
    list::split(&mapping, list::Empty::Drop)
        .iter()
        .filter_map(|value| split_once(value, b':'))
        .filter(|(name, _)| *name == set)
        .map(|(_, directive)| rule(cx, set, directive, options))
        .collect()
}

fn rule(
    cx: &mut Cx<'_>,
    set: &[u8],
    directive: &[u8],
    options: &InstallOptions,
) -> Result<InstallRule> {
    let set_name = BString::from(set);
    let mut parts = directive.splitn(3, |&byte| byte == b'/');
    let package = parts.next().filter(|name| !name.is_empty()).ok_or_else(|| {
        fail(format!(
            "CMAKE_INSTALL_EXPORTS_AS_PACKAGE_INFO missing package name for export \"{set_name}\"."
        ))
    })?;
    let flags = parts.next().unwrap_or_default();
    let mut tokens = vec![package.into(), "EXPORT".into(), set.into()];
    let mut directive = flags;
    if let Some(rest) = directive.strip_prefix(b"l") {
        tokens.push("LOWER_CASE_FILE".into());
        directive = rest;
    }
    if let Some(appendix) = directive.strip_prefix(b"a") {
        if appendix.is_empty() {
            return Err(fail(format!(
                "CMAKE_INSTALL_EXPORTS_AS_PACKAGE_INFO given APPENDIX directive for export \"{set_name}\", but no appendix name was provided."
            )));
        }
        tokens.extend(["APPENDIX".into(), appendix.into()]);
    } else if directive.is_empty() {
        add_metadata(cx, set, &mut tokens)?;
    } else {
        return Err(fail(format!(
            "CMAKE_INSTALL_EXPORTS_AS_PACKAGE_INFO given unrecognized directive \"{}\".",
            BString::from(flags)
        )));
    }
    let spec = metadata::mapped_spec(cx, &tokens)?;
    let options = InstallOptions {
        destination: parts
            .next()
            .filter(|path| !path.is_empty())
            .map(|path| destination(cx, path)),
        component: options.component.clone(),
        permissions: options.permissions.clone(),
        configurations: options.configurations.clone(),
        exclude_from_all: options.exclude_from_all,
        ..Default::default()
    };
    Ok(InstallRule {
        dir: cx.current_dir_id(),
        backtrace: cx.backtrace(),
        kind: InstallKind::PackageInfo(Box::new(spec)),
        options,
    })
}

fn add_metadata(cx: &mut Cx<'_>, set: &[u8], tokens: &mut Vec<BString>) -> Result<()> {
    for key in [
        b"VERSION".as_slice(),
        b"COMPAT_VERSION",
        b"VERSION_SCHEMA",
        b"LICENSE",
        b"DEFAULT_LICENSE",
        b"DESCRIPTION",
        b"HOMEPAGE_URL",
        b"DEFAULT_TARGETS",
        b"DEFAULT_CONFIGURATIONS",
    ] {
        let name = [set, b"_EXPORT_PACKAGE_INFO_", key].concat();
        let value = cx.var_or_empty(&name);
        let expanded = configure_text(
            cx,
            &value,
            ConfigureOptions {
                at_only: true,
                ..Default::default()
            },
        )?;
        if expanded.is_empty() {
            continue;
        }
        tokens.push(key.into());
        if matches!(key, b"DEFAULT_CONFIGURATIONS" | b"DEFAULT_TARGETS") {
            tokens.extend(list::split(&expanded, list::Empty::Drop));
        } else {
            tokens.push(expanded.into());
        }
    }
    Ok(())
}

fn split_once(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let (before, after) = bytes.split_at(bytes.iter().position(|&byte| byte == delimiter)?);
    Some((before, after.get(1..)?))
}
