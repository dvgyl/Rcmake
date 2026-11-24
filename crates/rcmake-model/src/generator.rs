//! Build-system generator selection (`-G`, `CMAKE_GENERATOR`) and the generator
//! specification (`-T`, `-A`, and the generator instance).
//!
//! The parsers in this module are pure. They return [`GeneratorSpecError`]. The
//! `Display` text of each error is the CMake 4.5.1 message text. The configure
//! commands report the error at the first `project()` or `enable_language()` call,
//! as CMake does.

use core::fmt;

use rcmake_base::BString;

/// A supported Visual Studio generator version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VsVersion {
    /// `Visual Studio 17 2022` (toolset `v143`, tools `17.0`, `.sln`).
    Vs17,
    /// `Visual Studio 18 2026` (toolset `v145`, tools `18.0`, `.slnx`).
    Vs18,
}

impl VsVersion {
    /// All supported versions, oldest first.
    pub const ALL: [Self; 2] = [Self::Vs17, Self::Vs18];

    /// Generator name, for example `Visual Studio 17 2022`.
    #[must_use]
    pub const fn generator_name(self) -> &'static str {
        match self {
            Self::Vs17 => "Visual Studio 17 2022",
            Self::Vs18 => "Visual Studio 18 2026",
        }
    }

    /// Default platform toolset (`CMAKE_VS_PLATFORM_TOOLSET` when `-T` gives no name).
    #[must_use]
    pub const fn default_toolset(self) -> &'static str {
        match self {
            Self::Vs17 => "v143",
            Self::Vs18 => "v145",
        }
    }

    /// MSBuild `ToolsVersion` and `/p:VisualStudioVersion` value.
    #[must_use]
    pub const fn tools_version(self) -> &'static str {
        match self {
            Self::Vs17 => "17.0",
            Self::Vs18 => "18.0",
        }
    }

    /// `true` if the generator writes an XML `.slnx` solution instead of a `.sln` file.
    #[must_use]
    pub const fn writes_slnx(self) -> bool {
        matches!(self, Self::Vs18)
    }
}

/// The `-T` grammar that a generator accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolsetSupport {
    /// The generator rejects every non-empty toolset.
    None,
    /// Xcode grammar: `[<toolset>][,buildsystem=1|12]`.
    Xcode,
    /// Visual Studio grammar: `[<name>][,host=..][,version=..][,cuda=..]...`.
    VisualStudio,
}

/// A build-system generator that Rcmake implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeneratorKind {
    /// `Ninja`.
    Ninja,
    /// `Ninja Multi-Config`.
    NinjaMultiConfig,
    /// `Unix Makefiles`.
    UnixMakefiles,
    /// `Xcode`.
    Xcode,
    /// `Visual Studio <N> <YYYY>`.
    VisualStudio(VsVersion),
}

impl GeneratorKind {
    /// All generators, in `cmake --help` order. Use this list to parse names.
    /// Use [`GeneratorKind::available`] to list the generators of this host.
    pub const ALL: [Self; 6] = [
        Self::UnixMakefiles,
        Self::Ninja,
        Self::NinjaMultiConfig,
        Self::Xcode,
        Self::VisualStudio(VsVersion::Vs17),
        Self::VisualStudio(VsVersion::Vs18),
    ];

    /// Generator name as given to `-G` and stored in `CMAKE_GENERATOR`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ninja => "Ninja",
            Self::NinjaMultiConfig => "Ninja Multi-Config",
            Self::UnixMakefiles => "Unix Makefiles",
            Self::Xcode => "Xcode",
            Self::VisualStudio(version) => version.generator_name(),
        }
    }

    /// One-line description in the `cmake --help` generator list.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Ninja => "Generates build.ninja files.",
            Self::NinjaMultiConfig => "Generates build-<Config>.ninja files.",
            Self::UnixMakefiles => "Generates standard UNIX makefiles.",
            Self::Xcode => "Generate Xcode project files.",
            Self::VisualStudio(VsVersion::Vs17) => "Generates Visual Studio 2022 project files.",
            Self::VisualStudio(VsVersion::Vs18) => "Generates Visual Studio 2026 project files.",
        }
    }

    /// `true` for a multi-configuration generator (`GENERATOR_IS_MULTI_CONFIG`).
    #[must_use]
    pub const fn multi_config(self) -> bool {
        !matches!(self, Self::Ninja | Self::UnixMakefiles)
    }

    /// `true` for a generator that writes IDE project files (Xcode, Visual Studio).
    #[must_use]
    pub const fn is_ide(self) -> bool {
        matches!(self, Self::Xcode | Self::VisualStudio(_))
    }

    /// The `Modules/` file that finds the build tool (`CMAKE_MAKE_PROGRAM`).
    ///
    /// `None` for Visual Studio: the generator finds MSBuild itself.
    #[must_use]
    pub const fn find_make_module(self) -> Option<&'static str> {
        match self {
            Self::Ninja | Self::NinjaMultiConfig => Some("CMakeNinjaFindMake"),
            Self::UnixMakefiles => Some("CMakeUnixFindMake"),
            Self::Xcode => Some("CMakeFindXCode"),
            Self::VisualStudio(_) => None,
        }
    }

    /// `CMAKE_CONFIGURATION_TYPES` when the project does not set it.
    /// Empty for a single-configuration generator.
    #[must_use]
    pub const fn default_configuration_types(self) -> &'static [&'static str] {
        match self {
            Self::Ninja | Self::UnixMakefiles => &[],
            Self::NinjaMultiConfig => &["Debug", "Release", "RelWithDebInfo"],
            Self::Xcode | Self::VisualStudio(_) => {
                &["Debug", "Release", "MinSizeRel", "RelWithDebInfo"]
            }
        }
    }

    /// Value of `CMAKE_CFG_INTDIR`.
    #[must_use]
    pub const fn cfg_int_dir(self) -> &'static str {
        match self {
            Self::Ninja | Self::UnixMakefiles => ".",
            Self::NinjaMultiConfig => "${CONFIGURATION}",
            Self::Xcode => "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)",
            Self::VisualStudio(_) => "$(Configuration)",
        }
    }

    /// The `-T` grammar of the generator.
    #[must_use]
    pub const fn toolset_support(self) -> ToolsetSupport {
        match self {
            Self::Ninja | Self::NinjaMultiConfig | Self::UnixMakefiles => ToolsetSupport::None,
            Self::Xcode => ToolsetSupport::Xcode,
            Self::VisualStudio(_) => ToolsetSupport::VisualStudio,
        }
    }

    /// `true` if the generator accepts `-A` (`CMAKE_GENERATOR_PLATFORM`).
    #[must_use]
    pub const fn supports_platform(self) -> bool {
        matches!(self, Self::VisualStudio(_))
    }

    /// `true` if the generator accepts `CMAKE_GENERATOR_INSTANCE`.
    #[must_use]
    pub const fn supports_instance(self) -> bool {
        matches!(self, Self::VisualStudio(_))
    }

    /// `true` if the generator can be created on this host.
    ///
    /// Xcode needs macOS. Visual Studio needs Windows, or the `vs-cross` Cargo feature
    /// of this crate (structural tests only, never in a release binary).
    #[must_use]
    pub const fn host_available(self) -> bool {
        match self {
            Self::Ninja | Self::NinjaMultiConfig | Self::UnixMakefiles => true,
            Self::Xcode => cfg!(target_os = "macos"),
            Self::VisualStudio(_) => cfg!(any(windows, feature = "vs-cross")),
        }
    }

    /// The generators that this host can create, in `cmake --help` order.
    pub fn available() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(|kind| kind.host_available())
    }

    /// Generator with the exact name `name` (case-sensitive, as in CMake).
    ///
    /// This function also returns generators that this host cannot create. Check
    /// [`GeneratorKind::host_available`] before you use the result for `-G`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name().as_bytes() == name)
    }
}

/// An Xcode version, as reported by `xcodebuild -version` (for example `27.2`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XcodeVersion {
    /// Version text, used in messages (for example `27.2`).
    pub text: BString,
    /// `10 * major + minor` (for example `272`). CMake compares this number.
    pub number: u32,
}

impl XcodeVersion {
    /// Parse `<major>[.<minor>[...]]`. Missing or invalid parts count as `0`.
    #[must_use]
    pub fn parse(text: &[u8]) -> Self {
        let mut parts = text.split(|&byte| byte == b'.').map(leading_number);
        let major = parts.next().flatten().unwrap_or(0);
        let minor = parts.next().flatten().unwrap_or(0);
        Self {
            text: BString::from(text),
            number: major.saturating_mul(10).saturating_add(minor),
        }
    }
}

fn leading_number(part: &[u8]) -> Option<u32> {
    let digits = part.iter().take_while(|byte| byte.is_ascii_digit());
    let mut value: Option<u32> = None;
    for &digit in digits {
        let current = value.unwrap_or(0);
        value = Some(
            current
                .saturating_mul(10)
                .saturating_add(u32::from(digit - b'0')),
        );
    }
    value
}

/// The Xcode build system selected by `-T buildsystem=<n>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum XcodeBuildSystem {
    /// The legacy build system (`buildsystem=1`).
    One,
    /// The new build system (`buildsystem=12`).
    Twelve,
}

impl XcodeBuildSystem {
    /// Value of `CMAKE_XCODE_BUILD_SYSTEM` (`1` or `12`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::One => "1",
            Self::Twelve => "12",
        }
    }

    /// The default for an Xcode version: `12` from Xcode 12, else `1`.
    #[must_use]
    pub const fn default_for(xcode: &XcodeVersion) -> Self {
        if xcode.number >= 120 {
            Self::Twelve
        } else {
            Self::One
        }
    }
}

/// Parsed fields of a Visual Studio `-T` value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VsToolset {
    /// Platform toolset name (`v143`, `v145`, `ClangCL`, ...).
    pub name: Option<BString>,
    /// `host=x64|x86|ARM64` (`CMAKE_VS_PLATFORM_TOOLSET_HOST_ARCHITECTURE`).
    pub host: Option<BString>,
    /// `version=<v>` (`CMAKE_VS_PLATFORM_TOOLSET_VERSION`).
    pub version: Option<BString>,
    /// `cuda=<version|path>`.
    pub cuda: Option<BString>,
    /// `VCTargetsPath=<path>`.
    pub vctargets: Option<BString>,
    /// `fortran=ifx|ifort`.
    pub fortran: Option<BString>,
    /// `customFlagTableDir=<path>`.
    pub custom_flag_table_dir: Option<BString>,
}

/// Parsed fields of a Visual Studio `-A` value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VsPlatform {
    /// Platform name (`x64`, `Win32`, `ARM64`, `ARM`).
    pub name: Option<BString>,
    /// `version=<SDK>` (Windows SDK version).
    pub version: Option<BString>,
}

/// The parsed `-T` value of the active generator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ParsedToolset {
    /// No toolset was given, or the generator has no toolset grammar.
    #[default]
    None,
    /// Xcode toolset fields.
    Xcode {
        /// `CMAKE_XCODE_PLATFORM_TOOLSET` (the first field when it has no `=`).
        toolset: Option<BString>,
        /// Value of an explicit `buildsystem=` field (`1` or `12`).
        build_system: Option<BString>,
    },
    /// Visual Studio toolset fields.
    Vs(VsToolset),
}

/// Raw and parsed `-T`, `-A`, and instance values of the active generator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeneratorSpec {
    /// `CMAKE_GENERATOR_TOOLSET`.
    pub toolset: BString,
    /// `CMAKE_GENERATOR_PLATFORM`.
    pub platform: BString,
    /// `CMAKE_GENERATOR_INSTANCE`.
    pub instance: BString,
    /// Parsed toolset.
    pub parsed: ParsedToolset,
    /// Parsed platform (Visual Studio only).
    pub parsed_platform: Option<VsPlatform>,
}

impl GeneratorSpec {
    /// Check and parse the raw values for `kind`, in CMake order: instance, then
    /// platform, then toolset.
    ///
    /// `xcode` is the Xcode version for the `buildsystem=` check. With `None`, that
    /// check is not done.
    ///
    /// # Errors
    ///
    /// Returns the first [`GeneratorSpecError`], with the CMake 4.5.1 message text.
    pub fn parse(
        kind: GeneratorKind,
        toolset: &[u8],
        platform: &[u8],
        instance: &[u8],
        xcode: Option<&XcodeVersion>,
    ) -> Result<Self, GeneratorSpecError> {
        if !instance.is_empty() && !kind.supports_instance() {
            return Err(GeneratorSpecError::Unsupported {
                generator: kind,
                field: SpecField::Instance,
                value: instance.into(),
            });
        }
        let parsed_platform = if kind.supports_platform() {
            Some(parse_vs_platform(kind, platform)?)
        } else if platform.is_empty() {
            None
        } else {
            return Err(GeneratorSpecError::Unsupported {
                generator: kind,
                field: SpecField::Platform,
                value: platform.into(),
            });
        };
        let parsed = match kind.toolset_support() {
            ToolsetSupport::Xcode => parse_xcode_toolset(kind, toolset, xcode)?,
            ToolsetSupport::VisualStudio => ParsedToolset::Vs(parse_vs_toolset(kind, toolset)?),
            ToolsetSupport::None if toolset.is_empty() => ParsedToolset::None,
            ToolsetSupport::None => {
                return Err(GeneratorSpecError::Unsupported {
                    generator: kind,
                    field: SpecField::Toolset,
                    value: toolset.into(),
                });
            }
        };
        Ok(Self {
            toolset: toolset.into(),
            platform: platform.into(),
            instance: instance.into(),
            parsed,
            parsed_platform,
        })
    }
}

/// The generator option that an error is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpecField {
    /// `-T` (`CMAKE_GENERATOR_TOOLSET`).
    Toolset,
    /// `-A` (`CMAKE_GENERATOR_PLATFORM`).
    Platform,
    /// `CMAKE_GENERATOR_INSTANCE`.
    Instance,
}

impl SpecField {
    /// Word used in CMake messages: `toolset`, `platform`, or `instance`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Toolset => "toolset",
            Self::Platform => "platform",
            Self::Instance => "instance",
        }
    }
}

/// A rejected `-T`, `-A`, or instance value. `Display` gives the CMake message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeneratorSpecError {
    /// The generator does not accept this option.
    Unsupported {
        /// Active generator.
        generator: GeneratorKind,
        /// Rejected option.
        field: SpecField,
        /// Raw value.
        value: BString,
    },
    /// A field after the first `,` has no `=`.
    FieldWithoutEquals {
        /// Active generator.
        generator: GeneratorKind,
        /// Option (`toolset` or `platform`).
        field: SpecField,
        /// Raw value.
        spec: BString,
    },
    /// A field key occurs two times.
    DuplicateKey {
        /// Active generator.
        generator: GeneratorKind,
        /// Option (`toolset` or `platform`).
        field: SpecField,
        /// Raw value.
        spec: BString,
        /// Repeated key.
        key: BString,
    },
    /// Visual Studio: a field is not known (the message shows `key=value`).
    InvalidField {
        /// Active generator.
        generator: GeneratorKind,
        /// Option (`toolset` or `platform`).
        field: SpecField,
        /// Raw value.
        spec: BString,
        /// Rejected `key=value` field.
        item: BString,
    },
    /// Xcode: a toolset key is not known (the message shows only the key).
    XcodeInvalidKey {
        /// Active generator.
        generator: GeneratorKind,
        /// Rejected key.
        key: BString,
    },
    /// Xcode: `buildsystem=` is not `1` or `12`.
    XcodeBuildSystemUnknown {
        /// Active generator.
        generator: GeneratorKind,
        /// Rejected value.
        value: BString,
    },
    /// Xcode: the build system is not allowed with this Xcode version.
    XcodeBuildSystemNotAllowed {
        /// Active generator.
        generator: GeneratorKind,
        /// Requested value.
        value: BString,
        /// Xcode version text.
        xcode: BString,
    },
}

impl fmt::Display for GeneratorSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported {
                generator,
                field,
                value,
            } => {
                let word = field.as_str();
                write!(
                    f,
                    "Generator\n  {}\ndoes not support {word} specification, but {word}\n  {value}\nwas specified.",
                    generator.name()
                )
            }
            Self::FieldWithoutEquals {
                generator,
                field,
                spec,
            } => write!(
                f,
                "Generator\n  {}\ngiven {} specification\n  {spec}\nthat contains a field after the first ',' with no '='.",
                generator.name(),
                field.as_str()
            ),
            Self::DuplicateKey {
                generator,
                field,
                spec,
                key,
            } => write!(
                f,
                "Generator\n  {}\ngiven {} specification\n  {spec}\nthat contains duplicate field key '{key}'.",
                generator.name(),
                field.as_str()
            ),
            Self::InvalidField {
                generator,
                field,
                spec,
                item,
            } => write!(
                f,
                "Generator\n  {}\ngiven {} specification\n  {spec}\nthat contains invalid field '{item}'.",
                generator.name(),
                field.as_str()
            ),
            Self::XcodeInvalidKey { generator, key } => write!(
                f,
                "Generator\n  {}\ngiven toolset specification that contains invalid field '{key}'.",
                generator.name()
            ),
            Self::XcodeBuildSystemUnknown { generator, value } => write!(
                f,
                "Generator\n  {}\ntoolset specification field\n  buildsystem={value}\nvalue is unknown.  It must be '1' or '12'.",
                generator.name()
            ),
            Self::XcodeBuildSystemNotAllowed {
                generator,
                value,
                xcode,
            } => write!(
                f,
                "Generator\n  {}\ntoolset specification field\n  buildsystem={value}\nis not allowed with Xcode {xcode}.",
                generator.name()
            ),
        }
    }
}

impl core::error::Error for GeneratorSpecError {}

/// One `key=value` field of a specification, after the optional leading name.
struct Field<'a> {
    item: &'a [u8],
    key: &'a [u8],
    value: &'a [u8],
}

/// Split a specification into the optional leading name and the `key=value` fields.
///
/// Empty fields are dropped (`a,,b` has two fields), as in CMake.
fn split_spec(
    generator: GeneratorKind,
    field: SpecField,
    spec: &[u8],
) -> Result<(Option<&[u8]>, Vec<Field<'_>>), GeneratorSpecError> {
    let mut items = spec
        .split(|&byte| byte == b',')
        .filter(|item| !item.is_empty());
    let mut items_peek = items.clone();
    let mut name = None;
    if let Some(first) = items_peek.next()
        && !first.contains(&b'=')
    {
        name = Some(first);
        items.next();
    }
    let mut fields: Vec<Field<'_>> = Vec::new();
    for item in items {
        let Some(pos) = item.iter().position(|&byte| byte == b'=') else {
            return Err(GeneratorSpecError::FieldWithoutEquals {
                generator,
                field,
                spec: spec.into(),
            });
        };
        let (key, rest) = item.split_at(pos);
        let value = rest.get(1..).unwrap_or_default();
        if fields.iter().any(|seen| seen.key == key) {
            return Err(GeneratorSpecError::DuplicateKey {
                generator,
                field,
                spec: spec.into(),
                key: key.into(),
            });
        }
        fields.push(Field { item, key, value });
    }
    Ok((name, fields))
}

/// Parse an Xcode `-T` value: `[<toolset>][,buildsystem=1|12]`.
///
/// With `xcode` = `None`, the check of the build system against the Xcode version is
/// not done.
///
/// # Errors
///
/// Returns a [`GeneratorSpecError`] with the CMake 4.5.1 message text.
pub fn parse_xcode_toolset(
    generator: GeneratorKind,
    spec: &[u8],
    xcode: Option<&XcodeVersion>,
) -> Result<ParsedToolset, GeneratorSpecError> {
    let (name, fields) = split_spec(generator, SpecField::Toolset, spec)?;
    let mut build_system = None;
    for field in fields {
        if field.key != b"buildsystem" {
            return Err(GeneratorSpecError::XcodeInvalidKey {
                generator,
                key: field.key.into(),
            });
        }
        let selected = match field.value {
            b"1" => XcodeBuildSystem::One,
            b"12" => XcodeBuildSystem::Twelve,
            _ => {
                return Err(GeneratorSpecError::XcodeBuildSystemUnknown {
                    generator,
                    value: field.value.into(),
                });
            }
        };
        if let Some(xcode) = xcode {
            let allowed = match selected {
                XcodeBuildSystem::Twelve => xcode.number >= 120,
                XcodeBuildSystem::One => xcode.number < 140,
            };
            if !allowed {
                return Err(GeneratorSpecError::XcodeBuildSystemNotAllowed {
                    generator,
                    value: field.value.into(),
                    xcode: xcode.text.clone(),
                });
            }
        }
        build_system = Some(BString::from(field.value));
    }
    Ok(ParsedToolset::Xcode {
        toolset: name.map(BString::from),
        build_system,
    })
}

/// Parse a Visual Studio `-T` value:
/// `[<name>][,host=x64|x86|ARM64][,version=..][,cuda=..][,VCTargetsPath=..][,fortran=..][,customFlagTableDir=..]`.
///
/// # Errors
///
/// Returns a [`GeneratorSpecError`] with the CMake 4.5.1 message text.
pub fn parse_vs_toolset(
    generator: GeneratorKind,
    spec: &[u8],
) -> Result<VsToolset, GeneratorSpecError> {
    let (name, fields) = split_spec(generator, SpecField::Toolset, spec)?;
    let mut toolset = VsToolset {
        name: name.map(BString::from),
        ..VsToolset::default()
    };
    for field in fields {
        let value = Some(BString::from(field.value));
        match field.key {
            b"host" if matches!(field.value, b"x64" | b"x86" | b"ARM64") => toolset.host = value,
            b"version" => toolset.version = value,
            b"cuda" => toolset.cuda = value,
            b"VCTargetsPath" => toolset.vctargets = value,
            b"fortran" => toolset.fortran = value,
            b"customFlagTableDir" => toolset.custom_flag_table_dir = value,
            _ => {
                return Err(GeneratorSpecError::InvalidField {
                    generator,
                    field: SpecField::Toolset,
                    spec: spec.into(),
                    item: field.item.into(),
                });
            }
        }
    }
    Ok(toolset)
}

/// Parse a Visual Studio `-A` value: `[<platform>][,version=<SDK>]`.
///
/// # Errors
///
/// Returns a [`GeneratorSpecError`] with the CMake 4.5.1 message text.
pub fn parse_vs_platform(
    generator: GeneratorKind,
    spec: &[u8],
) -> Result<VsPlatform, GeneratorSpecError> {
    let (name, fields) = split_spec(generator, SpecField::Platform, spec)?;
    let mut platform = VsPlatform {
        name: name.map(BString::from),
        version: None,
    };
    for field in fields {
        if field.key == b"version" {
            platform.version = Some(field.value.into());
        } else {
            return Err(GeneratorSpecError::InvalidField {
                generator,
                field: SpecField::Platform,
                spec: spec.into(),
                item: field.item.into(),
            });
        }
    }
    Ok(platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VS17: GeneratorKind = GeneratorKind::VisualStudio(VsVersion::Vs17);
    const VS18: GeneratorKind = GeneratorKind::VisualStudio(VsVersion::Vs18);

    fn xcode_27_2() -> XcodeVersion {
        XcodeVersion::parse(b"27.2")
    }

    fn message(result: Result<GeneratorSpec, GeneratorSpecError>) -> String {
        result.err().map(|err| err.to_string()).unwrap_or_default()
    }

    #[test]
    fn names_round_trip() {
        for kind in GeneratorKind::ALL {
            assert_eq!(GeneratorKind::from_name(kind.name().as_bytes()), Some(kind));
        }
        assert_eq!(GeneratorKind::from_name(b"ninja"), None);
        assert_eq!(GeneratorKind::from_name(b"Visual Studio 16 2019"), None);
        assert_eq!(GeneratorKind::from_name(b"xcode"), None);
    }

    #[test]
    fn method_table_matches_design() {
        use GeneratorKind::{Ninja, NinjaMultiConfig, UnixMakefiles, Xcode};
        let multi: Vec<bool> = GeneratorKind::ALL.map(GeneratorKind::multi_config).into();
        assert_eq!(multi, [false, false, true, true, true, true]);
        let ide: Vec<bool> = GeneratorKind::ALL.map(GeneratorKind::is_ide).into();
        assert_eq!(ide, [false, false, false, true, true, true]);
        assert_eq!(UnixMakefiles.find_make_module(), Some("CMakeUnixFindMake"));
        assert_eq!(Ninja.find_make_module(), Some("CMakeNinjaFindMake"));
        assert_eq!(
            NinjaMultiConfig.find_make_module(),
            Some("CMakeNinjaFindMake")
        );
        assert_eq!(Xcode.find_make_module(), Some("CMakeFindXCode"));
        assert_eq!(VS17.find_make_module(), None);
        assert!(Ninja.default_configuration_types().is_empty());
        assert!(UnixMakefiles.default_configuration_types().is_empty());
        assert_eq!(
            NinjaMultiConfig.default_configuration_types(),
            ["Debug", "Release", "RelWithDebInfo"]
        );
        for kind in [Xcode, VS17, VS18] {
            assert_eq!(
                kind.default_configuration_types(),
                ["Debug", "Release", "MinSizeRel", "RelWithDebInfo"]
            );
        }
        assert_eq!(Ninja.cfg_int_dir(), ".");
        assert_eq!(UnixMakefiles.cfg_int_dir(), ".");
        assert_eq!(NinjaMultiConfig.cfg_int_dir(), "${CONFIGURATION}");
        assert_eq!(
            Xcode.cfg_int_dir(),
            "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)"
        );
        assert_eq!(VS18.cfg_int_dir(), "$(Configuration)");
        assert_eq!(Ninja.toolset_support(), ToolsetSupport::None);
        assert_eq!(Xcode.toolset_support(), ToolsetSupport::Xcode);
        assert_eq!(VS17.toolset_support(), ToolsetSupport::VisualStudio);
        assert!(!Xcode.supports_platform() && !Xcode.supports_instance());
        assert!(VS17.supports_platform() && VS18.supports_instance());
        assert_eq!(VsVersion::Vs17.default_toolset(), "v143");
        assert_eq!(VsVersion::Vs18.default_toolset(), "v145");
        assert_eq!(VsVersion::Vs17.tools_version(), "17.0");
        assert_eq!(VsVersion::Vs18.tools_version(), "18.0");
        assert!(VsVersion::Vs18.writes_slnx() && !VsVersion::Vs17.writes_slnx());
    }

    #[test]
    fn available_generators_follow_host() {
        let names: Vec<&str> = GeneratorKind::available()
            .map(GeneratorKind::name)
            .collect();
        let mut expected = vec!["Unix Makefiles", "Ninja", "Ninja Multi-Config"];
        if cfg!(target_os = "macos") {
            expected.push("Xcode");
        }
        if cfg!(any(windows, feature = "vs-cross")) {
            expected.extend(["Visual Studio 17 2022", "Visual Studio 18 2026"]);
        }
        assert_eq!(names, expected);
    }

    #[test]
    fn xcode_version_number() {
        assert_eq!(xcode_27_2().number, 272);
        assert_eq!(XcodeVersion::parse(b"12").number, 120);
        assert_eq!(XcodeVersion::parse(b"15.4.1").number, 154);
        assert_eq!(XcodeVersion::parse(b"").number, 0);
        assert_eq!(
            XcodeBuildSystem::default_for(&xcode_27_2()),
            XcodeBuildSystem::Twelve
        );
        assert_eq!(
            XcodeBuildSystem::default_for(&XcodeVersion::parse(b"11.7")),
            XcodeBuildSystem::One
        );
    }

    #[test]
    fn xcode_oracle_error_texts() {
        let xcode = xcode_27_2();
        let parse = |t: &str, a: &str| {
            GeneratorSpec::parse(
                GeneratorKind::Xcode,
                t.as_bytes(),
                a.as_bytes(),
                b"",
                Some(&xcode),
            )
        };
        assert_eq!(
            message(parse("buildsystem=1", "")),
            "Generator\n  Xcode\ntoolset specification field\n  buildsystem=1\nis not allowed with Xcode 27.2."
        );
        assert_eq!(
            message(parse("bogus=1", "")),
            "Generator\n  Xcode\ngiven toolset specification that contains invalid field 'bogus'."
        );
        assert_eq!(
            message(parse("", "arm64")),
            "Generator\n  Xcode\ndoes not support platform specification, but platform\n  arm64\nwas specified."
        );
        assert_eq!(
            message(parse("buildsystem=3", "")),
            "Generator\n  Xcode\ntoolset specification field\n  buildsystem=3\nvalue is unknown.  It must be '1' or '12'."
        );
        assert_eq!(
            message(parse("foo,bar", "")),
            "Generator\n  Xcode\ngiven toolset specification\n  foo,bar\nthat contains a field after the first ',' with no '='."
        );
        assert_eq!(
            message(parse("buildsystem=12,buildsystem=12", "")),
            "Generator\n  Xcode\ngiven toolset specification\n  buildsystem=12,buildsystem=12\nthat contains duplicate field key 'buildsystem'."
        );
        let old = XcodeVersion::parse(b"11.0");
        assert_eq!(
            parse_xcode_toolset(GeneratorKind::Xcode, b"buildsystem=12", Some(&old))
                .err()
                .map(|err| err.to_string()),
            Some(
                "Generator\n  Xcode\ntoolset specification field\n  buildsystem=12\nis not allowed with Xcode 11.0."
                    .to_owned()
            )
        );
    }

    #[test]
    fn xcode_accepts_toolset_and_build_system() {
        let xcode = xcode_27_2();
        let spec = GeneratorSpec::parse(
            GeneratorKind::Xcode,
            b"foo,buildsystem=12",
            b"",
            b"",
            Some(&xcode),
        );
        assert_eq!(
            spec.map(|spec| spec.parsed),
            Ok(ParsedToolset::Xcode {
                toolset: Some("foo".into()),
                build_system: Some("12".into()),
            })
        );
        assert_eq!(
            parse_xcode_toolset(GeneratorKind::Xcode, b"", Some(&xcode)),
            Ok(ParsedToolset::Xcode {
                toolset: None,
                build_system: None
            })
        );
        assert_eq!(
            parse_xcode_toolset(GeneratorKind::Xcode, b"buildsystem=1", None).map(|_| ()),
            Ok(())
        );
    }

    #[test]
    fn non_ide_generators_reject_every_option() {
        assert_eq!(
            message(GeneratorSpec::parse(
                GeneratorKind::Ninja,
                b"v143",
                b"",
                b"",
                None
            )),
            "Generator\n  Ninja\ndoes not support toolset specification, but toolset\n  v143\nwas specified."
        );
        assert_eq!(
            message(GeneratorSpec::parse(
                GeneratorKind::UnixMakefiles,
                b"",
                b"x64",
                b"",
                None
            )),
            "Generator\n  Unix Makefiles\ndoes not support platform specification, but platform\n  x64\nwas specified."
        );
        // The instance is checked first.
        assert_eq!(
            message(GeneratorSpec::parse(
                GeneratorKind::Xcode,
                b"bogus=1",
                b"x64",
                b"/opt/vs",
                None
            )),
            "Generator\n  Xcode\ndoes not support instance specification, but instance\n  /opt/vs\nwas specified."
        );
        assert_eq!(
            GeneratorSpec::parse(GeneratorKind::NinjaMultiConfig, b"", b"", b"", None),
            Ok(GeneratorSpec::default())
        );
    }

    #[test]
    fn vs_toolset_fields() {
        let parsed = parse_vs_toolset(
            VS17,
            b"ClangCL,host=x64,version=14.44,cuda=12.4,VCTargetsPath=C:/t,fortran=ifx,customFlagTableDir=/f",
        );
        assert_eq!(
            parsed,
            Ok(VsToolset {
                name: Some("ClangCL".into()),
                host: Some("x64".into()),
                version: Some("14.44".into()),
                cuda: Some("12.4".into()),
                vctargets: Some("C:/t".into()),
                fortran: Some("ifx".into()),
                custom_flag_table_dir: Some("/f".into()),
            })
        );
        assert_eq!(
            parse_vs_toolset(VS18, b",,host=ARM64"),
            Ok(VsToolset {
                host: Some("ARM64".into()),
                ..VsToolset::default()
            })
        );
    }

    #[test]
    fn vs_oracle_error_texts() {
        let err = |result: Result<VsToolset, GeneratorSpecError>| {
            result.err().map(|err| err.to_string()).unwrap_or_default()
        };
        assert_eq!(
            err(parse_vs_toolset(VS17, b"v143,host=arm")),
            "Generator\n  Visual Studio 17 2022\ngiven toolset specification\n  v143,host=arm\nthat contains invalid field 'host=arm'."
        );
        assert_eq!(
            err(parse_vs_toolset(VS18, b"v145,bogus")),
            "Generator\n  Visual Studio 18 2026\ngiven toolset specification\n  v145,bogus\nthat contains a field after the first ',' with no '='."
        );
        assert_eq!(
            err(parse_vs_toolset(VS17, b"version=1,version=2")),
            "Generator\n  Visual Studio 17 2022\ngiven toolset specification\n  version=1,version=2\nthat contains duplicate field key 'version'."
        );
        assert_eq!(
            parse_vs_platform(VS17, b"x64,sdk=10")
                .err()
                .map(|err| err.to_string()),
            Some(
                "Generator\n  Visual Studio 17 2022\ngiven platform specification\n  x64,sdk=10\nthat contains invalid field 'sdk=10'."
                    .to_owned()
            )
        );
        assert_eq!(
            parse_vs_platform(VS17, b"x64,version=10.0.22621.0"),
            Ok(VsPlatform {
                name: Some("x64".into()),
                version: Some("10.0.22621.0".into()),
            })
        );
    }

    #[test]
    fn vs_spec_keeps_raw_values() {
        let spec = GeneratorSpec::parse(VS17, b"v143", b"Win32", b"C:/VS", None);
        assert_eq!(
            spec,
            Ok(GeneratorSpec {
                toolset: "v143".into(),
                platform: "Win32".into(),
                instance: "C:/VS".into(),
                parsed: ParsedToolset::Vs(VsToolset {
                    name: Some("v143".into()),
                    ..VsToolset::default()
                }),
                parsed_platform: Some(VsPlatform {
                    name: Some("Win32".into()),
                    version: None,
                }),
            })
        );
    }
}
