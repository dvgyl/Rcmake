use rcmake_base::BString;
use rcmake_host::{Host, Stream};

use super::{Argument, Options, generator_error, is_generator};

const EXTRA_GENERATOR_BASES: &[(&[u8], &[&[u8]])] = &[
    (
        b"CodeBlocks",
        &[
            b"MinGW Makefiles",
            b"NMake Makefiles",
            b"NMake Makefiles JOM",
            b"Ninja",
            b"Unix Makefiles",
        ],
    ),
    (
        b"CodeLite",
        &[
            b"MinGW Makefiles",
            b"NMake Makefiles",
            b"Ninja",
            b"Unix Makefiles",
        ],
    ),
    (
        b"Eclipse CDT4",
        &[
            b"MinGW Makefiles",
            b"NMake Makefiles",
            b"Ninja",
            b"Unix Makefiles",
        ],
    ),
    (
        b"Kate",
        &[
            b"MinGW Makefiles",
            b"NMake Makefiles",
            b"Ninja",
            b"Ninja Multi-Config",
            b"Unix Makefiles",
        ],
    ),
    (
        b"Sublime Text 2",
        &[
            b"MinGW Makefiles",
            b"NMake Makefiles",
            b"Ninja",
            b"Unix Makefiles",
        ],
    ),
];

fn split_extra_generator(name: &[u8]) -> Option<(&[u8], &[u8])> {
    let sep = b" - ";
    let idx = name.windows(sep.len()).position(|w| w == sep)?;
    let extra = name.get(..idx)?;
    let base = name.get(idx + sep.len()..)?;
    let (_, allowed_bases) = EXTRA_GENERATOR_BASES
        .iter()
        .find(|&&(known, _)| known == extra)?;
    (allowed_bases.contains(&base) && is_generator(base)).then_some((extra, base))
}

fn warn_extra_generator(host: &dyn Host, extra: &[u8]) {
    let extra = String::from_utf8_lossy(extra);
    host.write_out(
        Stream::Stderr,
        format!(
            "CMake Warning:\n  Support for \"Extra Generators\" like\n\n    {extra}\n\n  is deprecated and will be removed from a future version of CMake.  IDEs may\n  use the cmake-file-api(7) to view CMake-generated project build trees.\n\n\n"
        )
        .as_bytes(),
    );
}

pub(super) fn parse(
    arg: &mut Argument<'_, '_>,
    opts: &mut Options,
    host: &dyn Host,
) -> Result<bool, String> {
    match arg.flag {
        b"-G" => {
            let maybe = arg.value().filter(|v| !v.is_empty());
            if let Some(name) = maybe {
                if is_generator(name) {
                    opts.generator = Some(BString::from(name));
                } else if let Some((extra, base)) = split_extra_generator(name) {
                    warn_extra_generator(host, extra);
                    opts.generator = Some(BString::from(base));
                } else {
                    generator_error(host, Some(name));
                    return Err(String::new());
                }
            } else {
                generator_error(host, None);
                return Err(String::new());
            }
        }
        b"-A" | b"-T" => {
            let (slot, missing) = if arg.flag == b"-A" {
                (&mut opts.architecture, "No platform specified for -A")
            } else {
                (&mut opts.toolset, "No toolset specified for -T")
            };
            let value = arg.value().ok_or(missing)?;
            if slot.as_ref().is_some_and(|previous| previous != value) {
                let flag = BString::from(arg.flag);
                return Err(format!("Multiple {flag} options not allowed"));
            }
            *slot = Some(value.into());
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_host::{PureHost, Stream};

    use crate::options::{ParseResult, parse};
    use crate::persona::Persona;

    #[test]
    fn extra_generators_warn_and_select_base_generator() {
        for (extra, base) in [
            ("CodeBlocks", "Ninja"),
            ("CodeLite", "Ninja"),
            ("Eclipse CDT4", "Ninja"),
            ("Kate", "Ninja"),
            ("Kate", "Ninja Multi-Config"),
            ("Sublime Text 2", "Ninja"),
        ] {
            let host = PureHost::default();
            let spec = format!("{extra} - {base}");
            let args = [BString::from("-G"), BString::from(spec)];
            let ParseResult::Run(opts) = parse(&args, Persona::Compat, &host) else {
                panic!("expected extra generator {extra} - {base} to succeed");
            };
            assert_eq!(
                opts.generator.as_ref().map(|g| g.as_slice()),
                Some(base.as_bytes())
            );
            let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
            assert_eq!(
                stderr,
                format!(
                    "CMake Warning:\n  Support for \"Extra Generators\" like\n\n    {extra}\n\n  is deprecated and will be removed from a future version of CMake.  IDEs may\n  use the cmake-file-api(7) to view CMake-generated project build trees.\n\n\n"
                )
            );
        }
    }

    #[test]
    fn invalid_extra_generator_pairs_are_rejected() {
        for invalid in [
            "BogusIDE - Ninja",
            "CodeBlocks - Ninja Multi-Config",
            "CodeLite - NMake Makefiles JOM",
            "Sublime Text 2 - MSYS Makefiles",
        ] {
            let host = PureHost::default();
            let args = [BString::from("-G"), BString::from(invalid)];
            assert!(
                matches!(parse(&args, Persona::Compat, &host), ParseResult::Exit(1)),
                "expected {invalid} to be rejected"
            );
        }
    }
}
