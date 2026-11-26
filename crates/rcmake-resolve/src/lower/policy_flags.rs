use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target, TargetKind};

use super::{definition, target_property};
use crate::ResolveError;

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

    pub(super) fn compile(&self, ipo: bool, position_independent: Option<bool>) -> Vec<BString> {
        let mut flags = Vec::new();
        if ipo {
            self.extend(&mut flags, b"COMPILE_OPTIONS_IPO");
        }
        let pic = position_independent
            .or_else(|| self.position_independent())
            .unwrap_or(matches!(
                self.target.kind,
                TargetKind::SharedLibrary | TargetKind::ModuleLibrary,
            ));
        if pic {
            if self.target.kind == TargetKind::Executable
                && self
                    .option(b"COMPILE_OPTIONS_PIE")
                    .is_some_and(|value| !value.is_empty())
            {
                self.extend(&mut flags, b"COMPILE_OPTIONS_PIE");
            } else if self.option(b"COMPILE_OPTIONS_PIC").is_some() {
                self.extend(&mut flags, b"COMPILE_OPTIONS_PIC");
            } else if self.option(b"COMPILER_ID").is_none()
                && !matches!(self.language, b"RC" | b"ASM_MASM" | b"ASM_NASM")
                && !super::artifacts::is_dll_platform(self.model, self.target.dir)
            {
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

    pub(super) fn append_color(&self, flags: &mut Vec<BString>) {
        if let Some(color) = definition(self.model, self.target.dir, b"CMAKE_COLOR_DIAGNOSTICS")
            .filter(|value| {
                !value.is_empty()
                    && !value.ends_with(b"-NOTFOUND")
                    && value.as_slice() != b"NOTFOUND"
            })
        {
            let enabled = truth::is_on(color);
            let key = if enabled {
                b"COMPILE_OPTIONS_COLOR_DIAGNOSTICS".as_slice()
            } else {
                b"COMPILE_OPTIONS_COLOR_DIAGNOSTICS_OFF"
            };
            if self.option(key).is_some() {
                self.extend(flags, key);
            } else if !enabled
                && matches!(
                    self.option(b"COMPILER_ID").map(|id| id.as_slice()),
                    Some(b"Clang" | b"AppleClang")
                )
            {
                flags.push("-fno-color-diagnostics".into());
            }
        }
    }

    pub(super) fn link(&self, ipo: bool) -> Vec<BString> {
        let mut flags = Vec::new();
        if ipo {
            self.extend(&mut flags, b"COMPILE_OPTIONS_IPO");
            self.extend(&mut flags, b"LINK_OPTIONS_IPO");
        }
        flags
    }

    pub(super) fn validate_visibility(&self) -> Result<(), ResolveError> {
        if self.option(b"COMPILE_OPTIONS_VISIBILITY").is_none() {
            return Ok(());
        }
        let key = [self.language, b"_VISIBILITY_PRESET"].concat();
        let Some(value) = self.target.props.get(&key) else {
            return Ok(());
        };
        if matches!(
            value.as_slice(),
            b"default" | b"hidden" | b"protected" | b"internal"
        ) {
            return Ok(());
        }
        Err(ResolveError::Message(format!(
            "CMake Error: Target {} uses unsupported value \"{value}\" for {}. The supported values are: default, hidden, protected, and internal.",
            self.target.name,
            BString::from(key)
        ).into()))
    }

    pub(super) fn pie_link(&self, position_independent: Option<bool>) -> Vec<BString> {
        if self.target.kind != TargetKind::Executable
            || self.target.policies.policies.get(PolicyId::new(83)) != PolicyStatus::New
        {
            return Vec::new();
        }
        let Some(pic) = position_independent.or_else(|| self.position_independent()) else {
            return Vec::new();
        };
        let (supported, options): (&[u8], &[u8]) = if pic {
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

    fn position_independent(&self) -> Option<bool> {
        target_property(self.model, self.target, PIC).map(|value| truth::is_on(value))
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

/// Return the toolchain flag for an enabled unused-library check.
pub(super) fn link_what_you_use<'a>(
    model: &'a ProjectModel,
    target: &Target,
    language: &[u8],
) -> Option<&'a BString> {
    if !matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) || !target
        .props
        .get(b"LINK_WHAT_YOU_USE")
        .is_some_and(|value| truth::is_on(value))
        || !model
            .generator
            .is_some_and(rcmake_model::GeneratorKind::is_make_or_ninja)
    {
        return None;
    }
    let key = [b"CMAKE_".as_slice(), language, b"_LINK_WHAT_YOU_USE_FLAG"].concat();
    definition(model, target.dir, &key).filter(|value| !value.is_empty())
}
