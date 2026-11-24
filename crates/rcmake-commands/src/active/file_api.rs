use std::collections::BTreeMap;
use std::io::ErrorKind;

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{WriteMode, path};
use rcmake_model::DirId;

use super::values;

struct QueryKind {
    keyword: &'static str,
    query_file: &'static str,
    supported: (u32, u32),
}

static KINDS: [QueryKind; 4] = [
    QueryKind {
        keyword: "CODEMODEL",
        query_file: "codemodel-v2",
        supported: (2, 12),
    },
    QueryKind {
        keyword: "CACHE",
        query_file: "cache-v2",
        supported: (2, 0),
    },
    QueryKind {
        keyword: "CMAKEFILES",
        query_file: "cmakeFiles-v1",
        supported: (1, 1),
    },
    QueryKind {
        keyword: "TOOLCHAINS",
        query_file: "toolchains-v1",
        supported: (1, 1),
    },
];

#[derive(Clone, Copy)]
enum Keyword {
    ApiVersion,
    Kind(&'static QueryKind),
}

impl Keyword {
    fn parse(value: &[u8]) -> Option<Self> {
        if value == b"API_VERSION" {
            Some(Self::ApiVersion)
        } else {
            KINDS
                .iter()
                .find(|kind| kind.keyword.as_bytes() == value)
                .map(Self::Kind)
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ApiVersion => "API_VERSION",
            Self::Kind(kind) => kind.keyword,
        }
    }
}

pub(super) fn query(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let args = values(args);
    let Some((&operation, rest)) = args.split_first() else {
        return Err(CmdError::status("must be called with arguments."));
    };
    if operation != b"QUERY" {
        return Err(CmdError::status(format!(
            "does not recognize sub-command {}",
            BString::from(operation)
        )));
    }
    let files = query_files(cx, rest)?;
    if files.is_empty() {
        return Ok(Flow::Normal);
    }
    let root = &cx.project().dir(DirId::ROOT).binary_dir;
    let directory = path::to_native(root).join(".cmake/api/v1/query/client-rcmake");
    let host = cx.host();
    host.create_dir_all(&directory)
        .map_err(|error| CmdError::status(error.to_string()))?;
    for name in files {
        if let Err(error) = host.write(&directory.join(name), b"", WriteMode::CreateNew)
            && error.kind != ErrorKind::AlreadyExists
        {
            return Err(CmdError::status(error.to_string()));
        }
    }
    Ok(Flow::Normal)
}

fn query_files(cx: &mut Cx<'_>, mut args: &[&[u8]]) -> Result<Vec<&'static str>, CmdError> {
    let mut version = b"".as_slice();
    let mut kinds = BTreeMap::<&str, Vec<&[u8]>>::new();
    let mut missing = Vec::new();
    let mut unknown = None;
    while let Some((&argument, rest)) = args.split_first() {
        args = rest;
        let Some(keyword) = Keyword::parse(argument) else {
            unknown.get_or_insert(argument);
            continue;
        };
        let Some((&first, tail)) = args
            .split_first()
            .filter(|(next, _)| Keyword::parse(next).is_none())
        else {
            missing.push(format!(
                "Error after keyword \"{}\":\n  missing required value",
                keyword.name()
            ));
            continue;
        };
        match keyword {
            Keyword::ApiVersion => {
                version = first;
                args = tail;
            }
            Keyword::Kind(kind) => {
                let count = tail
                    .iter()
                    .take_while(|arg| Keyword::parse(arg).is_none())
                    .count();
                let (values, rest) = args.split_at(count + 1);
                kinds
                    .entry(kind.keyword)
                    .or_default()
                    .extend_from_slice(values);
                args = rest;
            }
        }
    }
    if !missing.is_empty() {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            missing.join("\n").into(),
        );
        return Err(CmdError::Reported);
    }
    if let Some(argument) = unknown {
        return Err(CmdError::status(format!(
            "QUERY given unknown argument \"{}\".",
            BString::from(argument)
        )));
    }
    if !version.iter().all(u8::is_ascii_digit) {
        return Err(CmdError::status("QUERY given non-integer API_VERSION."));
    }
    // CMake narrows the parsed version to 32 bits.
    let api_version = parse_unsigned(version).map(|number| number as u32);
    if api_version != Some(1) {
        return Err(CmdError::status(format!(
            "QUERY given unsupported API_VERSION \"{}\" (the only currently supported version is 1).",
            BString::from(version)
        )));
    }
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for kind in &KINDS {
        let Some(requested) = kinds.get(kind.keyword) else {
            continue;
        };
        match supports(kind, requested) {
            Ok(()) => files.push(kind.query_file),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(files)
    } else {
        Err(CmdError::status(format!(
            "QUERY given invalid arguments:\n  {}",
            errors.join("\n  ")
        )))
    }
}

fn supports(kind: &QueryKind, requested: &[&[u8]]) -> Result<(), String> {
    for value in requested {
        let Some((major, minor)) = object_version(value) else {
            return Err(format!(
                "Given a malformed version \"{}\" for {}.",
                BString::from(*value),
                kind.keyword
            ));
        };
        if major == kind.supported.0 && minor <= kind.supported.1 {
            return Ok(());
        }
    }
    Err(format!(
        "None of the specified {} versions is supported by this version of CMake.",
        kind.keyword
    ))
}

fn object_version(value: &[u8]) -> Option<(u32, u32)> {
    let (major, tail) = component(value)?;
    let minor = tail
        .trim_ascii_start()
        .strip_prefix(b".")
        .and_then(component)
        .map_or(0, |(number, _)| number);
    let major = u32::try_from(major).ok().filter(|&number| number > 0)?;
    Some((major, u32::try_from(minor).ok()?))
}

fn component(value: &[u8]) -> Option<(i32, &[u8])> {
    let value = value.trim_ascii_start();
    let (negative, value) = match value {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, value),
    };
    let digits = value
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let (number, tail) = value.split_at(digits);
    let number = parse_unsigned(number)? as i32;
    Some((
        if negative {
            number.wrapping_neg()
        } else {
            number
        },
        tail,
    ))
}

fn parse_unsigned(value: &[u8]) -> Option<u64> {
    core::str::from_utf8(value).ok()?.parse().ok()
}
