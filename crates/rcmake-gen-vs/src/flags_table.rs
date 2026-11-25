//! MSBuild properties for documented MSVC flags.

use std::collections::BTreeMap;

use crate::{cuda::OptionText, msbuild_value, rules::quote, windows, xml::Writer};

#[derive(Default)]
pub(crate) struct Settings {
    values: BTreeMap<&'static str, String>,
    lists: BTreeMap<&'static str, Vec<String>>,
    options: Vec<String>,
    condition_banner: bool,
}

impl Settings {
    pub(crate) fn set(&mut self, key: &'static str, value: impl Into<String>) {
        self.values.insert(key, value.into());
    }

    pub(crate) fn take(&mut self, key: &str) -> Option<String> {
        self.values.remove(key)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub(crate) fn list(&mut self, key: &'static str, value: impl Into<String>) {
        self.lists.entry(key).or_default().push(value.into());
    }

    pub(crate) fn list_unique(&mut self, key: &'static str, value: String) {
        let values = self.lists.entry(key).or_default();
        if !values.contains(&value) {
            values.push(value);
        }
    }

    pub(crate) fn option(&mut self, flag: &str) {
        self.options.push(msbuild_value(&quote(flag)));
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.values.extend(other.values);
        for (key, values) in other.lists {
            self.lists.entry(key).or_default().extend(values);
        }
        self.options.extend(other.options);
    }

    pub(crate) fn compiler_defaults(&mut self, toolset: &str) {
        if toolset == "v142" && !self.values.contains_key("RemoveUnreferencedCodeData") {
            self.condition_banner = true;
        }
        for (key, value) in [
            ("BasicRuntimeChecks", "Default"),
            ("BufferSecurityCheck", ""),
            ("CallingConvention", ""),
            ("FloatingPointModel", ""),
            ("ForceConformanceInForLoopScope", ""),
            ("MinimalRebuild", ""),
            ("Optimization", ""),
            ("RemoveUnreferencedCodeData", ""),
            ("RuntimeLibrary", ""),
            ("SupportJustMyCode", ""),
            ("TreatWChar_tAsBuiltInType", ""),
        ] {
            self.values.entry(key).or_insert_with(|| value.into());
        }
    }

    pub(crate) fn write(&self, xml: &mut Writer) {
        self.write_condition(xml, &[]);
    }

    pub(crate) fn write_condition(&self, xml: &mut Writer, attrs: &[(&str, &str)]) {
        for (key, value) in &self.values {
            if *key == "SuppressStartupBanner" && self.condition_banner {
                xml.text(
                    key,
                    &[("Condition", "'$(VCToolsVersion)' >= '14.29'")],
                    value,
                );
            } else {
                xml.text(key, attrs, value);
            }
        }
        for (key, values) in &self.lists {
            let values = values.join(";");
            let value = if *key == "PreprocessorDefinitions" {
                format!("%({key});{values}")
            } else {
                format!("{values};%({key})")
            };
            xml.text(key, attrs, &value);
        }
        if !self.options.is_empty() {
            xml.text(
                "AdditionalOptions",
                attrs,
                &format!("%(AdditionalOptions) {}", self.options.join(" ")),
            );
        }
    }
}

pub(crate) fn compile(flags: impl IntoIterator<Item = String>) -> Settings {
    let mut flags = flags.into_iter();
    let mut settings = Settings::default();
    while let Some(flag) = flags.next() {
        let key = flag
            .strip_prefix(['-', '/'])
            .or_else(|| flag.starts_with("std:").then_some(flag.as_str()));
        match key {
            Some("D" | "I") => {
                let property = if key == Some("D") {
                    "PreprocessorDefinitions"
                } else {
                    "AdditionalIncludeDirectories"
                };
                if let Some(value) = flags.next() {
                    let value = if key == Some("I") {
                        windows(&value)
                    } else {
                        value
                    };
                    settings.list(property, msbuild_value(&value));
                } else {
                    settings.option(&flag);
                }
            }
            Some(key) if key.starts_with('D') && !key.starts_with("DEBUG") => {
                settings.list(
                    "PreprocessorDefinitions",
                    msbuild_value(key.strip_prefix('D').unwrap_or_default()),
                );
            }
            Some(key) if key.starts_with('I') => {
                settings.list(
                    "AdditionalIncludeDirectories",
                    msbuild_value(&windows(key.strip_prefix('I').unwrap_or_default())),
                );
            }
            Some(key) => match compile_property(key) {
                Some((property, value)) => settings.set(property, value),
                None => settings.option(&flag),
            },
            None => settings.option(&flag),
        }
    }
    settings
}

fn compile_property(flag: &str) -> Option<(&'static str, &'static str)> {
    Some(match flag {
        "Od" | "O0" => ("Optimization", "Disabled"),
        "O1" => ("Optimization", "MinSpace"),
        "O2" => ("Optimization", "MaxSpeed"),
        "TC" => ("CompileAs", "CompileAsC"),
        "TP" => ("CompileAs", "CompileAsCpp"),
        "RTCs" => ("BasicRuntimeChecks", "StackFrameRuntimeCheck"),
        "RTCu" => ("BasicRuntimeChecks", "UninitializedLocalUsageCheck"),
        "RTC1" | "RTCsu" | "RTCus" => ("BasicRuntimeChecks", "EnableFastChecks"),
        "GS" => ("BufferSecurityCheck", "true"),
        "GS-" => ("BufferSecurityCheck", "false"),
        "Gd" => ("CallingConvention", "Cdecl"),
        "Gr" => ("CallingConvention", "FastCall"),
        "Gz" => ("CallingConvention", "StdCall"),
        "Gv" => ("CallingConvention", "VectorCall"),
        "fp:precise" => ("FloatingPointModel", "Precise"),
        "fp:strict" => ("FloatingPointModel", "Strict"),
        "fp:fast" => ("FloatingPointModel", "Fast"),
        "Gm" => ("MinimalRebuild", "true"),
        "Gm-" => ("MinimalRebuild", "false"),
        "JMC" => ("SupportJustMyCode", "true"),
        "JMC-" => ("SupportJustMyCode", "false"),
        "nologo" => ("SuppressStartupBanner", "true"),
        "W0" => ("WarningLevel", "TurnOffAllWarnings"),
        "W1" => ("WarningLevel", "Level1"),
        "W2" => ("WarningLevel", "Level2"),
        "W3" => ("WarningLevel", "Level3"),
        "W4" => ("WarningLevel", "Level4"),
        "WX" => ("TreatWarningAsError", "true"),
        "WX-" => ("TreatWarningAsError", "false"),
        "MD" => ("RuntimeLibrary", "MultiThreadedDLL"),
        "MDd" => ("RuntimeLibrary", "MultiThreadedDebugDLL"),
        "MT" => ("RuntimeLibrary", "MultiThreaded"),
        "MTd" => ("RuntimeLibrary", "MultiThreadedDebug"),
        "Zi" => ("DebugInformationFormat", "ProgramDatabase"),
        "ZI" => ("DebugInformationFormat", "EditAndContinue"),
        "Z7" => ("DebugInformationFormat", "OldStyle"),
        "EHsc" => ("ExceptionHandling", "Sync"),
        "GR" => ("RuntimeTypeInfo", "true"),
        "GR-" => ("RuntimeTypeInfo", "false"),
        "std:c++14" => ("LanguageStandard", "stdcpp14"),
        "std:c++17" => ("LanguageStandard", "stdcpp17"),
        "std:c++20" => ("LanguageStandard", "stdcpp20"),
        "std:c++23latest" => ("LanguageStandard", "stdcpp23"),
        "std:c++latest" => ("LanguageStandard", "stdcpplatest"),
        "std:c11" => ("LanguageStandard_C", "stdc11"),
        "std:c17" => ("LanguageStandard_C", "stdc17"),
        "std:clatest" => ("LanguageStandard_C", "stdclatest"),
        "permissive-" => ("ConformanceMode", "true"),
        "permissive" => ("ConformanceMode", "false"),
        "Zc:wchar_t" => ("TreatWChar_tAsBuiltInType", "true"),
        "Zc:wchar_t-" => ("TreatWChar_tAsBuiltInType", "false"),
        "Zc:forScope" => ("ForceConformanceInForLoopScope", "true"),
        "Zc:forScope-" => ("ForceConformanceInForLoopScope", "false"),
        "Zc:inline" => ("RemoveUnreferencedCodeData", "true"),
        "Zc:inline-" => ("RemoveUnreferencedCodeData", "false"),
        "Zc:preprocessor" => ("UseStandardPreprocessor", "true"),
        "Zc:preprocessor-" => ("UseStandardPreprocessor", "false"),
        _ => return None,
    })
}

pub(crate) fn link(flags: impl IntoIterator<Item = String>, archive: bool) -> Settings {
    let mut settings = Settings::default();
    let warnings = if archive {
        "TreatLibWarningAsErrors"
    } else {
        "TreatLinkerWarningAsErrors"
    };
    for flag in flags {
        let key = flag.trim_start_matches(['-', '/']);
        match key.to_ascii_uppercase().as_str() {
            "DEBUG" if !archive => settings.set("GenerateDebugInformation", "true"),
            "DEBUG:FULL" if !archive => settings.set("GenerateDebugInformation", "DebugFull"),
            "DEBUG:NONE" if !archive => settings.set("GenerateDebugInformation", "false"),
            "DEBUG:FASTLINK" if !archive => {
                settings.set("GenerateDebugInformation", "DebugFastLink")
            }
            "SUBSYSTEM:CONSOLE" if !archive => settings.set("SubSystem", "Console"),
            "SUBSYSTEM:WINDOWS" if !archive => settings.set("SubSystem", "Windows"),
            "SUBSYSTEM:NATIVE" if !archive => settings.set("SubSystem", "Native"),
            "SUBSYSTEM:EFI_APPLICATION" if !archive => settings.set("SubSystem", "EFI Application"),
            "WX" => settings.set(warnings, "true"),
            "WX:NO" => settings.set(warnings, "false"),
            "INCREMENTAL" | "INCREMENTAL:YES" if !archive => {
                settings.set("LinkIncremental", "true")
            }
            "INCREMENTAL:NO" if !archive => settings.set("LinkIncremental", "false"),
            "MANIFEST" if !archive => settings.set("GenerateManifest", "true"),
            "MANIFEST:NO" if !archive => settings.set("GenerateManifest", "false"),
            "LTCG" if archive => settings.set("LinkTimeCodeGeneration", "true"),
            _ if !archive && key.contains(':') => {
                let (name, value) = key.split_once(':').unwrap_or_default();
                if let Some(property) = match name.to_ascii_uppercase().as_str() {
                    "DEF" => Some("ModuleDefinitionFile"),
                    "IMPLIB" => Some("ImportLibrary"),
                    "PDB" => Some("ProgramDatabaseFile"),
                    _ => None,
                } {
                    let path = windows(value);
                    settings.set(property, OptionText::WithProperties(&path).escaped());
                } else if name.eq_ignore_ascii_case("LIBPATH") {
                    settings.list(
                        "AdditionalLibraryDirectories",
                        msbuild_value(&windows(value)),
                    );
                } else {
                    settings.option(&flag);
                }
            }
            _ if key.to_ascii_uppercase().starts_with("LIBPATH:") => {
                if let Some((_, path)) = key.split_once(':') {
                    settings.list(
                        "AdditionalLibraryDirectories",
                        msbuild_value(&windows(path)),
                    );
                }
            }
            _ => settings.option(&flag),
        }
    }
    settings
}

// Split a flag string. Preserve backslashes in Windows paths and decode POSIX quotes
// from the lowered graph.
pub(crate) fn split(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut has_word = false;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if quote != Some('\'')
                && chars.peek().is_some_and(|ch| {
                    matches!(ch, '"' | '\'') || (quote.is_none() && ch.is_whitespace())
                }) =>
            {
                word.extend(chars.next());
                has_word = true;
            }
            '"' | '\'' if quote == Some(ch) => quote = None,
            '"' | '\'' if quote.is_none() => {
                quote = Some(ch);
                has_word = true;
            }
            ch if ch.is_whitespace() && quote.is_none() => {
                if has_word {
                    words.push(std::mem::take(&mut word));
                    has_word = false;
                }
            }
            _ => {
                word.push(ch);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(word);
    }
    words
}
