use rcmake_base::{BString, list, shell::quote};
use rcmake_model::{ProjectModel, Target};

use crate::{
    ResolveError,
    lower::{
        definition,
        shell::{join_words, simple_args},
    },
};

pub(super) struct LinkerDriver<'a> {
    flags: Vec<BString>,
    separator: Option<&'a BString>,
}

impl<'a> LinkerDriver<'a> {
    pub(super) fn new(model: &'a ProjectModel, target: &Target, language: &[u8]) -> Self {
        let key = |suffix: &[u8]| [b"CMAKE_", language, suffix].concat();
        let flags = definition(model, target.dir, &key(b"_LINKER_WRAPPER_FLAG"))
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
        let separator = definition(model, target.dir, &key(b"_LINKER_WRAPPER_FLAG_SEP"))
            .filter(|separator| !separator.is_empty());
        Self { flags, separator }
    }

    pub(super) fn flag_string(&self, flags: &[u8]) -> Result<BString, ResolveError> {
        let Some(args) = simple_args(flags) else {
            return Ok(flags.into());
        };
        args.iter()
            .map(|arg| {
                if arg.starts_with(b"LINKER:") {
                    self.argument(arg)
                } else {
                    Ok(quote(arg))
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(join_words)
    }

    pub(super) fn argument(&self, argument: &[u8]) -> Result<BString, ResolveError> {
        let Some(value) = argument.strip_prefix(b"LINKER:") else {
            return Ok(argument.into());
        };
        let args = if let Some(shell) = value.strip_prefix(b"SHELL:") {
            simple_args(shell).ok_or_else(|| {
                ResolveError::Message("Invalid shell arguments in LINKER:SHELL expression.".into())
            })?
        } else {
            if value.windows(6).any(|window| window == b"SHELL:") {
                return Err(ResolveError::Message(
                    "'SHELL:' prefix is not supported as part of 'LINKER:' arguments.".into(),
                ));
            }
            value
                .split(|&byte| byte == b',')
                .map(BString::from)
                .collect()
        };
        if self.flags.is_empty() {
            return Ok(join_words(args.iter().map(|arg| quote(arg))));
        }
        let args = if let Some(separator) = self.separator {
            let values: Vec<_> = args.iter().map(|arg| arg.as_slice()).collect();
            vec![BString::from(values.join(separator.as_slice()))]
        } else {
            args
        };
        let mut wrapped = Vec::new();
        for arg in args {
            let mut flags = self.flags.clone();
            if let Some(last) = flags.last_mut() {
                if last == " " {
                    *last = arg;
                } else {
                    last.extend_from_slice(&arg);
                }
            }
            wrapped.extend(flags.iter().map(|value| quote(value)));
        }
        Ok(join_words(wrapped))
    }
}
