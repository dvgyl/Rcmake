use rcmake_base::BString;
use rcmake_eval::CacheDefine;
use rcmake_host::{Host, Stream};
use rcmake_model::CacheType;

use crate::parse_define;

use super::{Argument, CacheEdit, Options, cli_usage_error_only};

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"--toolchain" | b"--install-prefix" => {
            let (name, message) = if arg.flag == b"--toolchain" {
                ("CMAKE_TOOLCHAIN_FILE", "No file specified for --toolchain")
            } else {
                (
                    "CMAKE_INSTALL_PREFIX",
                    "No install directory specified for --install-prefix",
                )
            };
            opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                name: name.into(),
                ty: Some(if arg.flag == b"--toolchain" {
                    CacheType::Filepath
                } else {
                    CacheType::Path
                }),
                value: arg.required(message)?.into(),
            }));
        }
        b"-C" => {
            let path = if let Some(tail) = arg.inline {
                tail.strip_prefix(b"=").unwrap_or(tail).trim_ascii_start()
            } else {
                arg.value().ok_or("-C must be followed by a file name.")?
            };
            if path.is_empty() {
                return Err("No file name specified for -C".into());
            }
            opts.cache_edits
                .push(CacheEdit::Preload(rcmake_host::path::to_native(path)));
        }
        b"-D" => {
            let raw = arg.value().ok_or("-D must be followed with VAR=VALUE.")?;
            let Some(mut def) = parse_define(raw) else {
                host.write_out(Stream::Stderr, format!("CMake Error: Parse error in command line argument: {}\n Should be: VAR:type=value\n\n", String::from_utf8_lossy(raw)).as_bytes());
                return Err(cli_usage_error_only(host));
            };
            if matches!(def.ty, Some(CacheType::Path | CacheType::Filepath))
                && let Some(home) = host.env_get(b"HOME")
            {
                def.value = expand_home(&def.value, &home);
            }
            opts.cache_edits.push(CacheEdit::Define(def));
        }
        b"-U" => {
            opts.cache_edits.push(CacheEdit::Remove(
                arg.value().ok_or("-U must be followed with VAR.")?.into(),
            ));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Replace a leading `~` in each `;`-separated element of a `PATH` or
/// `FILEPATH` value, as `cmSystemTools::ConvertToUnixSlashes` does when
/// CMake stores the entry. Only `~` alone or `~/...` changes.
fn expand_home(value: &[u8], home: &[u8]) -> BString {
    let mut out = BString::default();
    for (index, element) in value.split(|byte| *byte == b';').enumerate() {
        if index > 0 {
            out.push(b';');
        }
        match element.strip_prefix(b"~") {
            Some(rest) if rest.is_empty() || rest.starts_with(b"/") => {
                out.extend_from_slice(home);
                out.extend_from_slice(rest);
            }
            _ => out.extend_from_slice(element),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_host::{Host, PureHost};

    use super::expand_home;
    use crate::options::{CacheEdit, ParseResult, parse};
    use crate::persona::Persona;

    #[test]
    fn tilde_expands_only_for_path_definitions() {
        let host = PureHost::default();
        host.env_set(b"HOME", Some(b"/hh"));
        let args = [
            "-DA=~/q",
            "-DB:PATH=~/q",
            "-DC:FILEPATH=~",
            "-DL:PATH=~/a;~/b",
            "-DS:STRING=~/s",
            "-P",
            "script.cmake",
        ]
        .map(BString::from);
        let ParseResult::Run(opts) = parse(&args, Persona::Compat, &host) else {
            panic!("the definitions must parse");
        };
        let values: Vec<_> = opts
            .cache_edits
            .iter()
            .filter_map(|edit| match edit {
                CacheEdit::Define(def) => Some((def.name.to_string(), def.value.to_string())),
                _ => None,
            })
            .collect();
        let expected = [
            ("A", "~/q"),
            ("B", "/hh/q"),
            ("C", "/hh"),
            ("L", "/hh/a;/hh/b"),
            ("S", "~/s"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        assert_eq!(values, expected);
    }

    #[test]
    fn home_expands_in_each_path_element() {
        let expand = |value: &str| expand_home(value.as_bytes(), b"/hh");
        assert_eq!(expand("~/q").as_slice(), b"/hh/q");
        assert_eq!(expand("~").as_slice(), b"/hh");
        assert_eq!(expand("~/a;~/b;c").as_slice(), b"/hh/a;/hh/b;c");
        assert_eq!(expand("~x").as_slice(), b"~x");
        assert_eq!(expand("a/~/b").as_slice(), b"a/~/b");
    }
}
