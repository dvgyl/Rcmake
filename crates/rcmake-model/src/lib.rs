//! CMake cache store (`CMakeCache.txt`), property maps, global state, and the arena-based
//! [`ProjectModel`] (ADR-007).
//!
//! The project model stores directories, targets, custom commands, tests, file sets,
//! install rules, and export sets from configuration. Each arena hands out a
//! typed id ([`DirId`], [`TargetId`], [`CommandId`], [`TestId`], [`FileSetId`]). Ids are
//! only created by the arenas, and arenas never shrink.

#![forbid(unsafe_code)]

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::Backtrace;

mod command;
mod directory;
mod export_dependency;
mod generator;
mod glob;
mod ids;
mod install;
mod property_print;
mod vars;
pub use property_print::PropertyPrint;
mod project;
mod props;
mod provenance;
pub use provenance::{LanguageRegistry, PropEvent, PropLedger, PropOp, PropValuePreview};
mod target;

pub use command::{CustomCommand, CustomCommandKind, TargetEvent};
pub use directory::{Directory, SourceGroupDef};
pub use export_dependency::{ExportDependencyMode, ExportDependencySetup};
pub use generator::{
    GeneratorKind, GeneratorSpec, GeneratorSpecError, ParsedToolset, SpecField, ToolsetSupport,
    VsPlatform, VsToolset, VsVersion, XcodeBuildSystem, XcodeVersion, parse_vs_platform,
    parse_vs_toolset, parse_xcode_toolset,
};
pub use glob::{GlobCall, GlobCheck, GlobKey};
pub use ids::{BT, CommandId, DirId, FileSetId, TargetId, TestId};
pub use install::{
    BuildExport, DiscoverySpec, ExportKind, ExportSet, InstallKind, InstallOptions, InstallRule,
    PackageInfoSpec, ProjectMetadata, RuntimeDependencyFilters, RuntimeDependencyInstall, SbomSpec,
    TestEntry, TestKind,
};
pub use project::{ModelError, ProjectModel, replay};
pub use props::{AppendMode, PropOrigin, PropScope};
pub use target::{
    FileSet, FileSetKind, ImportedVisibility, LinkItem, Target, TargetBinding, TargetKind,
    TargetOrigin, TargetPolicySnapshot, UsageField, UsageProps, UsageScope,
};
pub use vars::{Binding, VarLayer, VarSnapshot};

/// Cache entry type matching `CMakeCache.txt` (`<NAME>:<TYPE>=<VALUE>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheType {
    /// Boolean option (`BOOL`).
    Bool,
    /// Directory path (`PATH`).
    Path,
    /// File path (`FILEPATH`).
    Filepath,
    /// Arbitrary string (`STRING`).
    String,
    /// Internal cache variable (`INTERNAL`).
    Internal,
    /// Read-only static entry (`STATIC`).
    Static,
    /// Uninitialized CLI `-DVAR=val` without explicit type (`UNINITIALIZED`).
    Uninitialized,
}

impl CacheType {
    /// Parse a CMake cache type token (case-insensitive).
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        if name.eq_ignore_ascii_case(b"BOOL") {
            Some(Self::Bool)
        } else if name.eq_ignore_ascii_case(b"PATH") {
            Some(Self::Path)
        } else if name.eq_ignore_ascii_case(b"FILEPATH") {
            Some(Self::Filepath)
        } else if name.eq_ignore_ascii_case(b"STRING") {
            Some(Self::String)
        } else if name.eq_ignore_ascii_case(b"INTERNAL") {
            Some(Self::Internal)
        } else if name.eq_ignore_ascii_case(b"STATIC") {
            Some(Self::Static)
        } else if name.eq_ignore_ascii_case(b"UNINITIALIZED") {
            Some(Self::Uninitialized)
        } else {
            None
        }
    }

    /// Canonical uppercase spelling written into `CMakeCache.txt`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::Path => "PATH",
            Self::Filepath => "FILEPATH",
            Self::String => "STRING",
            Self::Internal => "INTERNAL",
            Self::Static => "STATIC",
            Self::Uninitialized => "UNINITIALIZED",
        }
    }
}

/// Deterministic ordered map of CMake property names to byte values.
#[derive(Clone, Debug, Default)]
pub struct PropertyMap {
    entries: IndexMap<BString, BString>,
    tracked: Option<Box<IndexMap<BString, Vec<BT<BString>>>>>,
}

impl PartialEq for PropertyMap {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for PropertyMap {}

impl PropertyMap {
    /// Read a property value.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&BString> {
        self.entries.get(name)
    }

    /// Set a property to `value`. Direct writes invalidate any tracked backtrace entries.
    pub fn set(&mut self, name: BString, value: BString) {
        if let Some(tracked) = &mut self.tracked {
            tracked.shift_remove(&name);
        }
        self.entries.insert(name, value);
    }

    /// Remove a property and any tracked backtrace entries.
    pub fn unset(&mut self, name: &[u8]) {
        if let Some(tracked) = &mut self.tracked {
            tracked.shift_remove(name);
        }
        self.entries.shift_remove(name);
    }

    /// Append a semicolon-separated list item (`set_property(... APPEND)`).
    pub fn append(&mut self, name: BString, value: &[u8]) {
        if let Some(tracked) = &mut self.tracked {
            tracked.shift_remove(&name);
        }
        if let Some(existing) = self.entries.get_mut(&name) {
            if !existing.is_empty() && !value.is_empty() {
                existing.push(b';');
            }
            existing.extend_from_slice(value);
        } else {
            self.entries.insert(name, BString::from(value));
        }
    }

    /// Append raw text without a semicolon (`set_property(... APPEND_STRING)`).
    pub fn append_string(&mut self, name: BString, value: &[u8]) {
        if let Some(tracked) = &mut self.tracked {
            tracked.shift_remove(&name);
        }
        if let Some(existing) = self.entries.get_mut(&name) {
            existing.extend_from_slice(value);
        } else {
            self.entries.insert(name, BString::from(value));
        }
    }

    /// Set a property while recording its single backtrace-tagged entry.
    pub fn set_tracked(&mut self, name: BString, value: BString, backtrace: Backtrace) {
        self.entries.insert(name.clone(), value.clone());
        self.tracked
            .get_or_insert_with(Default::default)
            .insert(name, vec![BT::new(value, backtrace)]);
    }

    /// Append a non-empty backtrace-tagged entry and keep the joined byte value in sync.
    pub fn append_tracked(&mut self, name: BString, value: &[u8], backtrace: Backtrace) {
        if value.is_empty() {
            return;
        }
        let previous = self.entries.get(&name).cloned();
        let entries = self
            .tracked
            .get_or_insert_with(Default::default)
            .entry(name.clone())
            .or_insert_with(|| {
                previous
                    .filter(|existing| !existing.is_empty())
                    .map(|existing| BT::new(existing, Backtrace::default()))
                    .into_iter()
                    .collect()
            });
        entries.push(BT::new(BString::from(value), backtrace));
        let joined = rcmake_base::list::join(entries.iter().map(|entry| entry.value.as_slice()));
        self.entries.insert(name, joined);
    }

    /// Return ordered backtrace-tagged entries recorded for `name`, if tracked.
    #[must_use]
    pub fn tracked_entries(&self, name: &[u8]) -> Option<&[BT<BString>]> {
        self.tracked
            .as_deref()
            .and_then(|map| map.get(name))
            .map(Vec::as_slice)
    }

    /// Return all property names in insertion order.
    #[must_use]
    pub fn names(&self) -> Vec<BString> {
        self.entries.keys().cloned().collect()
    }

    /// Total bytes of all property names and values.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.entries
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()))
            .fold(0, usize::saturating_add)
    }
}

/// Single entry inside [`Cache`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    /// Entry byte value.
    pub value: BString,
    /// Cache entry type.
    pub ty: CacheType,
    /// Help docstring (`HELPSTRING`).
    pub help: BString,
    /// Whether marked `ADVANCED`.
    pub advanced: bool,
    /// Additional cache entry properties (`STRINGS`, `MODIFIED`, etc.).
    pub props: PropertyMap,
}

/// Fixed cost of one cache entry for [`Cache::bytes`] (map slot and string headers).
const CACHE_ENTRY_OVERHEAD: usize = 128;

/// Estimated bytes of one cache entry with a name of `name_len` bytes: name, value, help
/// text, additional properties (such as `STRINGS`), and a fixed overhead.
fn cache_entry_cost(name_len: usize, entry: &CacheEntry) -> usize {
    name_len
        .saturating_add(entry.value.len())
        .saturating_add(entry.props.byte_len())
        .saturating_add(entry.help.len())
        .saturating_add(CACHE_ENTRY_OVERHEAD)
}

/// Ordered store of CMake cache variables (`CMakeCache.txt`).
#[derive(Clone, Debug, Default)]
pub struct Cache {
    entries: IndexMap<BString, CacheEntry>,
    /// Sum of the entry costs while `dirty` is false.
    bytes: usize,
    /// [`Cache::get_mut`] gave out an entry, so `bytes` must be counted again.
    dirty: bool,
    /// Reservation floors of pending inserts: each key keeps at least its largest floor,
    /// even if its live entry shrinks or disappears before the insert.
    reservations: Vec<Reservation>,
    /// Id of the next reservation.
    next_reservation: u64,
}

/// Identity of one [`Cache::reserve`] call, which only [`Cache::release`] of the same id
/// ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservationId(u64);

#[derive(Clone, Debug)]
struct Reservation {
    id: ReservationId,
    key: BString,
    floor: usize,
}

impl PartialEq for Cache {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for Cache {}

impl Cache {
    /// Estimated bytes of all entries (name, value, help text, and a fixed overhead).
    pub fn bytes(&mut self) -> usize {
        if self.dirty {
            self.bytes = self
                .entries
                .iter()
                .map(|(name, entry)| cache_entry_cost(name.len(), entry))
                .fold(0, usize::saturating_add);
            self.dirty = false;
        }
        self.bytes
    }

    /// Estimated bytes of all entries and reservations after [`Cache::insert`] of `entry`
    /// as `name`.
    pub fn bytes_after_insert(&mut self, name: &[u8], entry: &CacheEntry) -> usize {
        let replaced = self.live_cost(name);
        let added = cache_entry_cost(name.len(), entry);
        let bytes = self.bytes();
        let reserved = self.reserved_beyond(|key| {
            if key == name {
                added
            } else {
                self.live_cost(key)
            }
        });
        bytes
            .saturating_sub(replaced)
            .saturating_add(added)
            .saturating_add(reserved)
    }

    /// Bytes that reservations hold beyond the live entries of their keys.
    #[must_use]
    pub fn reserved_bytes(&self) -> usize {
        self.reserved_beyond(|key| self.live_cost(key))
    }

    /// Reserve the cost of `entry` as a floor for `name` until [`Cache::release`] of the
    /// returned id. While the reservation lasts, `name` counts at least its largest floor,
    /// also if a write shrinks or removes its live entry, so that other writes cannot use
    /// the bytes.
    pub fn reserve(&mut self, name: &[u8], entry: &CacheEntry) -> ReservationId {
        let id = ReservationId(self.next_reservation);
        self.next_reservation = self.next_reservation.wrapping_add(1);
        self.reservations.push(Reservation {
            id,
            key: name.into(),
            floor: cache_entry_cost(name.len(), entry),
        });
        id
    }

    /// Release the reservation `id`. Other reservations of the same key stay.
    pub fn release(&mut self, id: ReservationId) {
        self.reservations.retain(|reservation| reservation.id != id);
    }

    /// Sum, over the reserved keys, of the largest floor beyond the `live` cost of the key.
    /// Each key counts once.
    fn reserved_beyond(&self, live: impl Fn(&[u8]) -> usize) -> usize {
        let mut floors: IndexMap<&[u8], usize> = IndexMap::new();
        for reservation in &self.reservations {
            let floor = floors.entry(reservation.key.as_slice()).or_default();
            *floor = (*floor).max(reservation.floor);
        }
        floors
            .into_iter()
            .map(|(key, floor)| floor.saturating_sub(live(key)))
            .fold(0, usize::saturating_add)
    }

    /// Estimated bytes of the live entry of `name`, or zero.
    fn live_cost(&self, name: &[u8]) -> usize {
        self.entries
            .get(name)
            .map_or(0, |entry| cache_entry_cost(name.len(), entry))
    }

    /// Get a cache entry by name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&CacheEntry> {
        self.entries.get(name)
    }

    /// Get a mutable reference to a cache entry by name.
    pub fn get_mut(&mut self, name: &[u8]) -> Option<&mut CacheEntry> {
        let entry = self.entries.get_mut(name);
        self.dirty |= entry.is_some();
        entry
    }

    /// Insert or replace a cache entry.
    pub fn insert(&mut self, name: BString, entry: CacheEntry) {
        let name_len = name.len();
        let added = cache_entry_cost(name_len, &entry);
        let removed = self
            .entries
            .insert(name, entry)
            .map_or(0, |old| cache_entry_cost(name_len, &old));
        self.bytes = self.bytes.saturating_sub(removed).saturating_add(added);
    }

    /// Remove a cache entry (`unset(VAR CACHE)`).
    pub fn remove(&mut self, name: &[u8]) -> Option<CacheEntry> {
        let removed = self.entries.shift_remove(name);
        if let Some(entry) = &removed {
            self.bytes = self
                .bytes
                .saturating_sub(cache_entry_cost(name.len(), entry));
        }
        removed
    }

    /// Return `true` if a cache entry with `name` exists.
    #[must_use]
    pub fn contains(&self, name: &[u8]) -> bool {
        self.entries.contains_key(name)
    }

    /// Iterate over all cache entries in deterministic insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&BString, &CacheEntry)> {
        self.entries.iter()
    }
}

/// Property definition registered via `define_property()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinedProperty {
    /// Scope kind (`GLOBAL`, `DIRECTORY`, `TARGET`, `SOURCE`, `TEST`, `VARIABLE`, `CACHED_VARIABLE`).
    pub scope: BString,
    /// Property name.
    pub name: BString,
    /// Whether the property inherits from parent scopes (`INHERITED`).
    pub inherited: bool,
    /// Brief documentation string.
    pub brief_docs: BString,
    /// Full documentation string.
    pub full_docs: BString,
    /// Variable that initializes the property on target creation
    /// (`INITIALIZE_FROM_VARIABLE`, `TARGET` scope only).
    pub initialize_from_variable: Option<BString>,
}

/// One `CUSTOM_CONTENT <name> <type> <content>` entry of `cmake_instrumentation()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstrumentationContent {
    /// Content name.
    pub name: BString,
    /// Content type keyword (`STRING`, `BOOL`, `LIST`, or `JSON`).
    pub ty: BString,
    /// Raw content.
    pub content: BString,
}

/// One `cmake_instrumentation()` call, after argument parsing. `rcmake-instrument`
/// writes it as a query file under `<build>/.cmake/instrumentation/v1/query/generated`.
#[derive(Clone, Debug, Default)]
pub struct InstrumentationRequest {
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// `API_VERSION`.
    pub api_version: BString,
    /// `DATA_VERSION` (`major` or `major.minor`).
    pub data_version: BString,
    /// `HOOKS` (for example `postGenerate`, `postCMakeBuild`).
    pub hooks: Vec<BString>,
    /// `OPTIONS` (for example `staticSystemInformation`, `trace`).
    pub options: Vec<BString>,
    /// `CALLBACK` command lines, one for each keyword.
    pub callbacks: Vec<BString>,
    /// `CUSTOM_CONTENT` entries.
    pub custom_content: Vec<InstrumentationContent>,
}

/// Global properties and defined-property metadata shared across the build/script session.
#[derive(Clone, Debug, Default)]
pub struct GlobalState {
    /// Global properties (`set_property(GLOBAL ...)`).
    pub props: PropertyMap,
    /// Properties registered via `define_property()`.
    pub defined_props: Vec<DefinedProperty>,
    /// `cmake_instrumentation()` calls in call order.
    pub instrumentation: Vec<InstrumentationRequest>,
}

impl GlobalState {
    /// The `define_property()` entry for `name` in `scope`, if any.
    #[must_use]
    pub fn defined_property(&self, scope: &[u8], name: &[u8]) -> Option<&DefinedProperty> {
        self.defined_props
            .iter()
            .find(|entry| entry.scope == scope && entry.name == name)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::{Cache, CacheEntry, CacheType, PropertyMap};

    fn entry(value: &str) -> CacheEntry {
        CacheEntry {
            value: value.into(),
            ty: CacheType::Static,
            help: Default::default(),
            advanced: false,
            props: PropertyMap::default(),
        }
    }

    #[test]
    fn reservations_of_one_key_count_their_largest_floor_once() {
        let mut cache = Cache::default();
        let outer = cache.reserve(b"K", &entry(&"x".repeat(500)));
        let inner = cache.reserve(b"K", &entry(""));
        let large = cache.reserved_bytes();
        assert_eq!(large, 1 + 500 + 128);

        // The nested release keeps the outer floor, also after a small insert.
        cache.release(inner);
        cache.insert("K".into(), entry(""));
        assert_eq!(cache.reserved_bytes(), large - (1 + 128));
        assert_eq!(cache.bytes_after_insert(b"L", &entry("")), large + 1 + 128);

        cache.release(outer);
        assert_eq!(cache.reserved_bytes(), 0);
    }
}
