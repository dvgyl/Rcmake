use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{Stream, path};
use rcmake_presets::{EnvSnapshot, PresetKind};

use super::{Arguments, absolute, error};

pub(super) struct Selection {
    name: BString,
    file: Option<PathBuf>,
}

impl Selection {
    pub(super) fn append_to(self, command: &mut Vec<BString>) {
        command.extend(["--preset".into(), self.name]);
        if let Some(file) = self.file {
            command.extend(["--presets-file".into(), path::from_native(&file)]);
        }
    }
}

pub(super) fn select(
    cx: &Cx<'_>,
    args: &Arguments<'_>,
    source: &Path,
    kind: PresetKind,
) -> Result<Option<Selection>, CmdError> {
    let key = if kind == PresetKind::Configure {
        b"CTEST_CONFIGURE_PRESET".as_slice()
    } else {
        b"CTEST_BUILD_PRESET".as_slice()
    };
    let category = kind.label();
    let option = |argument: &[u8], variable: &[u8]| {
        args.value(argument)
            .cloned()
            .filter(|value| !value.is_empty())
            .or_else(|| cx.var(variable).filter(|value| !value.is_empty()))
    };
    let specific = option(b"PRESET", key);
    let Some(name) = specific
        .clone()
        .or_else(|| cx.var(b"CTEST_PRESET").filter(|name| !name.is_empty()))
    else {
        return Ok(None);
    };
    let file = option(b"PRESETS_FILE", b"CTEST_PRESETS_FILE").map(|file| absolute(source, &file));
    let host = cx.host();
    let presets = match &file {
        Some(file) => rcmake_presets::load_file(source, file, host),
        None => rcmake_presets::load(source, host),
    };
    let resolved =
        presets.and_then(|presets| presets.resolve(kind, &name, &EnvSnapshot::from_host(host)));
    match resolved {
        Ok(_) => Ok(Some(Selection { name, file })),
        Err(failure) if specific.is_some() => Err(error(failure.message)),
        Err(_) => {
            host.write_out(
                Stream::Stderr,
                format!("No {category} preset named \"{name}\" found, ignoring CTEST_PRESET.\n")
                    .as_bytes(),
            );
            Ok(None)
        }
    }
}
