//! Check direct and FetchContent transfers before they use network or file input.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::hermetic::{AuditDecision, HermeticSite, ViolationKind};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{HostResult, path};

use super::{HermeticHost, require_allowed};

pub(super) fn check(
    cx: &Cx<'_>,
    url: &str,
    pinned: bool,
    dependency: Option<&str>,
) -> Result<(), CmdError> {
    let Some(host) = cx.session_data::<Arc<HermeticHost>>() else {
        return Ok(());
    };
    let site = if dependency.is_some() {
        let state = host.state();
        cx.backtrace()
            .frames()
            .filter(|frame| frame.line > 0)
            .find_map(|frame| {
                let file = path::to_native(&frame.file);
                state
                    .inputs
                    .project_file(&file)
                    .then(|| HermeticSite::new(&file, frame.line))
            })
            .unwrap_or_else(|| host.context.site())
    } else {
        host.context.site()
    };
    check_url(host, &site, url, pinned, dependency).map_err(|_| CmdError::Reported)
}

pub(super) fn check_curl(
    host: &HermeticHost,
    site: &HermeticSite,
    program: &Path,
    executable: &Path,
    args: &[BString],
    cwd: &Path,
    env: &[(BString, BString)],
) -> HostResult<()> {
    let curl = [program, executable].into_iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("curl") || name.eq_ignore_ascii_case("curl.exe")
            })
    });
    if !curl {
        return Ok(());
    }
    let mut args = args.iter().map(|arg| arg.as_slice());
    let mut upload = false;
    let mut url = None;
    let mut netrc = false;
    let mut netrc_file = false;
    let check_input = |value: Option<&[u8]>| -> HostResult<()> {
        if let Some(value) = value {
            host.guard_path(&cwd.join(path::to_native(value)))?;
        }
        Ok(())
    };
    while let Some(flag) = args.next() {
        match flag {
            b"--upload-file" => {
                upload = true;
                check_input(args.next())?;
            }
            b"--url" => url = args.next(),
            b"--netrc" | b"--netrc-optional" => netrc = true,
            b"--netrc-file" => {
                netrc_file = true;
                check_input(args.next())?;
            }
            b"--cacert" => check_input(args.next())?,
            b"--config" => check_input(args.next().filter(|value| *value != b"-"))?,
            b"--header" => check_input(args.next().and_then(|value| value.strip_prefix(b"@")))?,
            b"--range" | b"--max-time" | b"--speed-limit" | b"--speed-time" | b"--output" => {
                args.next();
            }
            _ => {}
        }
    }
    if upload && let Some(url) = url {
        check_url(host, site, &String::from_utf8_lossy(url), false, None)?;
    }
    if netrc && !netrc_file {
        let home = env
            .iter()
            .rev()
            .find_map(|(name, value)| {
                (name == b"HOME" || (host.windows && name.eq_ignore_ascii_case(b"HOME")))
                    .then(|| path::to_native(value))
            })
            .unwrap_or_default();
        host.guard_path(&home.join(".netrc"))?;
        if host.windows {
            host.guard_path(&home.join("_netrc"))?;
        }
    }
    Ok(())
}

fn check_url(
    host: &HermeticHost,
    site: &HermeticSite,
    url: &str,
    pinned: bool,
    dependency: Option<&str>,
) -> HostResult<()> {
    let file_url = url
        .split_once(':')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("file"));
    let local = file_url
        .and_then(|(_, rest)| local_file(rest, host.windows))
        .or_else(|| {
            let path = path::to_native(url.as_bytes());
            (dependency.is_some() && path.is_absolute()).then_some(path)
        });
    if let Some(path) = local {
        let _scope = host.context.enter(site.clone());
        return host.guard_path(&path).map(|_| ());
    }
    let pinned = pinned && file_url.is_none();
    let decision = host.audit(|audit| match dependency {
        Some(name) => audit.check_fetch_network(site, url, pinned, name),
        None => audit.check_network(site, url, pinned),
    });
    require_allowed(decision, ViolationKind::NetworkAccess)?;
    if decision == AuditDecision::Allow {
        host.state().inputs.network(url);
    }
    Ok(())
}

fn local_file(rest: &str, windows: bool) -> Option<PathBuf> {
    let rest = rest.strip_prefix("//")?;
    let start = rest.find('/')?;
    let (authority, path) = rest.split_at(start);
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let mut encoded = path.split(['?', '#']).next()?.bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = encoded.next() {
        if byte == b'%' {
            let high = char::from(encoded.next()?).to_digit(16)?;
            let low = char::from(encoded.next()?).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
        } else {
            decoded.push(byte);
        }
    }
    if decoded.contains(&0) {
        return None;
    }
    let bytes = if windows {
        match decoded.as_slice() {
            [b'/', drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => decoded.get(1..)?,
            _ => return None,
        }
    } else {
        decoded.as_slice()
    };
    let path = path::to_native(bytes);
    path.is_absolute().then_some(path)
}
