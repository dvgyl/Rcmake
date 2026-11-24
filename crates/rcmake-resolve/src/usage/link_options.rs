use rcmake_base::{BString, list, shell::quote_always};
use rcmake_diag::{Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{EvalContext, UsageTraversal};
use rcmake_model::{BT, ProjectModel, Target, TargetId, TargetKind};

use super::{UsageResolver, UsageValue};
use crate::{ResolveError, lower::definition};

impl UsageResolver<'_> {
    pub(super) fn archive_options(
        &mut self,
        id: TargetId,
        ctx: &EvalContext,
    ) -> Result<UsageValue, ResolveError> {
        let target = self.target(id)?;
        let property = b"STATIC_LIBRARY_OPTIONS";
        let mut value = UsageValue::default();
        if let Some(raw) = target.props.get(property) {
            let entry = BT::new(raw.clone(), target.backtrace.clone());
            let ctx = self.property_context(id, property, ctx)?;
            self.eval_entries(id, property, [&entry], &ctx, &mut value)?;
        }
        Ok(value)
    }

    pub(super) fn prepare_link_options(
        &self,
        id: TargetId,
        ctx: &EvalContext,
        value: &mut UsageValue,
    ) -> Result<(), ResolveError> {
        let target = self.target(id)?;
        let prefix = if target.kind == TargetKind::StaticLibrary {
            b"ARCHIVER".as_slice()
        } else {
            b"LINKER"
        };
        let wrapper = Wrapper::new(
            self.model,
            target,
            ctx.link_language
                .as_ref()
                .map_or(b"", |value| value.as_slice()),
            prefix,
        );
        let mut result = Vec::new();
        for entry in std::mem::take(&mut value.entries) {
            let args = entry
                .value
                .strip_prefix(b"SHELL:")
                .map_or_else(|| vec![entry.value.clone()], shell_arguments);
            for arg in args {
                let args = wrapper.expand(&arg).map_err(|error| {
                    if let Some(paths) = self.source_paths() {
                        let body = BString::from(error.to_string());
                        paths.emit(
                            self.model,
                            MessageKind::Error,
                            Location::Backtrace(target.backtrace.clone()),
                            body.clone(),
                        );
                        ResolveError::Reported(body)
                    } else {
                        error
                    }
                })?;
                result.extend(args.into_iter().map(|arg| {
                    // Preserve literal prefixes through link fragment lowering.
                    let arg = if arg.starts_with(b"LINKER:") || arg.starts_with(b"SHELL:") {
                        [b"SHELL:".as_slice(), &quote_always(&arg)].concat().into()
                    } else {
                        arg
                    };
                    BT::new(arg, entry.backtrace.clone())
                }));
            }
        }
        value.entries = result;
        Ok(())
    }
}

pub(super) fn traversal(target: &Target) -> UsageTraversal {
    if target.policies.policies.get(PolicyId::new(99)) == PolicyStatus::New {
        UsageTraversal::Link
    } else {
        UsageTraversal::Compile
    }
}

pub(crate) struct Wrapper<'a> {
    prefix: &'a [u8],
    flags: Vec<BString>,
    separator: Option<&'a BString>,
}

impl<'a> Wrapper<'a> {
    pub(crate) fn new(
        model: &'a ProjectModel,
        target: &Target,
        language: &[u8],
        prefix: &'a [u8],
    ) -> Self {
        let key = |suffix: &[u8]| [b"CMAKE_", language, b"_", prefix, suffix].concat();
        let flags = definition(model, target.dir, &key(b"_WRAPPER_FLAG"))
            .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
        let separator = definition(model, target.dir, &key(b"_WRAPPER_FLAG_SEP"))
            .filter(|value| !value.is_empty());
        Self {
            prefix,
            flags,
            separator,
        }
    }

    pub(crate) fn expand(&self, option: &[u8]) -> Result<Vec<BString>, ResolveError> {
        let Some(payload) = option
            .strip_prefix(self.prefix)
            .and_then(|rest| rest.strip_prefix(b":"))
        else {
            return Ok(vec![option.into()]);
        };
        let mut args = if let Some(shell) = payload.strip_prefix(b"SHELL:") {
            shell_arguments(shell)
        } else {
            payload
                .split(|&byte| byte == b',')
                .filter(|arg| !arg.is_empty())
                .map(BString::from)
                .collect()
        };
        if args
            .iter()
            .any(|arg| arg.windows(6).any(|bytes| bytes == b"SHELL:"))
        {
            return Err(ResolveError::Message(
                format!(
                    "'SHELL:' prefix is not supported as part of '{}:' arguments.",
                    BString::from(self.prefix)
                )
                .into(),
            ));
        }
        let Some((last, prefix_flags)) = self.flags.split_last() else {
            return Ok(args);
        };
        if let Some(separator) = self.separator
            && !args.is_empty()
        {
            let parts: Vec<_> = args.iter().map(|value| value.as_slice()).collect();
            args = vec![parts.join(separator.as_slice()).into()];
        }
        let mut wrapped = Vec::new();
        for arg in args {
            wrapped.extend(prefix_flags.iter().cloned());
            wrapped.push(if last == " " {
                arg
            } else {
                [last.as_slice(), &arg].concat().into()
            });
        }
        Ok(wrapped)
    }
}

fn shell_arguments(input: &[u8]) -> Vec<BString> {
    let mut args = Vec::new();
    let mut word = BString::default();
    let mut quote = None;
    let mut started = false;
    let mut bytes = input.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                word.extend(bytes.next());
                started = true;
            }
            b'\'' | b'"' if quote.is_none() || quote == Some(byte) => {
                quote = if quote.is_none() { Some(byte) } else { None };
                started = true;
            }
            byte if quote.is_none() && byte.is_ascii_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            byte => {
                word.push(byte);
                started = true;
            }
        }
    }
    if started {
        args.push(word);
    }
    args
}
