//! Targets, usage requirements, link items, and file sets.

use indexmap::IndexMap;
use rcmake_base::{BString, list};
use rcmake_diag::{Backtrace, PolicyMap};

use crate::{BT, CommandId, DirId, FileSetId, PropertyMap, TargetId};

/// Target type (`TYPE` target property).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// `add_executable()`.
    Executable,
    /// `add_library(STATIC)`.
    StaticLibrary,
    /// `add_library(SHARED)`.
    SharedLibrary,
    /// `add_library(MODULE)`.
    ModuleLibrary,
    /// `add_library(OBJECT)`.
    ObjectLibrary,
    /// `add_library(INTERFACE)`.
    InterfaceLibrary,
    /// `add_library(UNKNOWN IMPORTED)`.
    UnknownLibrary,
    /// `add_custom_target()`.
    Utility,
    /// A target that the generator creates (`install`, `test`, and so on).
    GlobalTarget,
}

impl TargetKind {
    /// Value of the `TYPE` target property.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Executable => "EXECUTABLE",
            Self::StaticLibrary => "STATIC_LIBRARY",
            Self::SharedLibrary => "SHARED_LIBRARY",
            Self::ModuleLibrary => "MODULE_LIBRARY",
            Self::ObjectLibrary => "OBJECT_LIBRARY",
            Self::InterfaceLibrary => "INTERFACE_LIBRARY",
            Self::UnknownLibrary => "UNKNOWN_LIBRARY",
            Self::Utility => "UTILITY",
            Self::GlobalTarget => "GLOBAL_TARGET",
        }
    }

    /// `true` for every library kind.
    #[must_use]
    pub const fn is_library(self) -> bool {
        matches!(
            self,
            Self::StaticLibrary
                | Self::SharedLibrary
                | Self::ModuleLibrary
                | Self::ObjectLibrary
                | Self::InterfaceLibrary
                | Self::UnknownLibrary
        )
    }

    /// `true` if `PRIVATE` link dependencies propagate as `$<LINK_ONLY:...>` (ADR-007).
    #[must_use]
    pub const fn propagates_private_links(self) -> bool {
        matches!(self, Self::StaticLibrary | Self::ObjectLibrary)
    }
}

/// Visibility of an `IMPORTED` target name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportedVisibility {
    /// Visible in the creating directory and its subdirectories created later.
    Local,
    /// `IMPORTED GLOBAL` (or `IMPORTED_GLOBAL` set to true): visible everywhere.
    Global,
}

/// Where a target comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetOrigin {
    /// A target that this project builds.
    Normal,
    /// An `IMPORTED` target.
    Imported(ImportedVisibility),
}

/// What a target name resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetBinding {
    /// The name of a real target.
    Target(TargetId),
    /// An `ALIAS` name for a real target.
    Alias(TargetId),
}

impl TargetBinding {
    /// The real target.
    #[must_use]
    pub const fn id(self) -> TargetId {
        match self {
            Self::Target(id) | Self::Alias(id) => id,
        }
    }
}

/// `PRIVATE`, `PUBLIC`, or `INTERFACE` keyword of a `target_*()` command or file set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsageScope {
    /// `PRIVATE`: the target's own build only.
    Private,
    /// `PUBLIC`: the target and its consumers.
    Public,
    /// `INTERFACE`: consumers only.
    Interface,
}

/// One `LINK_LIBRARIES` or `INTERFACE_LINK_LIBRARIES` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkItem {
    /// Item text as written (can hold generator expressions).
    pub text: BT<BString>,
    /// Directory whose target names resolve the item (`CMP0079`: the directory of the
    /// `target_link_libraries()` call).
    pub lookup_dir: DirId,
}

/// A usage-requirement property with backtraces per entry (ADR-007).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageField {
    /// `INCLUDE_DIRECTORIES`.
    IncludeDirectories,
    /// `COMPILE_DEFINITIONS`.
    CompileDefinitions,
    /// `COMPILE_OPTIONS`.
    CompileOptions,
    /// `COMPILE_FEATURES`.
    CompileFeatures,
    /// `LINK_OPTIONS`.
    LinkOptions,
    /// `LINK_DIRECTORIES`.
    LinkDirectories,
    /// `PRECOMPILE_HEADERS`.
    PrecompileHeaders,
    /// `INTERFACE_INCLUDE_DIRECTORIES`.
    InterfaceIncludeDirectories,
    /// `INTERFACE_SYSTEM_INCLUDE_DIRECTORIES`.
    InterfaceSystemIncludeDirectories,
    /// `INTERFACE_COMPILE_DEFINITIONS`.
    InterfaceCompileDefinitions,
    /// `INTERFACE_COMPILE_OPTIONS`.
    InterfaceCompileOptions,
    /// `INTERFACE_COMPILE_FEATURES`.
    InterfaceCompileFeatures,
    /// `INTERFACE_LINK_OPTIONS`.
    InterfaceLinkOptions,
    /// `INTERFACE_LINK_DIRECTORIES`.
    InterfaceLinkDirectories,
    /// `INTERFACE_LINK_DEPENDS`.
    InterfaceLinkDepends,
    /// `INTERFACE_PRECOMPILE_HEADERS`.
    InterfacePrecompileHeaders,
    /// `INTERFACE_SOURCES`.
    InterfaceSources,
}

impl UsageField {
    /// Every usage field.
    pub const ALL: [Self; 17] = [
        Self::IncludeDirectories,
        Self::CompileDefinitions,
        Self::CompileOptions,
        Self::CompileFeatures,
        Self::LinkOptions,
        Self::LinkDirectories,
        Self::PrecompileHeaders,
        Self::InterfaceIncludeDirectories,
        Self::InterfaceSystemIncludeDirectories,
        Self::InterfaceCompileDefinitions,
        Self::InterfaceCompileOptions,
        Self::InterfaceCompileFeatures,
        Self::InterfaceLinkOptions,
        Self::InterfaceLinkDirectories,
        Self::InterfaceLinkDepends,
        Self::InterfacePrecompileHeaders,
        Self::InterfaceSources,
    ];

    /// Property name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::IncludeDirectories => "INCLUDE_DIRECTORIES",
            Self::CompileDefinitions => "COMPILE_DEFINITIONS",
            Self::CompileOptions => "COMPILE_OPTIONS",
            Self::CompileFeatures => "COMPILE_FEATURES",
            Self::LinkOptions => "LINK_OPTIONS",
            Self::LinkDirectories => "LINK_DIRECTORIES",
            Self::PrecompileHeaders => "PRECOMPILE_HEADERS",
            Self::InterfaceIncludeDirectories => "INTERFACE_INCLUDE_DIRECTORIES",
            Self::InterfaceSystemIncludeDirectories => "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES",
            Self::InterfaceCompileDefinitions => "INTERFACE_COMPILE_DEFINITIONS",
            Self::InterfaceCompileOptions => "INTERFACE_COMPILE_OPTIONS",
            Self::InterfaceCompileFeatures => "INTERFACE_COMPILE_FEATURES",
            Self::InterfaceLinkOptions => "INTERFACE_LINK_OPTIONS",
            Self::InterfaceLinkDirectories => "INTERFACE_LINK_DIRECTORIES",
            Self::InterfaceLinkDepends => "INTERFACE_LINK_DEPENDS",
            Self::InterfacePrecompileHeaders => "INTERFACE_PRECOMPILE_HEADERS",
            Self::InterfaceSources => "INTERFACE_SOURCES",
        }
    }

    /// Field with property name `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| field.name().as_bytes() == name)
    }

    /// `true` for the fields that a directory also has (`include_directories()`,
    /// `add_compile_definitions()`, `add_compile_options()`, `add_link_options()`,
    /// `link_directories()`).
    #[must_use]
    pub const fn on_directory(self) -> bool {
        matches!(
            self,
            Self::IncludeDirectories
                | Self::CompileDefinitions
                | Self::CompileOptions
                | Self::LinkOptions
                | Self::LinkDirectories
        )
    }
}

/// Usage requirements of a target or directory. Each entry keeps its backtrace.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageProps {
    /// Entries of each [`UsageField`] in call order. A missing key means "not set".
    pub fields: IndexMap<UsageField, Vec<BT<BString>>>,
    /// `LINK_LIBRARIES` entries (`None` while not set).
    pub link_libraries: Option<Vec<LinkItem>>,
    /// `INTERFACE_LINK_LIBRARIES` entries (`None` while not set).
    pub interface_link_libraries: Option<Vec<LinkItem>>,
}

impl UsageProps {
    /// Entries of `field` (empty when not set).
    #[must_use]
    pub fn entries(&self, field: UsageField) -> &[BT<BString>] {
        self.fields.get(&field).map_or(&[], Vec::as_slice)
    }

    /// Joined list value of `field`, or `None` when it is not set.
    #[must_use]
    pub fn joined(&self, field: UsageField) -> Option<BString> {
        self.fields
            .get(&field)
            .map(|entries| join_entries(entries.iter().map(|e| e.value.as_slice())))
    }

    /// Append one entry to `field`. Empty values are dropped, as CMake does.
    pub fn push(&mut self, field: UsageField, value: BT<BString>) {
        let entries = self.fields.entry(field).or_default();
        if !value.value.is_empty() {
            entries.push(value);
        }
    }
}

/// Join entries as a CMake list, skipping empty entries.
pub(crate) fn join_entries<'a>(entries: impl Iterator<Item = &'a [u8]>) -> BString {
    list::join(entries.filter(|e| !e.is_empty()))
}

/// Policy values recorded when a target is created (`cmTarget` policy map).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetPolicySnapshot {
    /// All policy settings active at the `add_*()` call.
    pub policies: PolicyMap,
}

/// Kind of a [`FileSet`] (`target_sources(FILE_SET ... TYPE ...)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileSetKind {
    /// `HEADERS`.
    Headers,
    /// `CXX_MODULES`.
    CxxModules,
}

impl FileSetKind {
    /// `TYPE` keyword value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Headers => "HEADERS",
            Self::CxxModules => "CXX_MODULES",
        }
    }

    /// Kind with `TYPE` keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"HEADERS" => Some(Self::Headers),
            b"CXX_MODULES" => Some(Self::CxxModules),
            _ => None,
        }
    }
}

/// A named file set of a target.
#[derive(Clone, Debug, PartialEq)]
pub struct FileSet {
    /// Owning target.
    pub target: TargetId,
    /// File set name.
    pub name: BString,
    /// File set type.
    pub kind: FileSetKind,
    /// `PRIVATE`, `PUBLIC`, or `INTERFACE`.
    pub scope: UsageScope,
    /// `BASE_DIRS` entries.
    pub base_dirs: Vec<BT<BString>>,
    /// `FILES` entries.
    pub files: Vec<BT<BString>>,
}

/// A target in the project model (`cmTarget`).
#[derive(Clone, Debug)]
pub struct Target {
    /// Target name.
    pub name: BString,
    /// Target type.
    pub kind: TargetKind,
    /// Normal or imported.
    pub origin: TargetOrigin,
    /// Directory of the `add_*()` call.
    pub dir: DirId,
    /// Backtrace of the `add_*()` call.
    pub backtrace: Backtrace,
    /// Policies at creation.
    pub policies: TargetPolicySnapshot,
    /// Properties that are not usage requirements or sources.
    pub props: PropertyMap,
    /// Usage requirements.
    pub usage: UsageProps,
    /// `SOURCES` entries.
    pub sources: Vec<BT<BString>>,
    /// File sets in creation order.
    pub file_sets: Vec<FileSetId>,
    /// `add_dependencies()` entries.
    pub utility_deps: Vec<BT<BString>>,
    /// `PRE_BUILD`, `PRE_LINK`, and `POST_BUILD` custom commands.
    pub event_commands: Vec<CommandId>,
}

impl Target {
    /// A new target without properties.
    #[must_use]
    pub fn new(
        name: BString,
        kind: TargetKind,
        origin: TargetOrigin,
        dir: DirId,
        backtrace: Backtrace,
        policies: PolicyMap,
    ) -> Self {
        Self {
            name,
            kind,
            origin,
            dir,
            backtrace,
            policies: TargetPolicySnapshot { policies },
            props: PropertyMap::default(),
            usage: UsageProps::default(),
            sources: Vec::new(),
            file_sets: Vec::new(),
            utility_deps: Vec::new(),
            event_commands: Vec::new(),
        }
    }

    /// `true` for an `IMPORTED` target.
    #[must_use]
    pub const fn is_imported(&self) -> bool {
        matches!(self.origin, TargetOrigin::Imported(_))
    }

    /// `true` if the name is visible from every directory.
    #[must_use]
    pub const fn is_global(&self) -> bool {
        matches!(
            self.origin,
            TargetOrigin::Normal | TargetOrigin::Imported(ImportedVisibility::Global)
        )
    }
}
