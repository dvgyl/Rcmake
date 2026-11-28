// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use rcmake_eval::journal::JournalDigest;
use rcmake_host::{Host, ProcessResult, ReadRange, RunOutput, RunRequest};
use rcmake_store::{Digest, Domain, KeyHasher, file_digest};

use super::Query;

pub(super) fn verify(
    host: &dyn Host,
    query: &Query,
    executable: Digest,
    module: &Path,
    request: &RunRequest,
) -> Option<(JournalDigest, RunOutput)> {
    let arm_capability = query.program == Path::new("/usr/sbin/sysctl");
    let product = query.program == Path::new("/usr/bin/sw_vers");
    let uname = query.program == Path::new("/usr/bin/uname");
    if !arm_capability && !product && !uname {
        return None;
    }
    if uname && super::has_uname_override(&request.env) {
        return None;
    }
    let system = host.system();
    if system.sysname != "Darwin" {
        return None;
    }

    let mut key = KeyHasher::new(Domain::ConfigureV1);
    key.field(0, b"Darwin query receipt v1");
    key.field(1, query.program.as_os_str().as_encoded_bytes());
    key.field(2, executable.as_bytes());
    key.field(3, query.env.as_bytes());
    key.field(4, &system.machine);
    key.field(5, &system.release);
    key.field(6, &system.version);
    key.field(7, file_digest(host, module).ok()?.as_bytes());
    key.field(8, &[query.stderr]);
    for arg in &query.args {
        key.field(9, arg);
    }
    for candidate in &query.missing_programs {
        key.field(10, candidate.as_os_str().as_encoded_bytes());
    }

    let stdout = if arm_capability && system.machine == "arm64" {
        match query.args.as_slice() {
            [flag, property] if flag == "-q" && property == "hw.optional.arm64" => {
                b"hw.optional.arm64: 1\n".to_vec()
            }
            [flag, property] if flag == "-n" && property == "hw.optional.arm64" => b"1\n".to_vec(),
            _ => return None,
        }
    } else if uname {
        // The kernel fields of the running system. A receipt is used only after
        // an equal live result, so an unexpected child context cannot match.
        // `-m` stays live, because a translated child can report another machine.
        let field = match query.args.as_slice() {
            [flag] if flag == "-s" => &system.sysname,
            [flag] if flag == "-r" => &system.release,
            _ => return None,
        };
        if field.is_empty() || field.contains(&b'\n') {
            return None;
        }
        [field.as_slice(), b"\n"].concat()
    } else if product
        && matches!(query.args.as_slice(), [flag] if flag == "-productVersion")
        && request
            .env
            .iter()
            .filter(|(name, _)| name == "SYSTEM_VERSION_COMPAT")
            .all(|(_, value)| value == "0" || value.is_empty())
    {
        let plist = host
            .read(
                Path::new("/System/Library/CoreServices/SystemVersion.plist"),
                ReadRange::All { limit: 64 * 1024 },
            )
            .ok()?;
        key.field(11, Digest::hash(&plist).as_bytes());
        let version = product_version(&plist)?;
        [version.as_bytes(), b"\n"].concat()
    } else {
        return None;
    };
    key.field(12, &stdout);
    Some((
        JournalDigest::hash(key.finish().as_bytes()),
        RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout,
            stderr: Vec::new(),
        },
    ))
}

fn product_version(plist: &[u8]) -> Option<&str> {
    let plist = std::str::from_utf8(plist).ok()?;
    let mut entries = plist.match_indices("<key>ProductVersion</key>");
    let (start, tag) = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    let (value, _) = plist
        .get(start + tag.len()..)?
        .trim_start()
        .strip_prefix("<string>")?
        .split_once("</string>")?;
    let mut fields = value.split('.');
    let major = fields.next()?;
    let minor = fields.next()?;
    let patch = fields.next();
    let digits = |field: &str| {
        !field.is_empty() && field.len() <= 4 && field.bytes().all(|byte| byte.is_ascii_digit())
    };
    (digits(major) && digits(minor) && patch.is_none_or(digits) && fields.next().is_none())
        .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn uname_receipts_equal_live_output_and_refuse_overrides()
    -> Result<(), Box<dyn std::error::Error>> {
        use rcmake_host::{CommandSpec, Input, NativeHost, Output};

        let host = NativeHost::new()?;
        let program = Path::new("/usr/bin/uname");
        let executable = file_digest(&host, program)?;
        let machine = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/usr/bin/uname".into(),
                args: vec!["-m".into()],
            }],
            cwd: "/".into(),
            env: Vec::new(),
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Null,
            timeout: None,
            tee: false,
            default_error_mode: false,
        };
        let key = super::super::query(&host, &machine).ok_or("uname -m is not admitted")?;
        assert!(verify(&host, &key, executable, program, &machine).is_none());
        for flag in ["-s", "-r"] {
            let request = |env: Vec<(rcmake_base::BString, rcmake_base::BString)>| RunRequest {
                pipeline: vec![CommandSpec {
                    program: "/usr/bin/uname".into(),
                    args: vec![flag.into()],
                }],
                cwd: "/".into(),
                env,
                stdin: Input::Inherit,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: None,
                tee: false,
                default_error_mode: false,
            };
            let plain = request(Vec::new());
            let key = super::super::query(&host, &plain).ok_or("uname is not admitted")?;
            let (_, receipt) =
                verify(&host, &key, executable, program, &plain).ok_or("no uname receipt")?;
            let live = host.run_bounded(&plain, 4096)?;
            assert_eq!(receipt.stdout, live.stdout, "{flag}");
            assert_eq!(receipt.results, live.results, "{flag}");
            for name in [
                "UNAME_r",
                "UNAME_RELEASE",
                "UNAME_m",
                "UNAME_MACHINE",
                "UNAME_s",
            ] {
                let overridden = request(vec![(name.into(), rcmake_base::BString::default())]);
                assert!(super::super::query(&host, &overridden).is_none(), "{name}");
                assert!(verify(&host, &key, executable, program, &overridden).is_none());
            }
        }
        Ok(())
    }

    #[test]
    fn uname_overrides_refuse_every_spelling_and_empty_values() {
        for name in [
            "UNAME_r",
            "UNAME_RELEASE",
            "UNAME_m",
            "UNAME_MACHINE",
            "uname_s",
            "UNAME_",
        ] {
            for value in ["", "wave2"] {
                assert!(
                    super::super::has_uname_override(&[(name.into(), value.into())]),
                    "{name}={value}"
                );
            }
        }
        assert!(!super::super::has_uname_override(&[
            ("PATH".into(), "/usr/bin".into()),
            ("UNAMEX".into(), "1".into()),
            ("XUNAME_r".into(), "1".into()),
        ]));
    }

    #[test]
    fn product_version_requires_an_unambiguous_numeric_value() {
        assert_eq!(
            product_version(b"<key>ProductVersion</key>\n<string>27.1.2</string>"),
            Some("27.1.2"),
        );
        assert_eq!(
            product_version(b"<key>ProductVersion</key><string>27.1</string>"),
            Some("27.1"),
        );
        for plist in [
            b"<key>ProductVersion</key><string>27.1 Beta</string>".as_slice(),
            b"<key>ProductVersion</key><string>27..1</string>",
            b"<key>ProductVersion</key><string>27.1</string><key>ProductVersion</key>",
        ] {
            assert!(product_version(plist).is_none());
        }
    }
}
