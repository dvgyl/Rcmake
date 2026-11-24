use rcmake_base::BString;

/// The parts of a supported ExternalProject Git clone command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitClone<'a> {
    /// Clone source.
    pub repository: &'a str,
    /// Destination, relative to the command working directory if needed.
    pub directory: &'a str,
    /// Name of the Git remote, usually `origin`.
    pub origin: &'a str,
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(crate) options: &'a [BString],
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(crate) global: &'a [BString],
}

/// Recognize the clone arguments emitted by the ExternalProject clone template.
///
/// Exclude options that can change the transport or clone layout. Call this on
/// commands from the template. A matching argument list alone does not prove origin.
pub fn classify_git_clone(args: &[BString]) -> Option<GitClone<'_>> {
    let (command, _global) = split_config(args)?;
    let [clone, rest @ ..] = command else {
        return None;
    };
    if clone != "clone" {
        return None;
    }
    let [options @ .., repository, directory] = rest else {
        return None;
    };
    let (repository, directory) = (plain(repository)?, plain(directory)?);
    let mut flags = options;
    let mut origin = "origin";
    while let Some((flag, rest)) = flags.split_first() {
        flags = match flag.as_slice() {
            b"--no-checkout" | b"--progress" | b"--no-single-branch" | b"--quiet" => rest,
            b"--depth" => match rest {
                [value, tail @ ..] if value == "1" => tail,
                _ => return None,
            },
            b"--origin" => {
                let (value, tail) = rest.split_first()?;
                origin = plain(value)?;
                tail
            }
            b"-c" | b"--config" => {
                let (value, tail) = rest.split_first()?;
                if !allowed_config(value) {
                    return None;
                }
                tail
            }
            _ => return None,
        };
    }
    Some(GitClone {
        repository,
        directory,
        origin,
        #[cfg(all(feature = "native", any(unix, windows)))]
        options,
        #[cfg(all(feature = "native", any(unix, windows)))]
        global: _global,
    })
}

/// Recognize the clone template's `checkout tag [--]` arguments.
///
/// A path checkout, unknown option, or different argument shape returns `None`.
pub fn classify_git_checkout(args: &[BString]) -> Option<&str> {
    match args {
        [command, tag] if command == "checkout" => plain(tag),
        [command, tag, end] if command == "checkout" && end == "--" => plain(tag),
        _ => None,
    }
}

fn split_config(mut args: &[BString]) -> Option<(&[BString], &[BString])> {
    let original = args;
    while let [flag, value, rest @ ..] = args {
        if flag != "-c" {
            break;
        }
        if !allowed_config(value) {
            return None;
        }
        args = rest;
    }
    Some((args, original.get(..original.len() - args.len())?))
}

fn allowed_config(config: &[u8]) -> bool {
    let Ok(config) = std::str::from_utf8(config) else {
        return false;
    };
    let Some((key, value)) = config.split_once('=') else {
        return false;
    };
    match key.to_ascii_lowercase().as_str() {
        "http.sslverify" => matches!(value, "true" | "false"),
        "http.sslversion" => matches!(
            value,
            "tlsv1" | "tlsv1.0" | "tlsv1.1" | "tlsv1.2" | "tlsv1.3"
        ),
        key if key.starts_with("advice.") => matches!(value, "true" | "false"),
        _ => false,
    }
}

pub(crate) fn plain(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?;
    (!value.is_empty() && !value.starts_with('-') && !value.chars().any(char::is_control))
        .then_some(value)
}
