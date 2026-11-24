use rcmake_base::{BString, shell::quote};
use rcmake_model::{ProjectModel, Target};

use crate::{
    ResolveError,
    lower::shell::{join_words, simple_args},
    usage::link_options::{Wrapper, shell_arguments},
};

pub(super) struct LinkerDriver<'a>(Wrapper<'a>);

impl<'a> LinkerDriver<'a> {
    pub(super) fn new(model: &'a ProjectModel, target: &Target, language: &[u8]) -> Self {
        Self(Wrapper::new(model, target, language, b"LINKER"))
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
        if let Some(payload) = argument.strip_prefix(b"SHELL:") {
            let mut args = Vec::new();
            for word in shell_arguments(payload) {
                args.extend(self.0.expand(&word)?);
            }
            return Ok(join_words(args.iter().map(|arg| quote(arg))));
        }
        if !argument.starts_with(b"LINKER:") {
            return Ok(argument.into());
        }
        self.0
            .expand(argument)
            .map(|args| join_words(args.iter().map(|arg| quote(arg))))
    }
}
