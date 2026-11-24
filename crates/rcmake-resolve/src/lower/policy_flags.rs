use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, Stream};
use rcmake_model::{ProjectModel, Target, TargetKind};

use super::{definition, target_property};

pub(super) struct Flags<'a> {
    model: &'a ProjectModel,
    target: &'a Target,
    language: &'a [u8],
}

const PIC: &[u8] = b"POSITION_INDEPENDENT_CODE";

impl<'a> Flags<'a> {
    pub(super) fn new(model: &'a ProjectModel, target: &'a Target, language: &'a [u8]) -> Self {
        Self {
            model,
            target,
            language,
        }
    }

    pub(super) fn option(&self, suffix: &[u8]) -> Option<&'a BString> {
        definition(
            self.model,
            self.target.dir,
            &[b"CMAKE_", self.language, b"_", suffix].concat(),
        )
    }

    pub(super) fn ipo(&self, config: &ConfigId) -> bool {
        let property = b"INTERPROCEDURAL_OPTIMIZATION";
        let specific = [property.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
        self.target.policies.policies.get(PolicyId::new(69)) == PolicyStatus::New
            && (!config.0.is_empty())
                .then(|| self.target.props.get(&specific))
                .flatten()
                .or_else(|| self.target.props.get(property))
                .is_some_and(|value| truth::is_on(value))
    }

    pub(super) fn compile(&self, ipo: bool) -> Vec<BString> {
        let mut flags = Vec::new();
        if ipo {
            self.extend(&mut flags, b"COMPILE_OPTIONS_IPO");
        }
        let pic = target_property(self.model, self.target, PIC).map_or(
            matches!(
                self.target.kind,
                TargetKind::SharedLibrary | TargetKind::ModuleLibrary
            ),
            |value| truth::is_on(value),
        );
        if pic {
            if self.target.kind == TargetKind::Executable
                && self
                    .option(b"COMPILE_OPTIONS_PIE")
                    .is_some_and(|value| !value.is_empty())
            {
                self.extend(&mut flags, b"COMPILE_OPTIONS_PIE");
            } else if self.option(b"COMPILE_OPTIONS_PIC").is_some() {
                self.extend(&mut flags, b"COMPILE_OPTIONS_PIC");
            } else if self.option(b"COMPILER_ID").is_none() {
                flags.push(if self.target.kind == TargetKind::Executable {
                    "-fPIE".into()
                } else {
                    "-fPIC".into()
                });
            }
        }
        if self
            .target
            .props
            .get(b"COMPILE_WARNING_AS_ERROR")
            .is_some_and(|value| truth::is_on(value))
            && !definition(self.model, self.target.dir, b"_RCMAKE_NO_WARNING_AS_ERROR")
                .is_some_and(|value| truth::is_on(value))
        {
            self.extend(&mut flags, b"COMPILE_OPTIONS_WARNING_AS_ERROR");
        }
        flags
    }

    pub(super) fn link(&self, ipo: bool) -> Vec<BString> {
        let mut flags = Vec::new();
        if ipo {
            self.extend(&mut flags, b"COMPILE_OPTIONS_IPO");
            self.extend(&mut flags, b"LINK_OPTIONS_IPO");
        }
        flags
    }

    pub(super) fn pie_link(&self) -> Vec<BString> {
        if self.target.kind != TargetKind::Executable
            || self.target.policies.policies.get(PolicyId::new(83)) != PolicyStatus::New
        {
            return Vec::new();
        }
        let Some(pic) = target_property(self.model, self.target, PIC) else {
            return Vec::new();
        };
        let (supported, options): (&[u8], &[u8]) = if truth::is_on(pic) {
            (b"LINK_PIE_SUPPORTED", b"LINK_OPTIONS_PIE")
        } else {
            (b"LINK_NO_PIE_SUPPORTED", b"LINK_OPTIONS_NO_PIE")
        };
        let mut flags = Vec::new();
        if self
            .option(supported)
            .is_some_and(|value| truth::is_on(value))
        {
            self.extend(&mut flags, options);
        }
        flags
    }

    fn extend(&self, flags: &mut Vec<BString>, option: &[u8]) {
        if let Some(value) = self.option(option) {
            flags.extend(
                list::split(value, list::Empty::Drop)
                    .iter()
                    .map(|arg| quote(arg)),
            );
        }
    }
}

pub(crate) fn warn_about_wrappers(model: &ProjectModel, host: &dyn Host) {
    const SUFFIXES: &[&[u8]] = &[
        b"CLANG_TIDY",
        b"INCLUDE_WHAT_YOU_USE",
        b"CPPLINT",
        b"CPPCHECK",
    ];
    for (_, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        for name in target.props.names() {
            if (name == b"LINK_WHAT_YOU_USE".as_slice()
                || name
                    .splitn(2, |&byte| byte == b'_')
                    .nth(1)
                    .is_some_and(|suffix| SUFFIXES.contains(&suffix)))
                && target
                    .props
                    .get(&name)
                    .is_some_and(|value| !value.is_empty())
            {
                host.write_out(Stream::Stderr, format!(
                    "Rcmake Warning: target property {name} on target '{}' is not implemented yet\n", target.name
                ).as_bytes());
            }
        }
    }
}
