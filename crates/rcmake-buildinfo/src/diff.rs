//! Comparison of two build snapshots.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;

use crate::snapshot::{
    Attribution, BuildSnapshot, CacheRecord, CompileSettings, Evidence, IncludeDir, LinkFragment,
    PackageSnapshot, Section, TargetSnapshot,
};

/// Cache entries to compare.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheSet {
    /// Build-relevant `CMAKE_*` entries and project options.
    #[default]
    Key,
    /// Every entry that is not `INTERNAL` or `STATIC`.
    All,
    /// No cache entries.
    None,
}

/// Comparison options.
#[derive(Clone, Debug)]
pub struct DiffOptions {
    /// Sections to compare.
    pub sections: BTreeSet<Section>,
    /// Compare only these targets. Empty means all.
    pub targets: Vec<BString>,
    /// Cache entries to compare.
    pub cache: CacheSet,
    /// Replace build and source roots with `${build}` and `${source}`.
    pub normalize: bool,
    /// Forced configuration pair.
    pub config_pair: Option<(BString, BString)>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            sections: Section::ALL.into_iter().collect(),
            targets: Vec::new(),
            cache: CacheSet::Key,
            normalize: true,
            config_pair: None,
        }
    }
}

/// Ordered list difference with duplicates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListChange<T> {
    /// Items only in `b`, counted with duplicates.
    pub added: Vec<T>,
    /// Items only in `a`, counted with duplicates.
    pub removed: Vec<T>,
    /// The same items appear in another order.
    pub reordered: bool,
}

impl<T> ListChange<T> {
    /// Return whether the lists differ.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty() || self.reordered
    }
}

fn list_change<T: Clone + Ord>(a: &[T], b: &[T]) -> ListChange<T> {
    let mut remaining: BTreeMap<&T, usize> = BTreeMap::new();
    for item in b {
        *remaining.entry(item).or_default() += 1;
    }
    let mut removed = Vec::new();
    for item in a {
        match remaining.get_mut(item) {
            Some(count) if *count > 0 => *count -= 1,
            _ => removed.push(item.clone()),
        }
    }
    let mut seen: BTreeMap<&T, usize> = BTreeMap::new();
    for item in a {
        *seen.entry(item).or_default() += 1;
    }
    let mut added = Vec::new();
    for item in b {
        match seen.get_mut(item) {
            Some(count) if *count > 0 => *count -= 1,
            _ => added.push(item.clone()),
        }
    }
    let reordered = added.is_empty() && removed.is_empty() && a != b;
    ListChange {
        added,
        removed,
        reordered,
    }
}

/// Value pair `a -> b`.
pub type Pair<T> = (Option<T>, Option<T>);

/// Change of compile settings shared by a set of sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileChange {
    /// Sources with this exact change.
    pub sources: Vec<BString>,
    /// Language, when it changed.
    pub language: Option<Pair<BString>>,
    /// Standard, when it changed.
    pub standard: Option<Pair<BString>>,
    /// Definitions.
    pub defines: ListChange<BString>,
    /// Include directories.
    pub includes: ListChange<IncludeDir>,
    /// Remaining fragments.
    pub fragments: ListChange<BString>,
    /// A side was compared as opaque command text.
    pub opaque: bool,
}

/// Change of one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetChange {
    /// Target name.
    pub name: BString,
    /// Configuration of `a`.
    pub config_a: BString,
    /// Configuration of `b`.
    pub config_b: BString,
    /// Kind, when it changed.
    pub kind: Option<Pair<BString>>,
    /// Attribution of each side.
    pub attribution: (Attribution, Attribution),
    /// Sources only in `b`.
    pub sources_added: Vec<BString>,
    /// Sources only in `a`.
    pub sources_removed: Vec<BString>,
    /// Compile changes, grouped by identical change.
    pub compile: Vec<CompileChange>,
    /// Link fragment changes.
    pub link: Option<ListChange<LinkFragment>>,
    /// Dependency changes.
    pub dependencies: ListChange<BString>,
    /// Definition sites.
    pub backtrace: Pair<BString>,
}

/// One added or removed target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetEntry {
    /// Name.
    pub name: BString,
    /// Configuration.
    pub config: BString,
    /// Kind, when known.
    pub kind: Option<BString>,
}

/// One toolchain field change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainChange {
    /// Language.
    pub language: BString,
    /// Field name.
    pub field: &'static str,
    /// Values.
    pub values: Pair<BString>,
}

/// One package change. Each field compares one kind of evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageChange {
    /// Package name.
    pub name: BString,
    /// Changed fields: `dir`, `version_hint`, or `outcome`.
    pub fields: Vec<&'static str>,
    /// Side `a`.
    pub a: Option<PackageSnapshot>,
    /// Side `b`.
    pub b: Option<PackageSnapshot>,
}

/// One cache entry change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheChange {
    /// Entry name.
    pub name: BString,
    /// Entry type of the side that has it.
    pub ty: BString,
    /// Values.
    pub values: Pair<BString>,
}

/// Result of a comparison.
#[derive(Clone, Debug, Default)]
pub struct ConfigDiff {
    /// Paths were normalized.
    pub normalized: bool,
    /// Paired configurations.
    pub config_pairs: Vec<(BString, BString)>,
    /// Configurations without a partner: `(side, name)`.
    pub unmatched_configs: Vec<(char, BString)>,
    /// Targets only in `b`.
    pub targets_added: Vec<TargetEntry>,
    /// Targets only in `a`.
    pub targets_removed: Vec<TargetEntry>,
    /// Changed targets.
    pub targets_changed: Vec<TargetChange>,
    /// Toolchain changes.
    pub toolchain: Vec<ToolchainChange>,
    /// Package changes.
    pub packages: Vec<PackageChange>,
    /// Cache changes.
    pub cache: Vec<CacheChange>,
    /// Requested sections that could not be compared, with the reason.
    pub incomplete: Vec<(Section, BString)>,
    /// Compared sections that used partial evidence, with the reason.
    pub partial: Vec<(Section, BString)>,
}

impl ConfigDiff {
    /// Return whether any compared section differs.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        !self.unmatched_configs.is_empty()
            || !self.targets_added.is_empty()
            || !self.targets_removed.is_empty()
            || !self.targets_changed.is_empty()
            || !self.toolchain.is_empty()
            || !self.packages.is_empty()
            || !self.cache.is_empty()
    }

    /// Exit status: 1 with differences, 3 when a requested section was not compared,
    /// and 0 otherwise.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.has_changes() {
            1
        } else if self.incomplete.is_empty() {
            0
        } else {
            3
        }
    }
}

fn replace_root(value: &mut BString, root: &[u8], token: &[u8]) {
    if root.is_empty() || root == b"/" {
        return;
    }
    let mut out = Vec::with_capacity(value.len());
    let mut rest = value.as_slice();
    while let Some(at) = rest.windows(root.len()).position(|window| window == root) {
        let before_ok = at == 0
            || rest
                .get(at - 1)
                .is_some_and(|byte| matches!(byte, b' ' | b'"' | b'=' | b'I' | b',' | b':' | b';'));
        let after = rest.get(at + root.len());
        let after_ok = after.is_none_or(|byte| matches!(byte, b'/' | b' ' | b'"' | b';' | b','));
        let (head, tail) = rest.split_at(at + root.len());
        if before_ok && after_ok {
            out.extend_from_slice(head.get(..at).unwrap_or_default());
            out.extend_from_slice(token);
        } else {
            out.extend_from_slice(head);
        }
        rest = tail;
    }
    out.extend_from_slice(rest);
    *value = out.into();
}

/// Replace the roots of `snapshot`, longest root first.
pub fn normalize(snapshot: &mut BuildSnapshot) {
    let mut roots: Vec<(BString, &[u8])> = vec![(snapshot.build_root.clone(), b"${build}")];
    if let Some(source) = &snapshot.source_root {
        roots.push((source.clone(), b"${source}"));
    }
    roots.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
    let fix = |value: &mut BString| {
        for (root, token) in &roots {
            replace_root(value, root, token);
        }
    };
    let targets = std::mem::take(&mut snapshot.targets);
    for (key, mut target) in targets {
        let sources = std::mem::take(&mut target.sources);
        for (mut path, mut settings) in sources {
            fix(&mut path);
            settings
                .includes
                .iter_mut()
                .for_each(|include| fix(&mut include.path));
            settings.fragments.iter_mut().for_each(&fix);
            settings.defines.iter_mut().for_each(&fix);
            target.sources.insert(path, settings);
        }
        for fragment in target.link.iter_mut().flatten() {
            fix(&mut fragment.fragment);
        }
        target.artifacts.iter_mut().for_each(&fix);
        snapshot.targets.insert(key, target);
    }
    for record in snapshot.cache.values_mut() {
        fix(&mut record.value);
    }
}

fn key_cache_entry(name: &[u8], record: &CacheRecord) -> bool {
    const SKIP_SUFFIX: [&[u8]; 3] = [b"_BINARY_DIR", b"_SOURCE_DIR", b"_IS_TOP_LEVEL"];
    if matches!(record.ty.as_slice(), b"INTERNAL" | b"STATIC")
        || matches!(name, b"CMAKE_CACHEFILE_DIR" | b"CMAKE_HOME_DIRECTORY")
        || SKIP_SUFFIX.iter().any(|suffix| name.ends_with(suffix))
    {
        return false;
    }
    const EXACT: [&[u8]; 13] = [
        b"CMAKE_BUILD_TYPE",
        b"CMAKE_CONFIGURATION_TYPES",
        b"CMAKE_TOOLCHAIN_FILE",
        b"CMAKE_INSTALL_PREFIX",
        b"CMAKE_PREFIX_PATH",
        b"CMAKE_SYSROOT",
        b"CMAKE_MSVC_RUNTIME_LIBRARY",
        b"CMAKE_POSITION_INDEPENDENT_CODE",
        b"CMAKE_INTERPROCEDURAL_OPTIMIZATION",
        b"BUILD_SHARED_LIBS",
        b"CMAKE_C_COMPILER",
        b"CMAKE_CXX_COMPILER",
        b"CMAKE_LINKER",
    ];
    if EXACT.contains(&name)
        || name.starts_with(b"CMAKE_GENERATOR")
        || name.starts_with(b"CMAKE_OSX_")
    {
        return true;
    }
    if let Some(rest) = name.strip_prefix(b"CMAKE_") {
        let flags = rest.windows(6).any(|part| part == b"_FLAGS")
            && (rest.ends_with(b"_FLAGS") || rest.windows(7).any(|part| part == b"_FLAGS_"));
        let compiler = rest.ends_with(b"_COMPILER");
        return flags || compiler;
    }
    matches!(
        record.ty.as_slice(),
        b"BOOL" | b"STRING" | b"PATH" | b"FILEPATH"
    )
}

fn pair_configs(
    a: &BuildSnapshot,
    b: &BuildSnapshot,
    options: &DiffOptions,
    diff: &mut ConfigDiff,
) {
    if let Some((left, right)) = &options.config_pair {
        diff.config_pairs.push((left.clone(), right.clone()));
        return;
    }
    if let ([left], [right]) = (a.configs.as_slice(), b.configs.as_slice()) {
        diff.config_pairs.push((left.clone(), right.clone()));
        return;
    }
    for config in &a.configs {
        if b.configs
            .iter()
            .any(|other| other.eq_ignore_ascii_case(config))
        {
            let other = b
                .configs
                .iter()
                .find(|other| other.eq_ignore_ascii_case(config))
                .cloned()
                .unwrap_or_default();
            diff.config_pairs.push((config.clone(), other));
        } else {
            diff.unmatched_configs.push(('a', config.clone()));
        }
    }
    for config in &b.configs {
        if !a
            .configs
            .iter()
            .any(|other| other.eq_ignore_ascii_case(config))
        {
            diff.unmatched_configs.push(('b', config.clone()));
        }
    }
}

fn settings_change(
    sources: Vec<BString>,
    a: &CompileSettings,
    b: &CompileSettings,
) -> CompileChange {
    let differs =
        |x: &Option<BString>, y: &Option<BString>| (x != y).then(|| (x.clone(), y.clone()));
    let opaque = a.opaque || b.opaque;
    CompileChange {
        sources,
        language: differs(&a.language, &b.language),
        standard: differs(&a.standard, &b.standard),
        defines: list_change(&a.defines, &b.defines),
        includes: list_change(&a.includes, &b.includes),
        fragments: list_change(&a.fragments, &b.fragments),
        opaque,
    }
}

fn target_change(
    name: &BString,
    pair: &(BString, BString),
    a: &TargetSnapshot,
    b: &TargetSnapshot,
    options: &DiffOptions,
) -> Option<TargetChange> {
    let compile_on = options.sections.contains(&Section::Compile);
    let link_on = options.sections.contains(&Section::Link);
    let mut groups: Vec<((CompileSettings, CompileSettings), Vec<BString>)> = Vec::new();
    let mut sources_added = Vec::new();
    let mut sources_removed = Vec::new();
    if compile_on {
        for (path, left) in &a.sources {
            match b.sources.get(path) {
                Some(right) if right != left => {
                    match groups
                        .iter_mut()
                        .find(|(key, _)| key.0 == *left && key.1 == *right)
                    {
                        Some((_, sources)) => sources.push(path.clone()),
                        None => groups.push(((left.clone(), right.clone()), vec![path.clone()])),
                    }
                }
                Some(_) => {}
                None => sources_removed.push(path.clone()),
            }
        }
        sources_added = b
            .sources
            .keys()
            .filter(|path| !a.sources.contains_key(*path))
            .cloned()
            .collect();
    }
    let compile: Vec<CompileChange> = groups
        .into_iter()
        .map(|((left, right), sources)| settings_change(sources, &left, &right))
        .collect();
    let link = if link_on {
        match (&a.link, &b.link) {
            (Some(left), Some(right)) => Some(list_change(left, right)).filter(ListChange::changed),
            (None, Some(right)) => Some(list_change(&[], right)).filter(ListChange::changed),
            (Some(left), None) => Some(list_change(left, &[])).filter(ListChange::changed),
            (None, None) => None,
        }
    } else {
        None
    };
    let dependencies = if link_on {
        let left: Vec<BString> = a.dependencies.iter().cloned().collect();
        let right: Vec<BString> = b.dependencies.iter().cloned().collect();
        list_change(&left, &right)
    } else {
        ListChange::default()
    };
    let kind = (options.sections.contains(&Section::Targets)
        && a.kind.is_some()
        && b.kind.is_some()
        && a.kind != b.kind)
        .then(|| (a.kind.clone(), b.kind.clone()));
    let changed = kind.is_some()
        || !compile.is_empty()
        || !sources_added.is_empty()
        || !sources_removed.is_empty()
        || link.is_some()
        || dependencies.changed();
    changed.then(|| TargetChange {
        name: name.clone(),
        config_a: pair.0.clone(),
        config_b: pair.1.clone(),
        kind,
        attribution: (a.attribution, b.attribution),
        sources_added,
        sources_removed,
        compile,
        link,
        dependencies,
        backtrace: (a.backtrace.clone(), b.backtrace.clone()),
    })
}

fn compare_targets(
    a: &BuildSnapshot,
    b: &BuildSnapshot,
    options: &DiffOptions,
    diff: &mut ConfigDiff,
) {
    let wanted = |name: &BString| options.targets.is_empty() || options.targets.contains(name);
    for pair in diff.config_pairs.clone() {
        let left: BTreeMap<&BString, &TargetSnapshot> = a
            .targets
            .iter()
            .filter(|((config, name), _)| *config == pair.0 && wanted(name))
            .map(|((_, name), target)| (name, target))
            .collect();
        let right: BTreeMap<&BString, &TargetSnapshot> = b
            .targets
            .iter()
            .filter(|((config, name), _)| *config == pair.1 && wanted(name))
            .map(|((_, name), target)| (name, target))
            .collect();
        if options.sections.contains(&Section::Targets) {
            for (name, target) in &right {
                if !left.contains_key(name) {
                    diff.targets_added.push(TargetEntry {
                        name: (*name).clone(),
                        config: pair.1.clone(),
                        kind: target.kind.clone(),
                    });
                }
            }
            for (name, target) in &left {
                if !right.contains_key(name) {
                    diff.targets_removed.push(TargetEntry {
                        name: (*name).clone(),
                        config: pair.0.clone(),
                        kind: target.kind.clone(),
                    });
                }
            }
        }
        for (name, target) in &left {
            if let Some(other) = right.get(name)
                && let Some(change) = target_change(name, &pair, target, other, options)
            {
                diff.targets_changed.push(change);
            }
        }
    }
}

type ToolchainField = fn(&crate::snapshot::ToolchainSnapshot) -> Option<BString>;

fn compare_toolchains(a: &BuildSnapshot, b: &BuildSnapshot, diff: &mut ConfigDiff) {
    let languages: BTreeSet<&BString> = a
        .toolchains
        .iter()
        .chain(&b.toolchains)
        .map(|toolchain| &toolchain.language)
        .collect();
    for language in languages {
        let left = a
            .toolchains
            .iter()
            .find(|toolchain| &toolchain.language == language);
        let right = b
            .toolchains
            .iter()
            .find(|toolchain| &toolchain.language == language);
        let fields: [(&'static str, ToolchainField); 4] = [
            ("id", |t| t.compiler_id.clone()),
            ("version", |t| t.version.clone()),
            ("path", |t| t.path.clone()),
            ("target", |t| t.target.clone()),
        ];
        for (field, get) in fields {
            let values = (left.and_then(get), right.and_then(get));
            if values.0 != values.1 {
                diff.toolchain.push(ToolchainChange {
                    language: language.clone(),
                    field,
                    values,
                });
            }
        }
    }
}

fn compare_packages(a: &BuildSnapshot, b: &BuildSnapshot, diff: &mut ConfigDiff) {
    let names: BTreeSet<&BString> = a.packages.keys().chain(b.packages.keys()).collect();
    for name in names {
        let left = a.packages.get(name);
        let right = b.packages.get(name);
        let mut fields = Vec::new();
        let dir =
            |side: Option<&PackageSnapshot>| side.and_then(|package| package.cached_dir.clone());
        if dir(left) != dir(right) {
            fields.push("dir");
        }
        let hint =
            |side: Option<&PackageSnapshot>| side.and_then(|package| package.version_hint.clone());
        if hint(left) != hint(right) {
            fields.push("version_hint");
        }
        let observed = |side: Option<&PackageSnapshot>| {
            side.map(|package| package.observed.clone())
                .unwrap_or_default()
        };
        let (left_seen, right_seen) = (observed(left), observed(right));
        let known = |snapshot: &BuildSnapshot, seen: &[crate::snapshot::PackageObservation]| {
            !seen.is_empty()
                || (!snapshot.packages_unknown && snapshot.evidence(Section::Packages).complete())
        };
        if known(a, &left_seen) && known(b, &right_seen) && left_seen != right_seen {
            fields.push("outcome");
        }
        if !fields.is_empty() {
            diff.packages.push(PackageChange {
                name: name.clone(),
                fields,
                a: left.cloned(),
                b: right.cloned(),
            });
        }
    }
}

fn compare_cache(a: &BuildSnapshot, b: &BuildSnapshot, set: CacheSet, diff: &mut ConfigDiff) {
    if set == CacheSet::None {
        return;
    }
    let include = |name: &[u8], record: &CacheRecord| match set {
        CacheSet::Key => key_cache_entry(name, record),
        CacheSet::All => !matches!(record.ty.as_slice(), b"INTERNAL" | b"STATIC"),
        CacheSet::None => false,
    };
    let names: BTreeSet<&BString> = a.cache.keys().chain(b.cache.keys()).collect();
    for name in names {
        let left = a.cache.get(name).filter(|record| include(name, record));
        let right = b.cache.get(name).filter(|record| include(name, record));
        if left.is_none() && right.is_none() {
            continue;
        }
        let values = (
            left.map(|record| record.value.clone()),
            right.map(|record| record.value.clone()),
        );
        if values.0 != values.1 {
            let ty = left
                .or(right)
                .map(|record| record.ty.clone())
                .unwrap_or_default();
            diff.cache.push(CacheChange {
                name: name.clone(),
                ty,
                values,
            });
        }
    }
}

/// Compare two snapshots.
#[must_use]
pub fn compare(a: &BuildSnapshot, b: &BuildSnapshot, options: &DiffOptions) -> ConfigDiff {
    let (mut a, mut b) = (a.clone(), b.clone());
    if options.normalize {
        normalize(&mut a);
        normalize(&mut b);
    }
    let mut diff = ConfigDiff {
        normalized: options.normalize,
        ..ConfigDiff::default()
    };
    let comparable = |section: Section, diff: &mut ConfigDiff| {
        if !options.sections.contains(&section) {
            return false;
        }
        for evidence in [a.evidence(section), b.evidence(section)] {
            match evidence {
                Evidence::Unavailable(reason) => {
                    diff.incomplete.push((section, reason));
                    return false;
                }
                Evidence::Partial(_, reason) => diff.partial.push((section, reason)),
                Evidence::Complete(_) => {}
            }
        }
        true
    };
    let targets = comparable(Section::Targets, &mut diff);
    let compile = comparable(Section::Compile, &mut diff);
    let link = comparable(Section::Link, &mut diff);
    let toolchain = comparable(Section::Toolchain, &mut diff);
    let packages = comparable(Section::Packages, &mut diff);
    let cache = comparable(Section::Cache, &mut diff);
    if targets || compile || link {
        pair_configs(&a, &b, options, &mut diff);
        let mut gated = options.clone();
        gated.sections.retain(|section| match section {
            Section::Targets => targets,
            Section::Compile => compile,
            Section::Link => link,
            _ => true,
        });
        compare_targets(&a, &b, &gated, &mut diff);
    }
    if toolchain {
        compare_toolchains(&a, &b, &mut diff);
    }
    if packages {
        compare_packages(&a, &b, &mut diff);
    }
    if cache {
        compare_cache(&a, &b, options.cache, &mut diff);
    }
    diff.partial.sort();
    diff.partial.dedup();
    diff
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;
