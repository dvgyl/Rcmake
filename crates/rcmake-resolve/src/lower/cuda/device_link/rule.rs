//! Expand toolchain rule templates for the driver and registration stub.

use rcmake_base::{BString, list, path_lex::Lexical, shell::quote};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, ProjectModel, Target};

use crate::lower::shell::{command, join_words};
use crate::lower::{definition, expand_rule, launcher, object_dir};
use crate::usage::{UsageResolver, link_options::shell_arguments};
use crate::{Invocation, ResolveError};

use super::{DeviceLinkInputs, lang_definition, words};

/// Target and arguments shared by the driver and registration stub templates.
pub(super) struct Rule<'a> {
    /// Configured project.
    pub(super) model: &'a ProjectModel,
    /// Target that needs the device object.
    pub(super) target: &'a Target,
    /// Configuration that uses the device object.
    pub(super) config: &'a ConfigId,
    /// Language that supplies the device linker.
    pub(super) lang: &'a [u8],
    /// Configuration-specific object directory.
    pub(super) dir: &'a [u8],
    /// Device object path relative to the build root.
    pub(super) output: &'a BString,
    /// Compiler arguments and link inputs.
    pub(super) input: &'a DeviceLinkInputs<'a>,
    /// Link flags after adding the device IPO options.
    pub(super) link_flags: &'a [BString],
}

impl Rule<'_> {
    /// Expand the commands and run them in order, stopping at the first failure.
    ///
    /// When compiling a stub, supply the fatbinary and registration headers.
    pub(super) fn expand(
        &self,
        template: &[u8],
        stub: Option<(&BString, &BString)>,
    ) -> Result<Invocation, ResolveError> {
        let get = |suffix: &[u8]| lang_definition(self.model, self.target, self.lang, suffix);
        let language: BString = self.lang.into();
        let linker_launcher = [self.lang, b"_LINKER_LAUNCHER"].concat();
        let launcher_suffix = if stub.is_none() && self.target.props.get(&linker_launcher).is_some()
        {
            b"_LINKER_LAUNCHER".as_slice()
        } else {
            b"_COMPILER_LAUNCHER"
        };
        let launcher = launcher::Command {
            role: if stub.is_some() { "compile" } else { "link" },
            config: self.config,
            output: self.output,
            source: stub.map(|(_, register)| register),
            language: &language,
        }
        .tool_prefix(
            self.model,
            self.target,
            launcher_suffix,
            &mut UsageResolver::new(self.model),
        )?;
        let launcher = words(&launcher);
        let compiler_key = compiler_key(self.lang, b"_COMPILER");
        let arg1_key = compiler_key(self.lang, b"_COMPILER_ARG1");
        let launcher_key = compiler_key(self.lang, b"_COMPILER_LAUNCHER");
        let arg1 = get(b"_COMPILER_ARG1")
            .map_or_else(BString::default, |arg| words(&shell_arguments(arg)));
        let compiler = quote(get(b"_COMPILER").map_or(b"cc".as_slice(), |path| path.as_slice()));
        let sysroot = if stub.is_some() {
            b"CMAKE_SYSROOT_COMPILE".as_slice()
        } else {
            b"CMAKE_SYSROOT_LINK"
        };
        let definition = |key: &[u8]| definition(self.model, self.target.dir, key);
        let toolchain = join_words([
            prefixed_option(get(b"_COMPILE_OPTIONS_TARGET"), get(b"_COMPILER_TARGET")),
            prefixed_option(
                get(b"_COMPILE_OPTIONS_EXTERNAL_TOOLCHAIN"),
                get(b"_COMPILER_EXTERNAL_TOOLCHAIN"),
            ),
            prefixed_option(
                get(b"_COMPILE_OPTIONS_SYSROOT"),
                definition(sysroot).or_else(|| definition(b"CMAKE_SYSROOT")),
            ),
        ]);
        let support = object_dir(self.model, self.target, &ConfigId::default());
        let (_, extension) = Lexical::new(self.output).stem_extension(true);
        let base = self.output.strip_suffix(extension).unwrap_or(self.output);
        let link_path = words(self.input.directories);
        let libraries = join_words([link_path.clone(), words(&self.input.libraries.args)]);
        let mut shell = BString::default();
        for rule in list::iter(template, list::Empty::Drop) {
            if rule.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let compiler = join_words([
                implicit(&rule, &launcher_key, &launcher),
                compiler.clone(),
                implicit(&rule, &arg1_key, &arg1),
                toolchain.clone(),
            ]);
            let expanded = expand_rule(&rule, |key| {
                Some(match key {
                    b"TARGET" | b"OBJECT" => quote(self.output),
                    b"TARGET_BASE" => quote(base),
                    b"OBJECT_DIR" => quote(self.dir),
                    b"TARGET_SUPPORT_DIR" => quote(&support),
                    b"OBJECTS" => words(self.input.objects),
                    b"LINK_LIBRARIES" => libraries.clone(),
                    b"FLAGS" | b"LANGUAGE_COMPILE_FLAGS" => words(self.input.compile_flags),
                    b"LINK_FLAGS" => words(self.link_flags),
                    b"LINK_PATH" => link_path.clone(),
                    b"TARGET_NAME" => quote(&self.target.name),
                    b"TARGET_TYPE" => self.target.kind.as_str().into(),
                    b"CONFIG" => quote(&self.config.0),
                    b"LANGUAGE" => language.clone(),
                    b"FATBINARY" => quote(stub?.0),
                    b"REGISTER_FILE" => quote(stub?.1),
                    b"CMAKE_CURRENT_BINARY_DIR" => {
                        quote(&self.model.dir(self.target.dir).binary_dir)
                    }
                    b"CMAKE_BINARY_DIR" => quote(&self.model.dir(DirId::ROOT).binary_dir),
                    b"LINKER" => quote(get(b"_DEVICE_LINKER")?),
                    b"CMAKE_COMMAND" => {
                        quote(definition(key).map_or(b"cmake".as_slice(), |path| path.as_slice()))
                    }
                    key if key == compiler_key.as_slice() => compiler.clone(),
                    key if key == arg1_key.as_slice() => arg1.clone(),
                    key if key == launcher_key.as_slice() => launcher.clone(),
                    _ => return definition(key).cloned(),
                })
            });
            if expanded.iter().any(|byte| !byte.is_ascii_whitespace()) {
                if !shell.is_empty() {
                    shell.extend_from_slice(b" && ");
                }
                shell.extend_from_slice(&expanded);
            }
        }
        if shell.is_empty() {
            return Err(ResolveError::Message(
                "The device-link command is empty.".into(),
            ));
        }
        Ok(command(shell))
    }
}

fn compiler_key(lang: &[u8], suffix: &[u8]) -> Vec<u8> {
    [b"CMAKE_", lang, suffix].concat()
}

fn implicit(rule: &[u8], key: &[u8], value: &BString) -> BString {
    let placeholder = [b"<".as_slice(), key, b">"].concat();
    if rule
        .windows(placeholder.len())
        .any(|part| part == placeholder.as_slice())
    {
        BString::default()
    } else {
        value.clone()
    }
}

fn prefixed_option(prefix: Option<&BString>, value: Option<&BString>) -> BString {
    match (prefix, value) {
        (Some(prefix), Some(value)) if !prefix.is_empty() && !value.is_empty() => {
            [prefix.as_slice(), &quote(value)].concat().into()
        }
        _ => BString::default(),
    }
}
