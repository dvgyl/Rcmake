//! Target-level `ClCompile` settings that CMake computes from the language
//! flags and the target properties.

use rcmake_base::{list, truth};
use rcmake_model::{Target, TargetKind};
use rcmake_resolve::usage::UsageResolver;

use super::{Context, gen_error};
use crate::{GenError, flags_table, string};

/// Facts that the target-level compile settings give to other parts of the
/// project file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClFacts {
    /// `CharacterSet` of the configuration from `_UNICODE`, `_MBCS`, or `_SBCS`.
    pub(crate) char_set: Option<&'static str>,
    /// `SpectreMitigation` of the configuration, from `/Qspectre` flags.
    pub(crate) spectre: Option<String>,
    /// The target compiles C++/CLI code (`/clr` flags).
    pub(crate) managed: bool,
    /// The target-level settings compile as Windows Runtime (`/ZW`).
    pub(crate) winrt: bool,
}

impl<'a> Context<'a> {
    /// Compute the flags that CMake adds with the language flags of
    /// `language`, but that the target settings of the resolver do not have.
    ///
    /// These are the MSVC runtime library flags (policy `CMP0091`) and the
    /// Just My Code flags (`VS_JUST_MY_CODE_DEBUGGING`).
    pub(super) fn language_extras(
        &self,
        target: &Target,
        language: &[u8],
        config: &str,
        usage: &mut Option<UsageResolver<'a>>,
    ) -> Result<Vec<String>, GenError> {
        let Some(id) = self.project.target else {
            return Ok(Vec::new());
        };
        let mut flags = Vec::new();
        let option = |suffix: &[u8]| {
            self.model
                .definition(target.dir, &[b"CMAKE_", language, suffix].concat())
                .map(|value| {
                    list::split(value, list::Empty::Drop)
                        .iter()
                        .map(|flag| string(flag))
                        .collect::<Vec<_>>()
                })
        };
        if let Some(default) = self
            .definition(b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
            .filter(|value| !value.is_empty())
        {
            let runtime = usage
                .get_or_insert_with(|| UsageResolver::new(self.model))
                .target_toolchain_property(
                    id,
                    config.as_bytes(),
                    language,
                    b"MSVC_RUNTIME_LIBRARY",
                    Some(default),
                )
                .map_err(gen_error)?
                .unwrap_or_default();
            if !runtime.is_empty()
                && let Some(options) = option(
                    &[
                        b"_COMPILE_OPTIONS_MSVC_RUNTIME_LIBRARY_",
                        runtime.as_slice(),
                    ]
                    .concat(),
                )
            {
                flags.extend(options);
            }
        }
        if !managed(target)
            && let Some(options) = option(b"_COMPILE_OPTIONS_JMC")
            && target.props.get(b"VS_JUST_MY_CODE_DEBUGGING").is_some()
        {
            let enabled = usage
                .get_or_insert_with(|| UsageResolver::new(self.model))
                .target_toolchain_property(
                    id,
                    config.as_bytes(),
                    language,
                    b"VS_JUST_MY_CODE_DEBUGGING",
                    None,
                )
                .map_err(gen_error)?
                .is_some_and(|value| truth::is_on(&value));
            if enabled {
                flags.extend(options);
            }
        }
        Ok(flags)
    }

    /// Finish the target-level compile settings as CMake does after it parses
    /// the flags, and return the facts for the other project file parts.
    pub(super) fn finish_cl(
        &self,
        target: &Target,
        settings: &mut flags_table::Settings,
    ) -> ClFacts {
        let mut facts = ClFacts::default();
        let flag = |key: &[u8]| {
            self.model
                .definition(target.dir, key)
                .is_some_and(|value| truth::is_on(value))
        };
        if target
            .props
            .get(b"VS_WINRT_COMPONENT")
            .is_some_and(|value| truth::is_on(value))
        {
            settings.set("CompileAsWinRT", "true");
            if matches!(
                target.kind,
                TargetKind::SharedLibrary | TargetKind::ModuleLibrary
            ) {
                settings.list("PreprocessorDefinitions", "_WINRT_DLL");
            }
        } else if (self
            .definition(b"CMAKE_SYSTEM_NAME")
            .is_some_and(|name| matches!(name.as_slice(), b"WindowsStore" | b"WindowsPhone"))
            || flag(b"CMAKE_VS_WINRT_BY_DEFAULT"))
            && !settings.has("CompileAsWinRT")
        {
            settings.set("CompileAsWinRT", "false");
        }
        facts.winrt = settings
            .get("CompileAsWinRT")
            .is_some_and(|value| truth::is_on(value.as_bytes()));
        facts.char_set = settings
            .list_values("PreprocessorDefinitions")
            .iter()
            .find_map(|define| char_set(define));
        if settings.has("CompileAsManaged") || settings.has("CLRSupport") {
            facts.managed = true;
            if matches!(settings.get("CompileAsManaged"), Some("Safe" | "Pure")) {
                settings.set("CallingConvention", "");
            }
            // The default values of these settings do not work for managed
            // code.
            settings.set("ExceptionHandling", "Async");
            settings.set("BasicRuntimeChecks", "Default");
        }
        facts.spectre = settings.take("SpectreMitigation");
        facts
    }
}

/// The `/clr` flag of `COMMON_LANGUAGE_RUNTIME`, when the property is set.
///
/// An empty value gives `/clr`, and a value `v` gives `/clr:v`.
pub(super) fn clr_flag(target: &Target) -> Option<String> {
    let value = target.props.get(b"COMMON_LANGUAGE_RUNTIME")?;
    Some(if value.is_empty() {
        "/clr".into()
    } else {
        format!("/clr:{}", string(value))
    })
}

/// Tell if CMake treats the target as managed code only, which turns off the
/// Just My Code flags.
///
/// A `COMMON_LANGUAGE_RUNTIME` value other than empty or `netcore` makes an
/// executable or a shared library managed. The other values mix managed and
/// native code.
fn managed(target: &Target) -> bool {
    matches!(
        target.kind,
        TargetKind::Executable | TargetKind::SharedLibrary
    ) && target
        .props
        .get(b"COMMON_LANGUAGE_RUNTIME")
        .is_some_and(|value| !matches!(value.as_slice(), b"" | b"netcore"))
}

/// Map a preprocessor definition to a character set, as CMake does.
fn char_set(define: &str) -> Option<&'static str> {
    let is = |name: &str| {
        define == name
            || define
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
    };
    if is("_UNICODE") {
        Some("Unicode")
    } else if is("_MBCS") {
        Some("MultiByte")
    } else if is("_SBCS") {
        Some("NotSet")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::char_set;

    #[test]
    fn character_set_follows_the_first_matching_definition() {
        assert_eq!(char_set("_UNICODE"), Some("Unicode"));
        assert_eq!(char_set("_UNICODE=1"), Some("Unicode"));
        assert_eq!(char_set("_MBCS=1"), Some("MultiByte"));
        assert_eq!(char_set("_SBCS"), Some("NotSet"));
        assert_eq!(char_set("_UNICODEX"), None);
        assert_eq!(char_set("UNICODE"), None);
    }
}
