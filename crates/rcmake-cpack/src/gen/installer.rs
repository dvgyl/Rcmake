//! Shared infrastructure for native CPack installer generators (`productbuild`, `NSIS`,
//! `NSIS64`, `WIX`, and `IFW`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, Limits, truth};
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest,
    Stream, WriteMode,
};

use crate::config::{CpackConfig, absolute};
use crate::source::{
    SymlinkPolicy, canonical_checked_root, copy_checked_tree_expected, verify_contained_path,
    verify_expected_root,
};
use crate::{CpackError, InstalledComponent, error, safe_name};

#[cfg(test)]
mod tests;

/// Maximum captured output size from an external installer tool (16 MiB).
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Default timeout for version probes (30 seconds).
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for installer build tool invocations (300 seconds).
pub(crate) const BUILD_TIMEOUT: Duration = Duration::from_secs(300);
/// Maximum allowed nesting depth for CPack component group hierarchies.
pub(crate) const MAX_COMPONENT_GROUP_DEPTH: usize = 256;

const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 128;
const MAX_XML_NODES: usize = 65_536;
const MAX_TEMPLATE_BYTES: u64 = 16 * 1024 * 1024;

/// Runtime host platform classified from [`Host::system`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostPlatform {
    MacOs,
    Linux,
    Windows,
    OtherUnix,
}

/// Classify the runtime platform from `host.system().sysname`.
pub(crate) fn host_platform(host: &dyn Host) -> HostPlatform {
    classify_sysname(&host.system().sysname)
}

pub(crate) fn classify_sysname(sysname: &[u8]) -> HostPlatform {
    let lower = sysname.to_ascii_lowercase();
    if lower == b"darwin" || lower.starts_with(b"macos") {
        HostPlatform::MacOs
    } else if lower == b"linux" {
        HostPlatform::Linux
    } else if lower.starts_with(b"windows")
        || lower.starts_with(b"mingw")
        || lower.starts_with(b"msys")
        || lower.starts_with(b"cygwin")
    {
        HostPlatform::Windows
    } else {
        HostPlatform::OtherUnix
    }
}

pub(crate) fn is_windows_host(host: &dyn Host) -> bool {
    host_platform(host) == HostPlatform::Windows
}

pub(crate) fn is_macos_host(host: &dyn Host) -> bool {
    host_platform(host) == HostPlatform::MacOs
}

/// Return whether a CPack variable is defined, non-empty, and not `NOTFOUND`/`*-NOTFOUND`.
pub(crate) fn option_set<'a>(cfg: &'a CpackConfig, key: &[u8]) -> Option<&'a [u8]> {
    cfg.get(key)
        .filter(|v| !v.is_empty() && *v != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND"))
}

/// Return whether a variable is defined and explicitly set to an empty string.
pub(crate) fn is_set_to_empty(cfg: &CpackConfig, key: &[u8]) -> bool {
    cfg.get(key).is_some_and(<[u8]>::is_empty)
}

/// Return whether a variable is defined, non-empty, and CMake-falsy.
pub(crate) fn is_set_to_off(cfg: &CpackConfig, key: &[u8]) -> bool {
    cfg.get(key)
        .is_some_and(|v| !v.is_empty() && truth::is_off(v))
}

/// Convert a CMake byte value to UTF-8, rejecting invalid UTF-8 or NUL bytes.
pub(crate) fn require_utf8<'a>(bytes: &'a [u8], label: &str) -> Result<&'a str, CpackError> {
    if bytes.contains(&0) {
        return Err(error(format!("{label} contains a NUL byte")));
    }
    std::str::from_utf8(bytes).map_err(|_| error(format!("{label} must be valid UTF-8")))
}

/// Metadata for one CPack installation type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallTypeMeta {
    pub(crate) name: BString,
    pub(crate) display_name: BString,
    pub(crate) index: usize,
}

/// Metadata for one CPack component group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupMeta {
    pub(crate) name: BString,
    pub(crate) display_name: BString,
    pub(crate) description: BString,
    pub(crate) parent_group: Option<BString>,
    pub(crate) expanded: bool,
    pub(crate) bold_title: bool,
    pub(crate) subgroups: Vec<BString>,
    pub(crate) components: Vec<BString>,
}

/// Metadata for one CPack component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComponentMeta {
    pub(crate) name: BString,
    pub(crate) display_name: BString,
    pub(crate) description: BString,
    pub(crate) group: Option<BString>,
    pub(crate) depends: Vec<BString>,
    pub(crate) reverse_depends: Vec<BString>,
    pub(crate) install_types: Vec<BString>,
    pub(crate) required: bool,
    pub(crate) hidden: bool,
    pub(crate) disabled: bool,
    pub(crate) downloaded: bool,
    pub(crate) archive_file: Option<BString>,
    pub(crate) plist: Option<BString>,
}

/// Validated catalog of components, groups, and installation types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ComponentCatalog {
    pub(crate) components: BTreeMap<BString, ComponentMeta>,
    pub(crate) groups: BTreeMap<BString, GroupMeta>,
    pub(crate) install_types: BTreeMap<BString, InstallTypeMeta>,
}

impl ComponentCatalog {
    /// Build and validate the catalog from `cfg` and staged `members`.
    ///
    /// Validates all component dependency references, component dependency cycles, and
    /// component-group parent cycles before any generator renders tool inputs.
    pub(crate) fn from_config(
        cfg: &CpackConfig,
        members: &[InstalledComponent],
    ) -> Result<Self, CpackError> {
        let mut catalog = Self::default();
        let mut names = BTreeSet::new();
        for member in members {
            names.insert(member.name.clone());
        }
        for [_, _, selection, _] in cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS").as_chunks::<4>().0 {
            for name in crate::stage::package_components(cfg, selection) {
                names.insert(name);
            }
            if !cfg.on(b"CPACK_MONOLITHIC_INSTALL") {
                let key = format!(
                    "CPACK_{}_INSTALL_TYPES",
                    BString::from(selection.to_ascii_uppercase())
                );
                for ty in cfg.list(key.as_bytes()) {
                    catalog.register_install_type(cfg, &ty);
                }
            }
        }
        if names.is_empty() {
            return Ok(catalog);
        }

        for name in &names {
            let upper = BString::from(name.to_ascii_uppercase());
            let prefix = format!("CPACK_COMPONENT_{upper}_");
            let get = |suffix: &str| cfg.get(format!("{prefix}{suffix}").as_bytes());
            let on = |suffix: &str| get(suffix).is_some_and(truth::is_on);
            let list = |suffix: &str| cfg.list(format!("{prefix}{suffix}").as_bytes());

            let display_name = get("DISPLAY_NAME")
                .filter(|v| !v.is_empty())
                .map_or_else(|| name.clone(), BString::from);
            let description = get("DESCRIPTION").map_or_else(BString::default, BString::from);
            let group = get("GROUP").filter(|v| !v.is_empty()).map(BString::from);
            let depends = list("DEPENDS");
            let install_types = list("INSTALL_TYPES");
            for ty in &install_types {
                catalog.register_install_type(cfg, ty);
            }
            if let Some(ref group_name) = group {
                catalog.register_group_chain(cfg, group_name)?;
                if let Some(g) = catalog.groups.get_mut(group_name)
                    && !g.components.contains(name)
                {
                    g.components.push(name.clone());
                }
            }
            let archive_file = get("ARCHIVE_FILE")
                .filter(|v| !v.is_empty())
                .map(BString::from);
            let plist = get("PLIST").filter(|v| !v.is_empty()).map(BString::from);

            catalog.components.insert(
                name.clone(),
                ComponentMeta {
                    name: name.clone(),
                    display_name,
                    description,
                    group,
                    depends,
                    reverse_depends: Vec::new(),
                    install_types,
                    required: on("REQUIRED"),
                    hidden: on("HIDDEN"),
                    disabled: on("DISABLED"),
                    downloaded: on("DOWNLOADED"),
                    archive_file,
                    plist,
                },
            );
        }

        let dep_edges: Vec<(BString, BString)> = catalog
            .components
            .values()
            .flat_map(|comp| {
                comp.depends
                    .iter()
                    .map(|dep| (comp.name.clone(), dep.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (comp_name, dep_name) in dep_edges {
            if !catalog.components.contains_key(&dep_name) {
                return Err(error(format!(
                    "CPack component \"{comp_name}\" depends on unknown component \"{dep_name}\""
                )));
            }
            if let Some(target) = catalog.components.get_mut(&dep_name)
                && !target.reverse_depends.contains(&comp_name)
            {
                target.reverse_depends.push(comp_name);
            }
        }

        catalog.check_component_cycles()?;
        catalog.check_group_hierarchy()?;

        Ok(catalog)
    }

    fn register_install_type(&mut self, cfg: &CpackConfig, name: &BString) {
        if self.install_types.contains_key(name) {
            return;
        }
        let index = self.install_types.len() + 1;
        let upper = BString::from(name.to_ascii_uppercase());
        let key = format!("CPACK_INSTALL_TYPE_{upper}_DISPLAY_NAME");
        let display_name = cfg
            .get(key.as_bytes())
            .filter(|v| !v.is_empty())
            .map_or_else(|| name.clone(), BString::from);
        self.install_types.insert(
            name.clone(),
            InstallTypeMeta {
                name: name.clone(),
                display_name,
                index,
            },
        );
    }

    fn register_group_chain(
        &mut self,
        cfg: &CpackConfig,
        start: &BString,
    ) -> Result<(), CpackError> {
        let mut current = start.clone();
        let mut visiting = BTreeSet::new();
        let mut links: Vec<(BString, BString)> = Vec::new();
        while !self.groups.contains_key(&current) {
            if !visiting.insert(current.clone()) {
                return Err(error(format!(
                    "Cycle detected in CPack component group hierarchy involving \"{current}\""
                )));
            }
            if visiting.len() > MAX_COMPONENT_GROUP_DEPTH {
                return Err(error(format!(
                    "CPack component group hierarchy exceeds maximum depth ({MAX_COMPONENT_GROUP_DEPTH}) involving \"{start}\""
                )));
            }
            let upper = BString::from(current.to_ascii_uppercase());
            let prefix = format!("CPACK_COMPONENT_GROUP_{upper}_");
            let get = |suffix: &str| cfg.get(format!("{prefix}{suffix}").as_bytes());
            let on = |suffix: &str| get(suffix).is_some_and(truth::is_on);
            let display_name = get("DISPLAY_NAME")
                .filter(|v| !v.is_empty())
                .map_or_else(|| current.clone(), BString::from);
            let description = get("DESCRIPTION").map_or_else(BString::default, BString::from);
            let parent_group = get("PARENT_GROUP")
                .filter(|v| !v.is_empty())
                .map(BString::from);
            self.groups.insert(
                current.clone(),
                GroupMeta {
                    name: current.clone(),
                    display_name,
                    description,
                    parent_group: parent_group.clone(),
                    expanded: on("EXPANDED"),
                    bold_title: on("BOLD_TITLE"),
                    subgroups: Vec::new(),
                    components: Vec::new(),
                },
            );
            let Some(parent) = parent_group else {
                break;
            };
            links.push((parent.clone(), current));
            current = parent;
        }
        for (parent, child) in links.into_iter().rev() {
            if let Some(p) = self.groups.get_mut(&parent)
                && !p.subgroups.contains(&child)
            {
                p.subgroups.push(child);
            }
        }
        Ok(())
    }

    fn check_component_cycles(&self) -> Result<(), CpackError> {
        let mut state: BTreeMap<&[u8], VisitState> = BTreeMap::new();
        for start in self.components.keys() {
            if state.contains_key(start.as_slice()) {
                continue;
            }
            state.insert(start.as_slice(), VisitState::Visiting);
            let mut stack: Vec<(&BString, usize)> = vec![(start, 0)];
            while let Some((node, next_idx)) = stack.last_mut() {
                let deps = self
                    .components
                    .get(*node)
                    .map_or(&[][..], |comp| comp.depends.as_slice());
                if let Some(dep) = deps.get(*next_idx) {
                    *next_idx += 1;
                    match state.get(dep.as_slice()) {
                        Some(VisitState::Visiting) => {
                            return Err(error(format!(
                                "Cycle detected in CPack component dependencies involving \"{node}\" and \"{dep}\""
                            )));
                        }
                        Some(VisitState::Visited) => {}
                        None => {
                            state.insert(dep.as_slice(), VisitState::Visiting);
                            stack.push((dep, 0));
                        }
                    }
                } else {
                    let finished = *node;
                    stack.pop();
                    state.insert(finished.as_slice(), VisitState::Visited);
                }
            }
        }
        Ok(())
    }

    fn check_group_hierarchy(&self) -> Result<(), CpackError> {
        for name in self.groups.keys() {
            let mut seen = BTreeSet::new();
            let mut cur = Some(name);
            let mut depth = 0_usize;
            while let Some(group_name) = cur {
                if !seen.insert(group_name.as_slice()) {
                    return Err(error(format!(
                        "Cycle detected in CPack component group hierarchy involving \"{group_name}\""
                    )));
                }
                depth += 1;
                if depth > MAX_COMPONENT_GROUP_DEPTH {
                    return Err(error(format!(
                        "CPack component group hierarchy exceeds maximum depth ({MAX_COMPONENT_GROUP_DEPTH}) involving \"{name}\""
                    )));
                }
                cur = self
                    .groups
                    .get(group_name)
                    .and_then(|g| g.parent_group.as_ref());
            }
        }
        Ok(())
    }
}

/// Traversal state for iterative dependency cycle checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisitState {
    Visiting,
    Visited,
}

/// Copy a component's validated live staged root into `dest_root`.
///
/// When `preserve_root_prefix` is true (`productbuild`), copies each pre-hook `staged.root`
/// tree directly so install prefixes and hook-added/hook-replaced files remain intact.
/// When false (`IFW`), copies each pre-hook `staged.scanned` payload root directly without
/// retaining the component staging directory prefix.
#[allow(clippy::too_many_arguments)]
pub(crate) fn copy_component_payload(
    host: &dyn Host,
    staging_dir: &Path,
    canon_staging: &Path,
    component: &InstalledComponent,
    dest_root: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
    preserve_root_prefix: bool,
) -> Result<(), CpackError> {
    host.create_dir_all(dest_root)?;
    if component.staged_roots.is_empty() {
        let comp_rel = crate::stage::component_dir_for_host(&component.name, windows_rules);
        let dir = staging_dir.join(&comp_rel);
        if host.metadata_fresh(&dir, Follow::No).is_ok() {
            verify_contained_path(
                host,
                staging_dir,
                canon_staging,
                &comp_rel,
                SymlinkPolicy::Reject,
                false,
            )?;
            let canon_dir = canonical_checked_root(host, &dir)?;
            if !canon_dir.starts_with(canon_staging) {
                return Err(error(format!(
                    "CPack component staging root escapes staging directory: {}",
                    dir.display()
                )));
            }
            copy_checked_tree_expected(
                host,
                &dir,
                &canon_dir,
                dest_root,
                symlink_policy,
                windows_rules,
            )?;
        }
        return Ok(());
    }
    for staged in &component.staged_roots {
        let (src_dir, expected_canon) = if preserve_root_prefix {
            (&staged.root, &staged.canon_root)
        } else {
            (&staged.scanned, &staged.canon_scanned)
        };
        let canon = if expected_canon.as_os_str().is_empty() {
            canonical_checked_root(host, src_dir)?
        } else {
            expected_canon.clone()
        };
        if !canon.starts_with(canon_staging) {
            return Err(error(format!(
                "CPack component root escapes staging directory: {}",
                src_dir.display()
            )));
        }
        copy_checked_tree_expected(
            host,
            src_dir,
            &canon,
            dest_root,
            symlink_policy,
            windows_rules,
        )?;
    }
    Ok(())
}

/// Copy a validated monolithic staging tree into `dest_root`.
pub(crate) fn copy_monolithic_payload(
    host: &dyn Host,
    staging_dir: &Path,
    expected_canon_staging: &Path,
    dest_root: &Path,
    symlink_policy: SymlinkPolicy,
    windows_rules: bool,
) -> Result<(), CpackError> {
    host.create_dir_all(dest_root)?;
    copy_checked_tree_expected(
        host,
        staging_dir,
        expected_canon_staging,
        dest_root,
        symlink_policy,
        windows_rules,
    )
}

/// Compute total regular file bytes under `dir` using checked no-follow traversal.
pub(crate) fn checked_tree_size_bytes(host: &dyn Host, dir: &Path) -> Result<u64, CpackError> {
    let canon = canonical_checked_root(host, dir)?;
    let mut total = 0_u64;
    let mut remaining = 100_000_usize;
    sum_dir_bytes(host, &canon, dir, 0, &mut remaining, &mut total)?;
    Ok(total)
}

fn sum_dir_bytes(
    host: &dyn Host,
    canon_root: &Path,
    dir: &Path,
    depth: usize,
    remaining: &mut usize,
    total: &mut u64,
) -> Result<(), CpackError> {
    if depth >= 256 {
        return Err(error("CPack payload depth exceeds the limit"));
    }
    for entry in host.read_dir(dir)? {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("CPack payload file count exceeds the limit"))?;
        if entry.name.starts_with(b"._") {
            continue;
        }
        let path = dir.join(to_native(&entry.name));
        let meta = host.metadata_fresh(&path, Follow::No)?;
        match meta.kind {
            FileKind::File => {
                if !host.canonicalize_fresh(&path)?.starts_with(canon_root) {
                    return Err(error(format!(
                        "CPack payload file escapes root: {}",
                        path.display()
                    )));
                }
                *total = total.saturating_add(meta.len);
            }
            FileKind::Dir => {
                if !host.canonicalize_fresh(&path)?.starts_with(canon_root) {
                    return Err(error(format!(
                        "CPack payload directory escapes root: {}",
                        path.display()
                    )));
                }
                sum_dir_bytes(host, canon_root, &path, depth + 1, remaining, total)?;
            }
            FileKind::Symlink | FileKind::Other => {}
        }
    }
    Ok(())
}

/// Validate that every character in `text` is a legal XML 1.0 character.
pub(crate) fn validate_xml_text(text: &str, label: &str) -> Result<(), CpackError> {
    for ch in text.chars() {
        let code = ch as u32;
        let ok = matches!(
            code,
            0x09 | 0x0A | 0x0D | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
        );
        if !ok {
            return Err(error(format!(
                "Invalid XML 1.0 character U+{code:04X} in {label}"
            )));
        }
    }
    Ok(())
}

/// Validate an XML 1.0 NCName (used for namespace prefixes and local names).
pub(crate) fn validate_xml_ncname(name: &str) -> Result<(), CpackError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(error("Empty XML name"));
    };
    let is_start =
        |c: char| c.is_ascii_alphabetic() || c == '_' || (!c.is_ascii() && c.is_alphabetic());
    let is_cont = |c: char| {
        is_start(c)
            || c.is_ascii_digit()
            || c == '-'
            || c == '.'
            || (!c.is_ascii() && c.is_alphanumeric())
    };
    if !is_start(first) || !chars.all(is_cont) {
        return Err(error(format!("Invalid XML name: \"{name}\"")));
    }
    Ok(())
}

/// Validate an XML 1.0 QName (`local` or `prefix:local`).
pub(crate) fn validate_xml_qname(name: &str) -> Result<(), CpackError> {
    if let Some((prefix, local)) = name.split_once(':') {
        if local.contains(':') {
            return Err(error(format!("Invalid XML qualified name: \"{name}\"")));
        }
        validate_xml_ncname(prefix)?;
        validate_xml_ncname(local)
    } else {
        validate_xml_ncname(name)
    }
}

/// Escape an XML 1.0 text node after validating legal scalar values.
pub(crate) fn escape_xml_text(value: &str) -> Result<String, CpackError> {
    validate_xml_text(value, "XML text")?;
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    Ok(out)
}

/// Escape an XML 1.0 double-quoted attribute value, encoding significant tabs and newlines
/// as numeric character references.
pub(crate) fn escape_xml_attr(value: &str) -> Result<String, CpackError> {
    validate_xml_text(value, "XML attribute")?;
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(ch),
        }
    }
    Ok(out)
}

/// Escape WiX preprocessor sigil `$` as `$$` for literal metadata values.
pub(crate) fn escape_wix_preprocessor(value: &str) -> String {
    value.replace('$', "$$")
}

/// Structured, typed XML 1.0 writer.
#[derive(Clone, Debug)]
pub(crate) struct XmlWriter {
    buf: String,
    stack: Vec<String>,
    tag_open: bool,
    has_children: Vec<bool>,
    indent_unit: &'static str,
    base_indent: usize,
}

impl XmlWriter {
    pub(crate) fn new(indent_unit: &'static str, base_indent: usize) -> Self {
        Self {
            buf: String::new(),
            stack: Vec::new(),
            tag_open: false,
            has_children: Vec::new(),
            indent_unit,
            base_indent,
        }
    }

    pub(crate) fn declaration(&mut self) {
        self.buf
            .push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    }

    fn write_indent(&mut self, depth: usize) {
        for _ in 0..(self.base_indent + depth) {
            self.buf.push_str(self.indent_unit);
        }
    }

    fn close_opening_tag(&mut self) {
        if self.tag_open {
            self.buf.push('>');
            self.tag_open = false;
        }
    }

    pub(crate) fn start_element(&mut self, name: &str) -> Result<(), CpackError> {
        validate_xml_qname(name)?;
        self.close_opening_tag();
        if let Some(last) = self.has_children.last_mut() {
            *last = true;
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.write_indent(self.stack.len());
        self.buf.push('<');
        self.buf.push_str(name);
        self.stack.push(name.to_owned());
        self.has_children.push(false);
        self.tag_open = true;
        Ok(())
    }

    pub(crate) fn attr(&mut self, name: &str, value: &str) -> Result<(), CpackError> {
        if !self.tag_open {
            return Err(error(format!(
                "Cannot write XML attribute \"{name}\" outside an opening tag"
            )));
        }
        validate_xml_qname(name)?;
        let escaped = escape_xml_attr(value)?;
        self.buf.push(' ');
        self.buf.push_str(name);
        self.buf.push_str("=\"");
        self.buf.push_str(&escaped);
        self.buf.push('"');
        Ok(())
    }

    pub(crate) fn text(&mut self, content: &str) -> Result<(), CpackError> {
        if self.stack.is_empty() {
            return Err(error("Cannot write XML text outside an element"));
        }
        self.close_opening_tag();
        let escaped = escape_xml_text(content)?;
        self.buf.push_str(&escaped);
        Ok(())
    }

    /// Force an empty open element to serialize as `<tag></tag>` instead of `<tag/>`.
    pub(crate) fn force_non_self_closing(&mut self) {
        self.close_opening_tag();
    }

    pub(crate) fn comment(&mut self, text: &str) -> Result<(), CpackError> {
        validate_xml_text(text, "XML comment")?;
        if text.contains("--") || text.ends_with('-') {
            return Err(error(
                "Invalid XML comment containing \"--\" or trailing '-'",
            ));
        }
        self.close_opening_tag();
        if let Some(last) = self.has_children.last_mut() {
            *last = true;
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.write_indent(self.stack.len());
        self.buf.push_str("<!--");
        self.buf.push_str(text);
        self.buf.push_str("-->");
        Ok(())
    }

    pub(crate) fn processing_instruction(
        &mut self,
        target: &str,
        content: &str,
    ) -> Result<(), CpackError> {
        validate_xml_ncname(target)?;
        validate_xml_text(content, "XML processing instruction")?;
        if content.contains("?>") {
            return Err(error(
                "Invalid XML processing instruction containing \"?>\"",
            ));
        }
        self.close_opening_tag();
        if let Some(last) = self.has_children.last_mut() {
            *last = true;
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.write_indent(self.stack.len());
        self.buf.push_str("<?");
        self.buf.push_str(target);
        if !content.is_empty() {
            self.buf.push(' ');
            self.buf.push_str(content);
        }
        self.buf.push_str("?>");
        Ok(())
    }

    pub(crate) fn end_element(&mut self, expected: &str) -> Result<(), CpackError> {
        let Some(name) = self.stack.pop() else {
            return Err(error(format!(
                "Cannot close XML element </{expected}>: no element is open"
            )));
        };
        let had_children = self.has_children.pop().unwrap_or(false);
        if name != expected {
            return Err(error(format!(
                "Mismatched XML closing tag: expected </{name}>, got </{expected}>"
            )));
        }
        if self.tag_open {
            self.buf.push_str("/>");
            self.tag_open = false;
        } else {
            if had_children {
                if !self.buf.ends_with('\n') {
                    self.buf.push('\n');
                }
                self.write_indent(self.stack.len());
            }
            self.buf.push_str("</");
            self.buf.push_str(&name);
            self.buf.push('>');
        }
        Ok(())
    }

    pub(crate) fn element_text(&mut self, name: &str, content: &str) -> Result<(), CpackError> {
        self.start_element(name)?;
        self.text(content)?;
        self.end_element(name)
    }

    pub(crate) fn finish(self) -> Result<String, CpackError> {
        if !self.stack.is_empty() {
            return Err(error(format!(
                "Unclosed XML elements remaining: {:?}",
                self.stack
            )));
        }
        let mut out = self.buf;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }

    pub(crate) fn finish_fragment(self) -> Result<String, CpackError> {
        if !self.stack.is_empty() {
            return Err(error(format!(
                "Unclosed XML fragment elements remaining: {:?}",
                self.stack
            )));
        }
        Ok(self.buf)
    }
}

/// A parsed XML element node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct XmlElement {
    pub(crate) name: String,
    pub(crate) attributes: Vec<(String, String)>,
    pub(crate) children: Vec<XmlNode>,
}

/// Child node inside an [`XmlElement`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum XmlNode {
    Element(XmlElement),
    Text(String),
}

/// Parse an XML document with strict resource bounds, forbidding `<!DOCTYPE`,
/// `<!ENTITY`, external resolution, and arbitrary entity expansion.
pub(crate) fn parse_xml_bounded(bytes: &[u8], label: &str) -> Result<XmlElement, CpackError> {
    if bytes.len() as u64 > MAX_XML_BYTES {
        return Err(error(format!(
            "XML file {label} exceeds maximum allowed size ({MAX_XML_BYTES} bytes)"
        )));
    }
    let text = require_utf8(bytes, label)?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    validate_xml_text(text, label)?;
    let mut parser = XmlParser {
        src: text,
        pos: 0,
        nodes: 0,
        label,
    };
    parser.parse_document()
}

struct XmlParser<'a> {
    src: &'a str,
    pos: usize,
    nodes: usize,
    label: &'a str,
}

impl<'a> XmlParser<'a> {
    fn rest(&self) -> &'a str {
        self.src.get(self.pos..).unwrap_or("")
    }

    fn err(&self, msg: impl std::fmt::Display) -> CpackError {
        let prefix = self.src.get(..self.pos).unwrap_or("");
        let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
        let col = prefix
            .rsplit_once('\n')
            .map_or(prefix.len() + 1, |(_, tail)| tail.len() + 1);
        error(format!(
            "XML parse error in {} at {line}:{col}: {msg}",
            self.label
        ))
    }

    fn bump_node(&mut self) -> Result<(), CpackError> {
        self.nodes += 1;
        if self.nodes > MAX_XML_NODES {
            return Err(self.err("XML node count exceeds safety limit"));
        }
        Ok(())
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n']);
        self.pos = self.src.len() - trimmed.len();
    }

    fn skip_misc(&mut self) -> Result<(), CpackError> {
        loop {
            self.skip_ws();
            let rest = self.rest();
            if rest.starts_with("<?") {
                let Some(end) = rest.find("?>") else {
                    return Err(self.err("unterminated processing instruction"));
                };
                self.pos += end + 2;
            } else if rest.starts_with("<!--") {
                let Some(end) = rest.get(4..).and_then(|s| s.find("-->")) else {
                    return Err(self.err("unterminated XML comment"));
                };
                if rest.get(4..4 + end).is_some_and(|body| body.contains("--")) {
                    return Err(self.err("invalid '--' inside XML comment"));
                }
                self.pos += 4 + end + 3;
            } else if rest.starts_with("<!DOCTYPE") || rest.starts_with("<!ENTITY") {
                return Err(self.err("DTD and ENTITY declarations are forbidden"));
            } else {
                break;
            }
        }
        Ok(())
    }

    fn parse_document(&mut self) -> Result<XmlElement, CpackError> {
        self.skip_misc()?;
        if !self.rest().starts_with('<') {
            return Err(self.err("expected root XML element"));
        }
        let root = self.parse_element(0)?;
        self.skip_misc()?;
        if !self.rest().is_empty() {
            return Err(self.err("unexpected trailing content after root XML element"));
        }
        Ok(root)
    }

    fn parse_element(&mut self, depth: usize) -> Result<XmlElement, CpackError> {
        if depth >= MAX_XML_DEPTH {
            return Err(self.err("XML element nesting exceeds depth limit"));
        }
        self.bump_node()?;
        if !self.rest().starts_with('<') {
            return Err(self.err("expected '<'"));
        }
        self.pos += 1;
        if self.rest().starts_with('!') {
            return Err(self.err("unexpected declaration (DTD/ENTITY declarations are forbidden)"));
        }
        let name = self.parse_name()?;
        validate_xml_qname(&name)?;
        let mut attributes = Vec::new();
        let mut seen_attrs = BTreeSet::new();
        let self_closing;
        loop {
            self.skip_ws();
            if self.rest().starts_with("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if self.rest().starts_with('>') {
                self.pos += 1;
                self_closing = false;
                break;
            }
            if self.rest().is_empty() {
                return Err(self.err(format!("unterminated start tag <{name}>")));
            }
            let attr_name = self.parse_name()?;
            validate_xml_qname(&attr_name)?;
            if !seen_attrs.insert(attr_name.clone()) {
                return Err(self.err(format!("duplicate attribute \"{attr_name}\"")));
            }
            self.skip_ws();
            if !self.rest().starts_with('=') {
                return Err(self.err(format!("expected '=' after attribute \"{attr_name}\"")));
            }
            self.pos += 1;
            self.skip_ws();
            let attr_value = self.parse_attr_value()?;
            attributes.push((attr_name, attr_value));
            if attributes.len() > 256 {
                return Err(self.err("attribute count on element exceeds limit"));
            }
        }
        if self_closing {
            return Ok(XmlElement {
                name,
                attributes,
                children: Vec::new(),
            });
        }

        let mut children = Vec::new();
        loop {
            let rest = self.rest();
            if rest.is_empty() {
                return Err(self.err(format!("unclosed element <{name}>")));
            }
            if rest.starts_with("</") {
                self.pos += 2;
                let close_name = self.parse_name()?;
                self.skip_ws();
                if !self.rest().starts_with('>') {
                    return Err(self.err(format!("expected '>' in closing tag </{close_name}>")));
                }
                self.pos += 1;
                if close_name != name {
                    return Err(self.err(format!(
                        "mismatched closing tag: expected </{name}>, got </{close_name}>"
                    )));
                }
                break;
            }
            if rest.starts_with("<!--") {
                let Some(end) = rest.get(4..).and_then(|s| s.find("-->")) else {
                    return Err(self.err("unterminated XML comment"));
                };
                if rest.get(4..4 + end).is_some_and(|body| body.contains("--")) {
                    return Err(self.err("invalid '--' inside XML comment"));
                }
                self.pos += 4 + end + 3;
            } else if rest.starts_with("<![CDATA[") {
                let Some(end) = rest.get(9..).and_then(|s| s.find("]]>")) else {
                    return Err(self.err("unterminated CDATA section"));
                };
                let body = rest.get(9..9 + end).unwrap_or("");
                self.pos += 9 + end + 3;
                self.bump_node()?;
                children.push(XmlNode::Text(body.to_owned()));
            } else if rest.starts_with("<?") {
                let Some(end) = rest.find("?>") else {
                    return Err(self.err("unterminated processing instruction"));
                };
                self.pos += end + 2;
            } else if rest.starts_with("<!") {
                return Err(self.err("DTD and ENTITY declarations are forbidden"));
            } else if rest.starts_with('<') {
                let child = self.parse_element(depth + 1)?;
                children.push(XmlNode::Element(child));
            } else {
                let next_lt = rest.find('<').unwrap_or(rest.len());
                let raw = rest.get(..next_lt).unwrap_or("");
                self.pos += next_lt;
                let decoded = self.decode_entities(raw)?;
                if !decoded.is_empty() {
                    self.bump_node()?;
                    children.push(XmlNode::Text(decoded));
                }
            }
        }
        Ok(XmlElement {
            name,
            attributes,
            children,
        })
    }

    fn parse_name(&mut self) -> Result<String, CpackError> {
        let rest = self.rest();
        let len = rest
            .char_indices()
            .take_while(|&(_, c)| {
                !matches!(
                    c,
                    ' ' | '\t' | '\r' | '\n' | '=' | '>' | '/' | '<' | '"' | '\''
                )
            })
            .last()
            .map_or(0, |(idx, c)| idx + c.len_utf8());
        if len == 0 {
            return Err(self.err("expected XML identifier"));
        }
        let name = rest.get(..len).unwrap_or("").to_owned();
        self.pos += len;
        Ok(name)
    }

    fn parse_attr_value(&mut self) -> Result<String, CpackError> {
        let rest = self.rest();
        let quote = match rest.as_bytes().first() {
            Some(b'"') => '"',
            Some(b'\'') => '\'',
            _ => return Err(self.err("expected quoted attribute value")),
        };
        self.pos += 1;
        let rest = self.rest();
        let Some(end) = rest.find(quote) else {
            return Err(self.err("unterminated attribute value"));
        };
        let raw = rest.get(..end).unwrap_or("");
        if raw.contains('<') {
            return Err(self.err("unescaped '<' inside attribute value"));
        }
        self.pos += end + 1;
        self.decode_entities(raw)
    }

    fn decode_entities(&self, raw: &str) -> Result<String, CpackError> {
        if !raw.contains('&') {
            return Ok(raw.to_owned());
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(amp) = rest.find('&') {
            out.push_str(rest.get(..amp).unwrap_or(""));
            let after = rest.get(amp + 1..).unwrap_or("");
            let Some(semi) = after.find(';') else {
                return Err(self.err("unterminated XML entity reference"));
            };
            let entity = after.get(..semi).unwrap_or("");
            match entity {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    let hex = entity.get(2..).unwrap_or("");
                    let code = u32::from_str_radix(hex, 16).map_err(|_| {
                        self.err(format!("invalid hex character reference &{entity};"))
                    })?;
                    let ch = char::from_u32(code)
                        .ok_or_else(|| self.err(format!("invalid Unicode scalar &{entity};")))?;
                    validate_xml_text(&ch.to_string(), self.label)?;
                    out.push(ch);
                }
                _ if entity.starts_with('#') => {
                    let dec = entity.get(1..).unwrap_or("");
                    let code = dec
                        .parse::<u32>()
                        .map_err(|_| self.err(format!("invalid character reference &{entity};")))?;
                    let ch = char::from_u32(code)
                        .ok_or_else(|| self.err(format!("invalid Unicode scalar &{entity};")))?;
                    validate_xml_text(&ch.to_string(), self.label)?;
                    out.push(ch);
                }
                _ => {
                    return Err(self.err(format!(
                        "custom or external XML entity reference &{entity}; is forbidden"
                    )));
                }
            }
            rest = after.get(semi + 1..).unwrap_or("");
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// Escape a metadata string for placement inside a double-quoted NSIS string literal.
///
/// Escapes `$` as `$$`, `"` as `$\"`, `\t` as `$\t`, and newlines as `$\r$\n` (when
/// `allow_newlines` is true) or rejects control characters when `allow_newlines` is false.
pub(crate) fn escape_nsis_literal(value: &str, allow_newlines: bool) -> Result<String, CpackError> {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\0' => return Err(error("NSIS string literal cannot contain NUL")),
            '$' => out.push_str("$$"),
            '"' => out.push_str("$\\\""),
            '\t' => {
                if allow_newlines {
                    out.push_str("$\\t");
                } else {
                    return Err(error("NSIS single-line literal cannot contain tab"));
                }
            }
            '\r' => {
                if !allow_newlines {
                    return Err(error("NSIS single-line literal cannot contain newline"));
                }
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push_str("$\\r$\\n");
                } else {
                    out.push_str("$\\r");
                }
            }
            '\n' => {
                if !allow_newlines {
                    return Err(error("NSIS single-line literal cannot contain newline"));
                }
                out.push_str("$\\r$\\n");
            }
            c if c.is_control() => {
                return Err(error(format!(
                    "NSIS string literal cannot contain control character U+{:04X}",
                    c as u32
                )));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Reject staged filenames or paths containing NSIS glob wildcards (`*`, `?`).
pub(crate) fn reject_nsis_glob_hazard(path: &str, context: &str) -> Result<(), CpackError> {
    if path.contains('*') || path.contains('?') {
        return Err(error(format!(
            "Unsafe NSIS glob character in {context}: \"{path}\""
        )));
    }
    Ok(())
}

/// Escape an identifier for embedding inside a JavaScript single-quoted string literal
/// (e.g., Apple `productbuild` `choices['...'].selected`).
pub(crate) fn escape_js_single_quoted(value: &str) -> Result<String, CpackError> {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\0' => return Err(error("JavaScript literal cannot contain NUL")),
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if c.is_control() => {
                return Err(error(format!(
                    "JavaScript literal cannot contain control character U+{:04X}",
                    c as u32
                )));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Percent-encode a relative URL path, preserving RFC 3986 unreserved characters and `/`.
pub(crate) fn encode_url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let unreserved =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/');
        if unreserved {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Find a CPack template file in `CPACK_MODULE_PATH` / `CMAKE_MODULE_PATH` or `CMAKE_ROOT`.
pub(crate) fn find_template(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    filename: &str,
    alt_subdir: Option<&str>,
) -> Result<PathBuf, CpackError> {
    let mut candidates = Vec::new();
    for var in [b"CPACK_MODULE_PATH".as_slice(), b"CMAKE_MODULE_PATH"] {
        for dir in cfg.list(var) {
            let base = absolute(cwd, &dir);
            if let Some(sub) = alt_subdir {
                candidates.push(base.join(sub).join(filename));
            }
            candidates.push(base.join(filename));
        }
    }
    if let Some(root) = cfg.get(b"CMAKE_ROOT").filter(|r| !r.is_empty()) {
        let root = absolute(cwd, root);
        let internal = root.join("Modules/Internal/CPack");
        if let Some(sub) = alt_subdir {
            candidates.push(internal.join(sub).join(filename));
        }
        candidates.push(internal.join(filename));
        candidates.push(root.join("Templates").join(filename));
    }
    for path in candidates {
        if host
            .metadata(&path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Ok(path);
        }
    }
    Err(error(format!(
        "Could not find CPack template file: {filename}"
    )))
}

/// Read a template file as UTF-8 within [`MAX_TEMPLATE_BYTES`].
pub(crate) fn read_template(host: &dyn Host, path: &Path) -> Result<String, CpackError> {
    let meta = host.metadata(path, Follow::Yes)?;
    if meta.kind != FileKind::File {
        return Err(error(format!(
            "Expected a regular template file at {}",
            path.display()
        )));
    }
    let bytes = host.read(
        path,
        ReadRange::All {
            limit: MAX_TEMPLATE_BYTES.min(Limits::default().max_file_read),
        },
    )?;
    let text = require_utf8(&bytes, &path.display().to_string())?;
    Ok(text.to_owned())
}

/// Perform a single-pass `@VAR@` substitution over `template` without re-expanding any
/// `@...@` sequences inside replacement values.
pub(crate) fn render_at_template(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('@') {
        out.push_str(rest.get(..start).unwrap_or(""));
        let after = rest.get(start + 1..).unwrap_or("");
        if let Some(end) = after.find('@') {
            let key = after.get(..end).unwrap_or("");
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                if let Some(val) = values.get(key) {
                    out.push_str(val);
                }
                rest = after.get(end + 1..).unwrap_or("");
                continue;
            }
        }
        out.push('@');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Find an executable `name` on `extra_paths` and the host `PATH`.
pub(crate) fn find_program_on_host(
    host: &dyn Host,
    cwd: &Path,
    name: &[u8],
    extra_paths: &[PathBuf],
    suffixes: &[&str],
) -> Option<PathBuf> {
    if name.is_empty() || name.contains(&0) {
        return None;
    }
    let windows = is_windows_host(host);
    let candidate_path = to_native(name);
    if candidate_path.is_absolute() || name.contains(&b'/') || (windows && name.contains(&b'\\')) {
        let resolved = absolute(cwd, name);
        return check_program_candidates(host, &resolved, windows);
    }

    let mut search_dirs: Vec<PathBuf> = Vec::new();
    for base in extra_paths {
        search_dirs.push(base.clone());
        for suffix in suffixes {
            search_dirs.push(base.join(suffix));
        }
    }
    if let Some(path_env) = host.env_get(b"PATH") {
        let sep = if windows { b';' } else { b':' };
        for dir in path_env.split(|&b| b == sep) {
            if !dir.is_empty() {
                search_dirs.push(to_native(dir));
            }
        }
    }
    for dir in search_dirs {
        let candidate = dir.join(&candidate_path);
        if let Some(found) = check_program_candidates(host, &candidate, windows) {
            return Some(found);
        }
    }
    None
}

fn check_program_candidates(host: &dyn Host, base: &Path, windows: bool) -> Option<PathBuf> {
    let mut tries = vec![base.to_path_buf()];
    if windows && base.extension().is_none() {
        for ext in ["exe", "cmd", "bat"] {
            tries.push(base.with_extension(ext));
        }
    }
    for path in tries {
        if let Ok(meta) = host.metadata(&path, Follow::Yes)
            && meta.kind == FileKind::File
        {
            return Some(path);
        }
    }
    None
}

/// Captured outcome from running an external installer tool.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) status: ProcessResult,
    pub(crate) output: Vec<u8>,
}

/// Run an external installer tool through [`Host::run_bounded`] with null stdin, explicit
/// timeout, output cap, and optional secret redaction.
pub(crate) fn run_installer_tool(
    host: &dyn Host,
    cwd: &Path,
    program: &[u8],
    args: &[BString],
    timeout: Duration,
) -> Result<ToolOutcome, CpackError> {
    let req = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.into(),
            args: args.to_vec(),
        }],
        cwd: cwd.to_owned(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::SharedWithStdout,
        timeout: Some(timeout),
        tee: false,
    };
    let out = host.run_bounded(&req, MAX_TOOL_OUTPUT_BYTES)?;
    let status = out
        .results
        .into_iter()
        .next()
        .unwrap_or(ProcessResult::Exited(1));
    Ok(ToolOutcome {
        status,
        output: out.stdout,
    })
}

/// Redact any non-empty secrets from `text` before writing logs or diagnostics.
///
/// Secrets are matched longest-first so XML-escaped or composite forms are scrubbed
/// before shorter substrings.
pub(crate) fn redact_secrets(text: &[u8], secrets: &[BString]) -> BString {
    let mut ordered: Vec<&[u8]> = secrets
        .iter()
        .map(|s| s.as_slice())
        .filter(|s| !s.is_empty())
        .collect();
    ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    ordered.dedup();
    let mut buf = text.to_vec();
    for secret in ordered {
        buf = replace_bytes(&buf, secret, b"***");
    }
    BString::from(buf)
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack
            .get(i..i + needle.len())
            .is_some_and(|slice| slice == needle)
        {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else if let Some(&b) = haystack.get(i) {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Format a tool invocation for log files with secrets redacted.
pub(crate) fn format_command_for_log(
    program: &[u8],
    args: &[BString],
    secrets: &[BString],
) -> BString {
    let mut line = Vec::new();
    line.extend_from_slice(program);
    for arg in args {
        line.push(b' ');
        line.extend_from_slice(arg);
    }
    redact_secrets(&line, secrets)
}

/// Write or append a tool failure/execution log safely (never following a symlink).
pub(crate) fn write_tool_log(
    host: &dyn Host,
    log_path: &Path,
    program: &[u8],
    args: &[BString],
    output: &[u8],
    secrets: &[BString],
    append: bool,
) -> Result<(), CpackError> {
    if let Ok(meta) = host.metadata_fresh(log_path, Follow::No)
        && meta.kind != FileKind::File
    {
        return Err(error(format!(
            "Refusing to write tool log to non-regular path: {}",
            log_path.display()
        )));
    }
    let cmd = format_command_for_log(program, args, secrets);
    let redacted = redact_secrets(output, secrets);
    let mut body = Vec::new();
    body.extend_from_slice(b"# Run command: ");
    body.extend_from_slice(&cmd);
    body.extend_from_slice(b"\n# Output:\n");
    body.extend_from_slice(&redacted);
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    let mode = if append {
        WriteMode::Append
    } else {
        WriteMode::Truncate
    };
    host.write(log_path, &body, mode)?;
    Ok(())
}

/// Prepare a fresh output file path before invoking a tool so a stale artifact cannot pass
/// as freshly created. Rejects existing directories or symlinks at the output path.
pub(crate) fn prepare_fresh_output(host: &dyn Host, output: &Path) -> Result<(), CpackError> {
    match host.metadata_fresh(output, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File => {
            host.remove_file(output)?;
            Ok(())
        }
        Ok(_) => Err(error(format!(
            "Refusing to overwrite non-regular output path: {}",
            output.display()
        ))),
        Err(err) if err.kind == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Verify after a tool exits 0 that `expected_parent` still matches `expected_canon_parent`
/// and that `output` now exists as a regular file (no-follow) strictly inside it.
pub(crate) fn verify_fresh_output(
    host: &dyn Host,
    expected_parent: &Path,
    expected_canon_parent: &Path,
    output: &Path,
    phase: &str,
) -> Result<(), CpackError> {
    let canon_parent = verify_expected_root(host, expected_parent, expected_canon_parent)?;
    let meta = host.metadata_fresh(output, Follow::No).map_err(|err| {
        error(format!(
            "{phase} exited with status 0 but did not create expected output {}: {err}",
            output.display()
        ))
    })?;
    if meta.kind != FileKind::File {
        return Err(error(format!(
            "{phase} created non-regular output at {} (expected regular file)",
            output.display()
        )));
    }
    let rel = output.strip_prefix(expected_parent).map_err(|_| {
        error(format!(
            "{phase} output escapes expected directory: {}",
            output.display()
        ))
    })?;
    verify_contained_path(
        host,
        expected_parent,
        &canon_parent,
        rel,
        SymlinkPolicy::Reject,
        false,
    )?;
    let canon_out = host.canonicalize_fresh(output)?;
    if !canon_out.starts_with(&canon_parent) {
        return Err(error(format!(
            "{phase} output escapes expected directory: {}",
            output.display()
        )));
    }
    Ok(())
}

/// Convert a [`ToolOutcome`] into `Ok(())` on `Exited(0)` or a detailed [`CpackError`]
/// with all configured secrets redacted.
pub(crate) fn check_tool_outcome(
    outcome: &ToolOutcome,
    program: &[u8],
    phase: &str,
    log_path: Option<&Path>,
    secrets: &[BString],
) -> Result<(), CpackError> {
    let raw_message = match &outcome.status {
        ProcessResult::Exited(0) => return Ok(()),
        ProcessResult::TimedOut => {
            let log_hint = log_path
                .map(|p| format!(" Please check {} for errors.", p.display()))
                .unwrap_or_default();
            format!("{phase}: {} timed out.{log_hint}", BString::from(program))
        }
        ProcessResult::SpawnFailed(err) => {
            format!("{phase}: cannot start {}: {err}", BString::from(program))
        }
        status => {
            let redacted = redact_secrets(&outcome.output, secrets);
            let log_hint = log_path
                .map(|p| format!("\nPlease check {} for errors", p.display()))
                .unwrap_or_default();
            format!(
                "{phase}: {} failed ({status:?}){log_hint}\n{redacted}",
                BString::from(program)
            )
        }
    };
    Err(error(redact_secrets(raw_message.as_bytes(), secrets)))
}

/// Emit a CPack warning on stderr.
pub(crate) fn warn(host: &dyn Host, message: impl std::fmt::Display) {
    host.write_out(
        Stream::Stderr,
        format!("CPack Warning: {message}\n").as_bytes(),
    );
}

/// Format a CMake path using Windows backslashes (`\` instead of `/`).
pub(crate) fn to_windows_slashes(path: &Path) -> String {
    from_native_cmake(path).to_string().replace('/', "\\")
}

/// Validate a configured extension string (leading dot, single filename token).
pub(crate) fn validate_extension(ext: &[u8]) -> Result<BString, CpackError> {
    if ext.is_empty()
        || !ext.starts_with(b".")
        || ext.iter().any(|&b| matches!(b, b'/' | b'\\' | 0))
    {
        return Err(error(format!(
            "Invalid CPack package file extension: {}",
            BString::from(ext)
        )));
    }
    let _ = safe_name(&[b"x", ext].concat())?;
    Ok(BString::from(ext))
}

/// Validate that a relative file reference (such as a `.qrc` `<file>` entry) does not escape
/// its parent directory.
pub(crate) fn safe_relative_subpath(raw: &str, label: &str) -> Result<PathBuf, CpackError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(error(format!("Empty or invalid relative path in {label}")));
    }
    let path = to_native(trimmed.as_bytes());
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(error(format!(
                    "Path in {label} escapes base directory: \"{trimmed}\""
                )));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(error(format!(
            "Empty relative path in {label}: \"{trimmed}\""
        )));
    }
    Ok(out)
}
