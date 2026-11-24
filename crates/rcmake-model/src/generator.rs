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
#[path = "generator_tests.rs"]
mod tests;
