//! IDE facts: structured, unescaped per-target values for the IDE generators
//! (Xcode and Visual Studio). This is an extension of layer 2 (ADR-009, ADR-014,
//! ADR-015).
//!
//! The Ninja and Makefile generators use the lowered, shell-escaped
//! [`crate::BuildGraph`]. The IDE generators need raw values instead: raw flags, raw
//! link items, bundle data, and per-target attributes. Each IDE generator escapes the
//! values for its own file format.
//!
//! The resolver fills [`crate::ResolvedTarget::ide`] only when
//! [`crate::ResolveOptions::ide`] is set. In Wave 0 of Milestone 4, the field always
//! stays `None`.

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_model::{BT, TargetId};

/// The IDE file format that the facts are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdeFlavor {
    /// Xcode (`project.pbxproj`).
    Xcode,
    /// Visual Studio (`.vcxproj`, `.sln`, `.slnx`).
    VisualStudio,
}

/// IDE facts of one target and one configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdeFacts {
    /// Output directory and file name parts.
    pub naming: OutputNaming,
    /// Apple bundle form of the target.
    pub bundle: BundleKind,
    /// Absolute path of the generated `Info.plist`, if the target has one.
    pub info_plist: Option<BString>,
    /// Every source, header, resource, and object file of the target.
    pub sources: Vec<IdeSource>,
    /// Compile settings, one entry per language.
    pub compile: Vec<IdeCompileSettings>,
    /// Link items in link order.
    pub link_items: Vec<IdeLinkItem>,
    /// Raw link options (not shell-escaped).
    pub link_options: Vec<BT<BString>>,
    /// Link directories.
    pub link_directories: Vec<BString>,
    /// Run-time search paths of the build tree.
    pub build_rpaths: Vec<BString>,
    /// `INSTALL_NAME_DIR` of the build tree (Apple).
    pub install_name_dir: Option<BString>,
    /// Custom commands and build events of the target.
    pub custom: Vec<IdeCustomStep>,
    /// `XCODE_ATTRIBUTE_*` (Xcode) or `VS_*` (Visual Studio) values for this
    /// configuration. The keys keep `[sdk=...]` and `[arch=...]` filters.
    pub attributes: IndexMap<BString, BString>,
    /// Precompiled headers, one entry per language.
    pub pch: Vec<IdePch>,
}

/// Output directory and file name parts of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputNaming {
    /// Absolute output directory.
    pub dir: BString,
    /// File name prefix (for example `lib`).
    pub prefix: BString,
    /// Base name (`OUTPUT_NAME` or the target name).
    pub base_name: BString,
    /// File name suffix (for example `.dylib`).
    pub suffix: BString,
    /// `VERSION` property.
    pub version: Option<BString>,
    /// `SOVERSION` property.
    pub soversion: Option<BString>,
}

/// Apple bundle form of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BundleKind {
    /// Not a bundle.
    #[default]
    None,
    /// `MACOSX_BUNDLE` executable (`.app`).
    App,
    /// `FRAMEWORK` library.
    Framework {
        /// `FRAMEWORK_VERSION` (default `A`).
        version: BString,
    },
    /// `BUNDLE` module or library.
    Bundle {
        /// `BUNDLE_EXTENSION` (default `bundle`).
        extension: BString,
    },
}

/// The role of one file in an IDE project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdeSourceKind {
    /// A source file that is compiled.
    Compile {
        /// Source language (for example `CXX`).
        language: BString,
    },
    /// A header file.
    Header,
    /// A resource (copied into a bundle).
    Resource,
    /// An object file made by another target (`$<TARGET_OBJECTS>`).
    Object,
    /// An object file from outside the project (`EXTERNAL_OBJECT`).
    ExternalObject,
    /// A `CMakeLists.txt` or other listfile.
    ListFile,
    /// Any other file.
    Other,
}

/// One file of an IDE project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeSource {
    /// Absolute path.
    pub path: BString,
    /// Role of the file.
    pub kind: IdeSourceKind,
    /// `source_group()` path, with `\` separators (empty for the default group).
    pub group: BString,
    /// The file is the output of a custom command.
    pub generated: bool,
    /// Visual Studio: configurations that do not build the file (`ExcludedFromBuild`).
    pub excluded_configs: Vec<BString>,
}

/// Compile settings of one language.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdeCompileSettings {
    /// Language (for example `C` or `CXX`).
    pub language: BString,
    /// Raw compile flags (not shell-escaped).
    pub flags: Vec<BT<BString>>,
    /// Preprocessor definitions (`NAME` or `NAME=value`).
    pub defines: Vec<BT<BString>>,
    /// Include directories.
    pub includes: Vec<IdeInclude>,
}

/// One include directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeInclude {
    /// Absolute path.
    pub path: BString,
    /// The directory is a system include directory.
    pub system: bool,
    /// The directory is a framework search path (Apple `-F`).
    pub framework: bool,
}

/// One link item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdeLinkItem {
    /// A target of this project.
    Target(TargetId),
    /// A library file, by absolute path.
    File(BString),
    /// An Apple framework, by name or path.
    Framework(BString),
    /// A library name that the linker finds (`-l<name>`).
    Library(BString),
    /// A raw linker flag.
    Flag(BString),
}

/// When a custom step runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdeCustomPhase {
    /// A custom command that makes output files (`add_custom_command(OUTPUT)`).
    Rule,
    /// `PRE_BUILD` build event.
    PreBuild,
    /// `PRE_LINK` build event.
    PreLink,
    /// `POST_BUILD` build event.
    PostBuild,
}

/// One custom command or build event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeCustomStep {
    /// When the step runs.
    pub phase: IdeCustomPhase,
    /// Raw argument vectors, one per `COMMAND`.
    pub commands: Vec<Vec<BString>>,
    /// Absolute working directory.
    pub working_dir: BString,
    /// Absolute output paths.
    pub outputs: Vec<BString>,
    /// Absolute byproduct paths.
    pub byproducts: Vec<BString>,
    /// Absolute dependency paths.
    pub depends: Vec<BString>,
    /// `COMMENT` text.
    pub comment: Option<BString>,
    /// The step must run on every build.
    pub always_run: bool,
}

/// A precompiled header of one language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdePch {
    /// Language.
    pub language: BString,
    /// Absolute path of the generated header that the sources include.
    pub header: BString,
    /// Absolute path of the source that makes the precompiled header.
    pub source: BString,
    /// Target whose precompiled header this target uses (`REUSE_FROM`).
    pub reuse_from: Option<TargetId>,
}
