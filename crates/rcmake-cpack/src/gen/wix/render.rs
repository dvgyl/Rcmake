//! WiX v3/v4 source writers, deterministic path-to-ID allocation, ACLs, and shortcuts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::hash::HashAlgo;
use rcmake_base::{list, truth};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_host::{FileKind, Follow, Host};

use super::patch::WixPatchSet;
use super::{InstallScope, WixConfig, WixVersion, guid_from_md5_component_id};
use crate::generators::installer::{
    ComponentCatalog, ComponentMeta, GroupMeta, XmlWriter, escape_wix_preprocessor, require_utf8,
    to_windows_slashes,
};
use crate::source::verify_expected_root;
use crate::{CpackError, error};

/// Structured install properties read from `CPackProperties.cmake` for one install-relative path.
#[derive(Clone, Debug, Default)]
pub(super) struct InstallFileProps {
    pub(super) never_overwrite: bool,
    pub(super) permanent: bool,
    pub(super) acls: Vec<String>,
    pub(super) start_menu_shortcuts: Vec<String>,
    pub(super) desktop_shortcuts: Vec<String>,
    pub(super) startup_shortcuts: Vec<String>,
    pub(super) custom_file_id: Option<String>,
}

/// Deterministic WiX path-to-ID allocator matching CMake 4.5.1's `PathToId` and collision suffixes.
#[derive(Clone, Debug, Default)]
pub(crate) struct PathIdAllocator {
    path_to_id: BTreeMap<String, String>,
    ambiguity: BTreeMap<String, usize>,
}

impl PathIdAllocator {
    pub(crate) fn id_for_path(&mut self, relative_path: &str) -> Result<String, CpackError> {
        if let Some(existing) = self.path_to_id.get(relative_path) {
            return Ok(existing.clone());
        }
        let parts: Vec<&str> = relative_path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(error("Cannot generate WiX ID for empty relative path"));
        }
        let mut replacement_count = 0_usize;
        let mut identifier = String::new();
        let mut last_comp = String::new();
        for (idx, part) in parts.iter().enumerate() {
            if idx > 0 {
                identifier.push('.');
            }
            last_comp = normalize_component_for_id(part, &mut replacement_count);
            identifier.push_str(&last_comp);
        }
        let replacement_percent = replacement_count
            .saturating_mul(100)
            .checked_div(identifier.len())
            .unwrap_or(0);
        let (prefix, final_ident) = if replacement_percent > 33 || identifier.len() > 60 {
            let hash = HashAlgo::Sha1.hex_digest(relative_path.as_bytes());
            let hash_prefix = hash.get(..7).unwrap_or(&hash);
            let max_name = 52;
            let mut hashed = format!(
                "{hash_prefix}_{}",
                last_comp
                    .get(..max_name.min(last_comp.len()))
                    .unwrap_or(&last_comp)
            );
            if last_comp.len() > max_name {
                hashed.push_str("...");
            }
            ("H", hashed)
        } else {
            ("P", identifier)
        };
        let count = {
            let entry = self.ambiguity.entry(final_ident.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        if count > 999 {
            return Err(error(format!(
                "Error while trying to generate a unique Id for '{relative_path}'"
            )));
        }
        let result = if count > 1 {
            format!("{prefix}_{final_ident}_{count}")
        } else {
            format!("{prefix}_{final_ident}")
        };
        self.path_to_id
            .insert(relative_path.to_owned(), result.clone());
        Ok(result)
    }
}

fn normalize_component_for_id(component: &str, replacements: &mut usize) -> String {
    let mut out = String::with_capacity(component.len());
    for byte in component.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' {
            out.push(char::from(byte));
        } else {
            out.push('_');
            *replacements += 1;
        }
    }
    out
}

/// Shortcut target category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ShortcutKind {
    StartMenu,
    Desktop,
    Startup,
}

#[derive(Clone, Debug)]
pub(super) struct ShortcutEntry {
    pub(super) label: String,
    pub(super) working_directory_id: String,
    pub(super) file_id: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ShortcutTable {
    by_kind: BTreeMap<ShortcutKind, BTreeMap<String, Vec<ShortcutEntry>>>,
}

impl ShortcutTable {
    pub(super) fn insert(&mut self, kind: ShortcutKind, id: String, entry: ShortcutEntry) {
        self.by_kind
            .entry(kind)
            .or_default()
            .entry(id)
            .or_default()
            .push(entry);
    }

    pub(super) fn is_empty(&self, kind: ShortcutKind) -> bool {
        self.by_kind.get(&kind).is_none_or(BTreeMap::is_empty)
    }

    pub(super) fn collect_kinds(&self, out: &mut BTreeSet<ShortcutKind>) {
        for (kind, entries) in &self.by_kind {
            if !entries.is_empty() {
                out.insert(*kind);
            }
        }
    }

    pub(super) fn add_from_props(
        &mut self,
        id: &str,
        file_id: &str,
        dir_id: &str,
        props: &InstallFileProps,
    ) {
        for (kind, labels) in [
            (ShortcutKind::StartMenu, &props.start_menu_shortcuts),
            (ShortcutKind::Desktop, &props.desktop_shortcuts),
            (ShortcutKind::Startup, &props.startup_shortcuts),
        ] {
            for label in labels {
                self.insert(
                    kind,
                    id.to_owned(),
                    ShortcutEntry {
                        label: label.clone(),
                        working_directory_id: dir_id.to_owned(),
                        file_id: file_id.to_owned(),
                    },
                );
            }
        }
    }
}

/// Valid WiX `<Permission>` boolean attributes (matching `cmWIXAccessControlList.cxx`).
const VALID_ACL_ATTRS: &[&str] = &[
    "Append",
    "ChangePermission",
    "CreateChild",
    "CreateFile",
    "CreateLink",
    "CreateSubkeys",
    "Delete",
    "DeleteChild",
    "EnumerateSubkeys",
    "Execute",
    "FileAllRights",
    "GenericAll",
    "GenericExecute",
    "GenericRead",
    "GenericWrite",
    "Notify",
    "Read",
    "ReadAttributes",
    "ReadExtendedAttributes",
    "ReadPermission",
    "SpecificRightsAll",
    "Synchronize",
    "TakeOwnership",
    "Traverse",
    "Write",
    "WriteAttributes",
    "WriteExtendedAttributes",
];

fn emit_acls(acls: &[String], writer: &mut XmlWriter) -> Result<(), CpackError> {
    for entry in acls {
        let Some((user_and_domain, perms_raw)) = entry.split_once('=') else {
            return Err(error(format!(
                "Failed processing ACL entry '{entry}': Did not find mandatory '='"
            )));
        };
        let (user, domain) = match user_and_domain.split_once('@') {
            Some((u, d)) => (u, Some(d)),
            None => (user_and_domain, None),
        };
        if user.is_empty() {
            return Err(error(format!(
                "Failed processing ACL entry '{entry}': Empty user name"
            )));
        }
        writer.start_element("Permission")?;
        writer.attr("User", user)?;
        if let Some(d) = domain.filter(|d| !d.is_empty()) {
            writer.attr("Domain", d)?;
        }
        for raw_perm in perms_raw.split(',') {
            let perm = raw_perm.trim();
            if perm.is_empty() {
                continue;
            }
            if !VALID_ACL_ATTRS.contains(&perm) {
                return Err(error(format!(
                    "Failed processing ACL entry '{entry}': Unknown boolean attribute '{perm}'"
                )));
            }
            writer.attr(perm, "yes")?;
        }
        writer.end_element("Permission")?;
    }
    Ok(())
}

/// Read structured install properties from `interp.cx().project().install_props`.
pub(super) fn collect_install_props(
    interp: &mut Interp,
) -> Result<BTreeMap<String, InstallFileProps>, CpackError> {
    let mut cx = interp.cx();
    let raw_props = &cx.project().install_props;
    let mut map = BTreeMap::new();
    for (file_key, props) in raw_props {
        let normalized =
            normalize_install_prop_key(require_utf8(file_key, "INSTALL property path")?);
        let mut parsed = InstallFileProps::default();
        if let Some(v) = props.get(b"CPACK_NEVER_OVERWRITE") {
            parsed.never_overwrite = truth::is_on(v);
        }
        if let Some(v) = props.get(b"CPACK_PERMANENT") {
            parsed.permanent = truth::is_on(v);
        }
        if let Some(v) = props.get(b"CPACK_WIX_ACL") {
            for item in list::split(v, list::Empty::Drop) {
                parsed
                    .acls
                    .push(require_utf8(&item, "CPACK_WIX_ACL")?.to_owned());
            }
        }
        for (prop_name, target) in [
            (
                b"CPACK_START_MENU_SHORTCUTS".as_slice(),
                &mut parsed.start_menu_shortcuts,
            ),
            (b"CPACK_DESKTOP_SHORTCUTS", &mut parsed.desktop_shortcuts),
            (b"CPACK_STARTUP_SHORTCUTS", &mut parsed.startup_shortcuts),
        ] {
            if let Some(v) = props.get(prop_name) {
                for item in list::split(v, list::Empty::Drop) {
                    target.push(require_utf8(&item, "CPACK shortcut property")?.to_owned());
                }
            }
        }
        if let Some(v) = props.get(b"CPACK_WIX_FILE_ID").filter(|v| !v.is_empty()) {
            parsed.custom_file_id = Some(require_utf8(v, "CPACK_WIX_FILE_ID")?.to_owned());
        }
        map.insert(normalized, parsed);
    }
    Ok(map)
}

fn normalize_install_prop_key(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let trimmed = replaced.trim_start_matches("./").trim_start_matches('/');
    if trimmed.is_empty() {
        ".".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Create a new WiX XML writer (`<Wix>` or `<Include>`) with version-specific default xmlns
/// and all custom namespaces from `CPACK_WIX_CUSTOM_XMLNS`.
pub(super) fn new_wix_writer(cfg: &WixConfig, is_include: bool) -> Result<XmlWriter, CpackError> {
    let mut w = XmlWriter::new("    ", 0);
    w.declaration();
    let root = if is_include { "Include" } else { "Wix" };
    w.start_element(root)?;
    w.attr("xmlns", cfg.version.default_xmlns())?;
    for (prefix, uri) in &cfg.custom_xmlns {
        w.attr(&format!("xmlns:{prefix}"), uri)?;
    }
    Ok(w)
}

/// Emit `cpack_variables.wxi` with WiX preprocessor escaping (`$` -> `$$`) on literal values.
pub(super) fn render_variables_wxi(cfg: &WixConfig) -> Result<String, CpackError> {
    let mut w = new_wix_writer(cfg, true)?;
    let defs: Vec<(&str, Option<&str>)> = vec![
        ("CPACK_WIX_PRODUCT_GUID", Some(cfg.product_guid.as_str())),
        ("CPACK_WIX_UPGRADE_GUID", Some(cfg.upgrade_guid.as_str())),
        ("CPACK_PACKAGE_VENDOR", Some(cfg.vendor.as_str())),
        ("CPACK_PACKAGE_NAME", Some(cfg.package_name.as_str())),
        ("CPACK_PACKAGE_VERSION", Some(cfg.package_version.as_str())),
        ("CPACK_WIX_LICENSE_RTF", Some(cfg.license_rtf.as_str())),
        ("CPACK_WIX_PRODUCT_ICON", cfg.product_icon.as_deref()),
        ("CPACK_WIX_UI_BANNER", cfg.ui_banner.as_deref()),
        ("CPACK_WIX_UI_DIALOG", cfg.ui_dialog.as_deref()),
        (
            "CPACK_WIX_PROGRAM_MENU_FOLDER",
            Some(cfg.program_menu_folder.as_str()),
        ),
        ("CPACK_WIX_UI_REF", Some(cfg.ui_ref.as_str())),
        (
            "CPACK_WIX_INSTALL_SCOPE",
            Some(cfg.install_scope.as_wix_str()),
        ),
        (
            "CPACK_WIX_CAB_PER_COMPONENT",
            cfg.cab_per_component.then_some("ON"),
        ),
    ];
    for (key, value) in defs {
        if let Some(val) = value {
            let escaped = escape_wix_preprocessor(val).replace('"', "&quot;");
            w.processing_instruction("define", &format!("{key}=\"{escaped}\""))?;
        }
    }
    w.end_element("Include")?;
    w.finish()
}

/// Emit `properties.wxi`.
pub(super) fn render_properties_wxi(cfg: &WixConfig) -> Result<String, CpackError> {
    let mut w = new_wix_writer(cfg, true)?;
    for (id, value) in &cfg.wix_properties {
        w.start_element("Property")?;
        w.attr("Id", id)?;
        w.attr("Value", &escape_wix_preprocessor(value))?;
        w.end_element("Property")?;
    }
    if !cfg.wix_properties.contains_key("ARPINSTALLLOCATION") {
        w.start_element("Property")?;
        w.attr("Id", "INSTALL_ROOT")?;
        w.attr("Secure", "yes")?;
        w.start_element("RegistrySearch")?;
        w.attr("Id", "FindInstallLocation")?;
        w.attr("Root", "HKLM")?;
        w.attr(
            "Key",
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\[WIX_UPGRADE_DETECTED]",
        )?;
        w.attr("Name", "InstallLocation")?;
        w.attr("Type", "raw")?;
        w.end_element("RegistrySearch")?;
        w.end_element("Property")?;

        w.start_element("SetProperty")?;
        w.attr("Id", "ARPINSTALLLOCATION")?;
        w.attr("Value", "[INSTALL_ROOT]")?;
        w.attr("After", "CostFinalize")?;
        w.end_element("SetProperty")?;
    }
    w.end_element("Include")?;
    w.finish()
}

/// Emit `product_fragment.wxi` (per-component `<Media>` + `#PRODUCT` patch fragment).
pub(super) fn render_product_fragment_wxi(
    cfg: &WixConfig,
    catalog: &ComponentCatalog,
    patches: &mut WixPatchSet,
) -> Result<String, CpackError> {
    let mut w = new_wix_writer(cfg, true)?;
    if cfg.cab_per_component {
        let count = catalog.components.len().max(1);
        for i in 1..=count {
            w.start_element("Media")?;
            w.attr("Id", &i.to_string())?;
            w.attr("Cabinet", &format!("media{i}.cab"))?;
            w.attr("EmbedCab", "yes")?;
            w.end_element("Media")?;
        }
    }
    w.force_non_self_closing();
    patches.apply("#PRODUCT", &mut w)?;
    w.end_element("Include")?;
    w.finish()
}

/// Rendered XML contents for `directories.wxs`, `files.wxs`, and `features.wxs`.
pub(super) struct RenderedWixSources {
    pub(super) directories_wxs: String,
    pub(super) files_wxs: String,
    pub(super) features_wxs: String,
}

/// Render `directories.wxs`, `files.wxs`, and `features.wxs` in one consistent pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_wix_sources(
    host: &dyn Host,
    staging_dir: &Path,
    expected_canon_staging: &Path,
    cfg: &WixConfig,
    catalog: &ComponentCatalog,
    install_props: &BTreeMap<String, InstallFileProps>,
    patches: &mut WixPatchSet,
    next_guid: &mut impl FnMut() -> String,
) -> Result<RenderedWixSources, CpackError> {
    let mut dirs_w = new_wix_writer(cfg, false)?;
    let mut files_w = new_wix_writer(cfg, false)?;
    let mut feats_w = new_wix_writer(cfg, false)?;

    dirs_w.start_element("Fragment")?;
    files_w.start_element("Fragment")?;
    feats_w.start_element("Fragment")?;

    if cfg.version == WixVersion::V3 {
        dirs_w.start_element("Directory")?;
        dirs_w.attr("Id", "TARGETDIR")?;
        dirs_w.attr("Name", "SourceDir")?;
    }

    let std_dir_tag = if cfg.version == WixVersion::V4 {
        "StandardDirectory"
    } else {
        "Directory"
    };
    let root_folder_id = cfg.root_folder_id();
    let has_std_dir = !root_folder_id.is_empty();
    if has_std_dir {
        dirs_w.start_element(std_dir_tag)?;
        dirs_w.attr("Id", &root_folder_id)?;
    }

    let install_segments: Vec<&str> = cfg
        .install_directory
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    if install_segments.is_empty() {
        return Err(error("CPACK_PACKAGE_INSTALL_DIRECTORY cannot be empty"));
    }
    for (idx, seg) in install_segments.iter().enumerate() {
        dirs_w.start_element("Directory")?;
        if idx + 1 == install_segments.len() {
            dirs_w.attr("Id", "INSTALL_ROOT")?;
        } else {
            dirs_w.attr("Id", &format!("INSTALL_PREFIX_{}", idx + 1))?;
        }
        dirs_w.attr("Name", &escape_wix_preprocessor(seg))?;
    }

    let root_remove_comp = emit_remove_folder_on_user_install(&mut dirs_w, cfg, "INSTALL_ROOT")?;

    feats_w.start_element("Feature")?;
    feats_w.attr("Id", "ProductFeature")?;
    feats_w.attr("Display", "expand")?;
    if cfg.version == WixVersion::V4 {
        feats_w.attr("AllowAbsent", "no")?;
    } else {
        feats_w.attr("Absent", "disallow")?;
    }
    feats_w.attr("ConfigurableDirectory", "INSTALL_ROOT")?;
    feats_w.attr("Title", &escape_wix_preprocessor(&cfg.root_feature_title))?;
    if let Some(ref desc) = cfg.root_feature_description {
        feats_w.attr("Description", &escape_wix_preprocessor(desc))?;
    }
    feats_w.attr("Level", "1")?;
    patches.apply("#PRODUCTFEATURE", &mut feats_w)?;

    if let Some(ref cmake_pkg) = cfg.cmake_package_registry {
        emit_cmake_package_registry(&mut feats_w, cfg, cmake_pkg)?;
    }
    if let Some(ref comp_id) = root_remove_comp {
        emit_component_ref(&mut feats_w, comp_id)?;
    }

    for group in catalog.groups.values() {
        if group.parent_group.is_none() {
            emit_group_feature(&mut feats_w, cfg, catalog, group, patches)?;
        }
    }
    for comp in catalog.components.values() {
        if comp.group.is_none() {
            emit_component_feature(&mut feats_w, cfg, comp, patches)?;
        }
    }
    feats_w.end_element("Feature")?;

    let mut allocator = PathIdAllocator::default();
    let mut emitted_shortcut_kinds = BTreeSet::new();
    let mut global_shortcuts = ShortcutTable::default();
    let canon_staging = verify_expected_root(host, staging_dir, expected_canon_staging)?;

    if catalog.components.is_empty() {
        feats_w.start_element("FeatureRef")?;
        feats_w.attr("Id", "ProductFeature")?;
        walk_and_emit_dir(
            host,
            staging_dir,
            &canon_staging,
            staging_dir,
            "INSTALL_ROOT",
            false,
            0,
            cfg,
            install_props,
            patches,
            &mut allocator,
            &mut dirs_w,
            &mut files_w,
            &mut feats_w,
            &mut global_shortcuts,
            next_guid,
            0,
        )?;
        feats_w.end_element("FeatureRef")?;
        global_shortcuts.collect_kinds(&mut emitted_shortcut_kinds);
    } else {
        let windows_host = crate::generators::installer::is_windows_host(host);
        for (idx, comp_name) in catalog.components.keys().enumerate() {
            let disk_id = if cfg.cab_per_component { idx + 1 } else { 0 };
            let comp_str = require_utf8(comp_name, "WiX component name")?;
            let comp_dir = crate::stage::component_dir_for_host(comp_name, windows_host);
            let comp_path = staging_dir.join(&comp_dir);
            let feature_id = format!("CM_C_{comp_str}");
            let mut comp_shortcuts = ShortcutTable::default();
            feats_w.start_element("FeatureRef")?;
            feats_w.attr("Id", &feature_id)?;
            if host.metadata_fresh(&comp_path, Follow::No).is_ok() {
                walk_and_emit_dir(
                    host,
                    staging_dir,
                    &canon_staging,
                    &comp_path,
                    "INSTALL_ROOT",
                    true,
                    disk_id,
                    cfg,
                    install_props,
                    patches,
                    &mut allocator,
                    &mut dirs_w,
                    &mut files_w,
                    &mut feats_w,
                    &mut comp_shortcuts,
                    next_guid,
                    0,
                )?;
            }
            feats_w.end_element("FeatureRef")?;
            comp_shortcuts.collect_kinds(&mut emitted_shortcut_kinds);
            emit_shortcuts_bundle(
                cfg,
                comp_str,
                &feature_id,
                &comp_shortcuts,
                false,
                patches,
                &mut files_w,
                &mut feats_w,
            )?;
        }
    }

    let emit_uninstall_shortcut =
        cfg.program_menu_folder != "." && emitted_shortcut_kinds.contains(&ShortcutKind::StartMenu);
    emit_shortcuts_bundle(
        cfg,
        "",
        "ProductFeature",
        &global_shortcuts,
        emit_uninstall_shortcut,
        patches,
        &mut files_w,
        &mut feats_w,
    )?;

    for _ in 0..install_segments.len() {
        dirs_w.end_element("Directory")?;
    }
    if has_std_dir {
        dirs_w.end_element(std_dir_tag)?;
    }

    if emitted_shortcut_kinds.contains(&ShortcutKind::StartMenu) {
        dirs_w.start_element(std_dir_tag)?;
        dirs_w.attr("Id", "ProgramMenuFolder")?;
        if cfg.program_menu_folder != "." {
            dirs_w.start_element("Directory")?;
            dirs_w.attr("Id", "PROGRAM_MENU_FOLDER")?;
            dirs_w.attr("Name", &escape_wix_preprocessor(&cfg.program_menu_folder))?;
            dirs_w.end_element("Directory")?;
        }
        dirs_w.end_element(std_dir_tag)?;
    }
    if emitted_shortcut_kinds.contains(&ShortcutKind::Desktop) {
        dirs_w.start_element(std_dir_tag)?;
        dirs_w.attr("Id", "DesktopFolder")?;
        if cfg.version == WixVersion::V3 {
            dirs_w.attr("Name", "Desktop")?;
        }
        dirs_w.end_element(std_dir_tag)?;
    }
    if emitted_shortcut_kinds.contains(&ShortcutKind::Startup) {
        dirs_w.start_element(std_dir_tag)?;
        dirs_w.attr("Id", "StartupFolder")?;
        if cfg.version == WixVersion::V3 {
            dirs_w.attr("Name", "Startup")?;
        }
        dirs_w.end_element(std_dir_tag)?;
    }

    if cfg.version == WixVersion::V3 {
        dirs_w.end_element("Directory")?;
    }
    dirs_w.end_element("Fragment")?;
    dirs_w.end_element("Wix")?;
    files_w.end_element("Fragment")?;
    files_w.end_element("Wix")?;
    feats_w.end_element("Fragment")?;
    feats_w.end_element("Wix")?;

    Ok(RenderedWixSources {
        directories_wxs: dirs_w.finish()?,
        files_wxs: files_w.finish()?,
        features_wxs: feats_w.finish()?,
    })
}

fn component_guid(cfg: &WixConfig, component_id: &str, force_cmake_guid: bool) -> String {
    if force_cmake_guid || cfg.skip_program_folder {
        guid_from_md5_component_id(component_id)
    } else {
        "*".to_owned()
    }
}

fn emit_remove_folder_on_user_install(
    dirs_w: &mut XmlWriter,
    cfg: &WixConfig,
    dir_id: &str,
) -> Result<Option<String>, CpackError> {
    if cfg.install_scope != InstallScope::PerUser {
        return Ok(None);
    }
    let comp_id = format!("{dir_id}_RemoveFolderComponent");
    dirs_w.start_element("Component")?;
    dirs_w.attr("Id", &comp_id)?;
    dirs_w.attr("Guid", &component_guid(cfg, &comp_id, true))?;
    dirs_w.start_element("RemoveFolder")?;
    dirs_w.attr("Id", &format!("{dir_id}_RemoveFolder"))?;
    dirs_w.attr("Directory", dir_id)?;
    dirs_w.attr("On", "uninstall")?;
    dirs_w.end_element("RemoveFolder")?;
    dirs_w.start_element("RegistryValue")?;
    dirs_w.attr("Root", "HKCU")?;
    dirs_w.attr(
        "Key",
        &escape_wix_preprocessor(&cfg.component_keys_registry_path),
    )?;
    dirs_w.attr("Name", &comp_id)?;
    dirs_w.attr("Type", "string")?;
    dirs_w.attr("Value", "1")?;
    dirs_w.attr("KeyPath", "yes")?;
    dirs_w.end_element("RegistryValue")?;
    dirs_w.end_element("Component")?;
    Ok(Some(comp_id))
}

fn emit_cmake_package_registry(
    feats_w: &mut XmlWriter,
    cfg: &WixConfig,
    package: &str,
) -> Result<(), CpackError> {
    feats_w.start_element("Component")?;
    feats_w.attr("Id", "CM_PACKAGE_REGISTRY")?;
    feats_w.attr("Directory", "TARGETDIR")?;
    feats_w.attr("Guid", &component_guid(cfg, "CM_PACKAGE_REGISTRY", false))?;
    feats_w.start_element("RegistryValue")?;
    feats_w.attr("Root", "HKLM")?;
    feats_w.attr(
        "Key",
        &escape_wix_preprocessor(&format!("Software\\Kitware\\CMake\\Packages\\{package}")),
    )?;
    feats_w.attr("Name", &cfg.upgrade_guid)?;
    feats_w.attr("Type", "string")?;
    feats_w.attr("Value", "[INSTALL_ROOT]")?;
    feats_w.attr("KeyPath", "yes")?;
    feats_w.end_element("RegistryValue")?;
    feats_w.end_element("Component")
}

fn emit_component_ref(feats_w: &mut XmlWriter, id: &str) -> Result<(), CpackError> {
    feats_w.start_element("ComponentRef")?;
    feats_w.attr("Id", id)?;
    feats_w.end_element("ComponentRef")
}

enum FeatureGroupStep<'a> {
    Enter(&'a GroupMeta),
    EmitComponentsAndClose(&'a GroupMeta),
}

fn emit_group_feature(
    feats_w: &mut XmlWriter,
    cfg: &WixConfig,
    catalog: &ComponentCatalog,
    root_group: &GroupMeta,
    patches: &mut WixPatchSet,
) -> Result<(), CpackError> {
    let mut stack = vec![FeatureGroupStep::Enter(root_group)];
    while let Some(step) = stack.pop() {
        match step {
            FeatureGroupStep::Enter(group) => {
                let gname = require_utf8(&group.name, "WiX group name")?;
                let id = format!("CM_G_{gname}");
                feats_w.start_element("Feature")?;
                feats_w.attr("Id", &id)?;
                if group.expanded {
                    feats_w.attr("Display", "expand")?;
                }
                let title = require_utf8(&group.display_name, "WiX group display name")?;
                if !title.is_empty() {
                    feats_w.attr("Title", &escape_wix_preprocessor(title))?;
                }
                let desc = require_utf8(&group.description, "WiX group description")?;
                if !desc.is_empty() {
                    feats_w.attr("Description", &escape_wix_preprocessor(desc))?;
                }
                patches.apply(&id, feats_w)?;
                stack.push(FeatureGroupStep::EmitComponentsAndClose(group));
                for sub_name in group.subgroups.iter().rev() {
                    if let Some(sub) = catalog.groups.get(sub_name) {
                        stack.push(FeatureGroupStep::Enter(sub));
                    }
                }
            }
            FeatureGroupStep::EmitComponentsAndClose(group) => {
                for comp_name in &group.components {
                    if let Some(comp) = catalog.components.get(comp_name) {
                        emit_component_feature(feats_w, cfg, comp, patches)?;
                    }
                }
                feats_w.end_element("Feature")?;
            }
        }
    }
    Ok(())
}

fn emit_component_feature(
    feats_w: &mut XmlWriter,
    cfg: &WixConfig,
    comp: &ComponentMeta,
    patches: &mut WixPatchSet,
) -> Result<(), CpackError> {
    let cname = require_utf8(&comp.name, "WiX component name")?;
    let id = format!("CM_C_{cname}");
    feats_w.start_element("Feature")?;
    feats_w.attr("Id", &id)?;
    let title = require_utf8(&comp.display_name, "WiX component display name")?;
    if !title.is_empty() {
        feats_w.attr("Title", &escape_wix_preprocessor(title))?;
    }
    let desc = require_utf8(&comp.description, "WiX component description")?;
    if !desc.is_empty() {
        feats_w.attr("Description", &escape_wix_preprocessor(desc))?;
    }
    if comp.required {
        if cfg.version == WixVersion::V4 {
            feats_w.attr("AllowAbsent", "no")?;
        } else {
            feats_w.attr("Absent", "disallow")?;
        }
    }
    if comp.hidden {
        feats_w.attr("Display", "hidden")?;
    }
    if comp.disabled {
        feats_w.attr("Level", "2")?;
    }
    patches.apply(&id, feats_w)?;
    feats_w.end_element("Feature")
}

#[allow(clippy::too_many_arguments)]
fn walk_and_emit_dir(
    host: &dyn Host,
    staging_dir: &Path,
    canon_staging: &Path,
    dir: &Path,
    dir_id: &str,
    has_component_prefix: bool,
    disk_id: usize,
    cfg: &WixConfig,
    install_props: &BTreeMap<String, InstallFileProps>,
    patches: &mut WixPatchSet,
    allocator: &mut PathIdAllocator,
    dirs_w: &mut XmlWriter,
    files_w: &mut XmlWriter,
    feats_w: &mut XmlWriter,
    shortcuts: &mut ShortcutTable,
    next_guid: &mut impl FnMut() -> String,
    depth: usize,
) -> Result<(), CpackError> {
    if depth >= 256 {
        return Err(error("WiX staging directory depth exceeds the limit"));
    }
    let mut entries = host.read_dir(dir)?;
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    entries.retain(|e| !e.name.starts_with(b"._"));

    let rel_dir = dir.strip_prefix(staging_dir).unwrap_or(Path::new(""));
    let rel_dir_str = from_native_cmake(rel_dir).to_string();
    let prop_dir_key = strip_component_prefix(&rel_dir_str, has_component_prefix);
    let dir_props = install_props.get(&prop_dir_key);

    let is_empty = entries.is_empty();
    let mut create_folder = is_empty && dir_id != "INSTALL_ROOT";
    if dir_props.is_some_and(|p| !p.acls.is_empty()) {
        create_folder = true;
    }
    if create_folder {
        let comp_id = format!("CM_C_EMPTY_{dir_id}");
        files_w.start_element("DirectoryRef")?;
        files_w.attr("Id", dir_id)?;
        files_w.start_element("Component")?;
        files_w.attr("Id", &comp_id)?;
        files_w.attr("Guid", &next_guid())?;
        files_w.start_element("CreateFolder")?;
        if let Some(props) = dir_props {
            emit_acls(&props.acls, files_w)?;
        }
        files_w.end_element("CreateFolder")?;
        files_w.end_element("Component")?;
        files_w.end_element("DirectoryRef")?;
        emit_component_ref(feats_w, &comp_id)?;
    }
    if is_empty {
        return Ok(());
    }

    for entry in entries {
        let name = to_native(&entry.name);
        let full_path = dir.join(&name);
        let meta = host.metadata_fresh(&full_path, Follow::No)?;
        if !host
            .canonicalize_fresh(&full_path)?
            .starts_with(canon_staging)
        {
            return Err(error(format!(
                "WiX staged entry escapes staging root: {}",
                full_path.display()
            )));
        }
        let rel_path = full_path.strip_prefix(staging_dir).unwrap_or(&full_path);
        let rel_str = from_native_cmake(rel_path).to_string();
        let file_name = require_utf8(&entry.name, "WiX payload file name")?;
        let id = allocator.id_for_path(&rel_str)?;

        match meta.kind {
            FileKind::Dir => {
                let sub_dir_id = format!("CM_D{id}");
                dirs_w.start_element("Directory")?;
                dirs_w.attr("Id", &sub_dir_id)?;
                dirs_w.attr("Name", &escape_wix_preprocessor(file_name))?;
                patches.apply(&sub_dir_id, dirs_w)?;
                if let Some(rm_id) = emit_remove_folder_on_user_install(dirs_w, cfg, &sub_dir_id)? {
                    emit_component_ref(feats_w, &rm_id)?;
                }
                walk_and_emit_dir(
                    host,
                    staging_dir,
                    canon_staging,
                    &full_path,
                    &sub_dir_id,
                    has_component_prefix,
                    disk_id,
                    cfg,
                    install_props,
                    patches,
                    allocator,
                    dirs_w,
                    files_w,
                    feats_w,
                    shortcuts,
                    next_guid,
                    depth + 1,
                )?;
                dirs_w.end_element("Directory")?;
            }
            FileKind::File => {
                let prop_key = strip_component_prefix(&rel_str, has_component_prefix);
                let file_props = install_props.get(&prop_key);
                let comp_id = format!("CM_C{id}");
                let file_id = file_props
                    .and_then(|p| p.custom_file_id.clone())
                    .unwrap_or_else(|| format!("CM_F{id}"));
                let per_user = cfg.install_scope == InstallScope::PerUser;
                let guid = component_guid(cfg, &comp_id, per_user);

                files_w.start_element("DirectoryRef")?;
                files_w.attr("Id", dir_id)?;
                files_w.start_element("Component")?;
                files_w.attr("Id", &comp_id)?;
                files_w.attr("Guid", &guid)?;
                if disk_id > 0 {
                    files_w.attr("DiskId", &disk_id.to_string())?;
                }
                if let Some(props) = file_props {
                    if props.never_overwrite {
                        files_w.attr("NeverOverwrite", "yes")?;
                    }
                    if props.permanent {
                        files_w.attr("Permanent", "yes")?;
                    }
                }
                patches.apply(&comp_id, files_w)?;

                if per_user {
                    files_w.start_element("RegistryValue")?;
                    files_w.attr("Root", "HKCU")?;
                    files_w.attr(
                        "Key",
                        &escape_wix_preprocessor(&cfg.component_keys_registry_path),
                    )?;
                    files_w.attr("Name", &file_id)?;
                    files_w.attr("Type", "string")?;
                    files_w.attr("Value", "1")?;
                    files_w.attr("KeyPath", "yes")?;
                    files_w.end_element("RegistryValue")?;
                }

                files_w.start_element("File")?;
                files_w.attr("Id", &file_id)?;
                let source_path = if cfg.windows_host && cfg.version == WixVersion::V4 {
                    format!("\\\\?\\{}", to_windows_slashes(&full_path))
                } else {
                    from_native_cmake(&full_path).to_string()
                };
                files_w.attr("Source", &escape_wix_preprocessor(&source_path))?;
                if !per_user {
                    files_w.attr("KeyPath", "yes")?;
                }
                if meta.mode & 0o200 == 0 {
                    files_w.attr("ReadOnly", "yes")?;
                }
                patches.apply(&file_id, files_w)?;
                if let Some(props) = file_props {
                    emit_acls(&props.acls, files_w)?;
                    shortcuts.add_from_props(&id, &file_id, dir_id, props);
                }
                files_w.end_element("File")?;
                files_w.end_element("Component")?;
                files_w.end_element("DirectoryRef")?;
                emit_component_ref(feats_w, &comp_id)?;

                // Match CPACK_PACKAGE_EXECUTABLES (<exe>.exe case-insensitive)
                let lower_name = file_name.to_ascii_lowercase();
                for (exe, label) in &cfg.package_executables {
                    if lower_name == format!("{}.exe", exe.to_ascii_lowercase()) {
                        let entry = ShortcutEntry {
                            label: label.clone(),
                            working_directory_id: dir_id.to_owned(),
                            file_id: file_id.clone(),
                        };
                        shortcuts.insert(ShortcutKind::StartMenu, id.clone(), entry.clone());
                        if cfg.desktop_links.contains(exe) {
                            shortcuts.insert(ShortcutKind::Desktop, id.clone(), entry);
                        }
                    }
                }
            }
            FileKind::Symlink | FileKind::Other => {
                return Err(error(format!(
                    "Unsupported entry kind in WiX payload: {}",
                    full_path.display()
                )));
            }
        }
    }
    Ok(())
}

fn strip_component_prefix(rel_path: &str, has_component_prefix: bool) -> String {
    if !has_component_prefix {
        return normalize_install_prop_key(rel_path);
    }
    match rel_path.split_once('/') {
        Some((_, rest)) => normalize_install_prop_key(rest),
        None => ".".to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_shortcuts_bundle(
    cfg: &WixConfig,
    component_name: &str,
    feature_id: &str,
    shortcuts: &ShortcutTable,
    emit_uninstall: bool,
    patches: &mut WixPatchSet,
    files_w: &mut XmlWriter,
    feats_w: &mut XmlWriter,
) -> Result<(), CpackError> {
    for (kind, id_prefix, shortcut_prefix, reg_suffix) in [
        (ShortcutKind::StartMenu, "", "CM_S", ""),
        (ShortcutKind::Desktop, "DESKTOP", "CM_DS", "_desktop"),
        (ShortcutKind::Startup, "STARTUP", "CM_SS", "_startup"),
    ] {
        let should_emit =
            !shortcuts.is_empty(kind) || (kind == ShortcutKind::StartMenu && emit_uninstall);
        if !should_emit {
            continue;
        }
        let (dir_ref, dir_id) = match kind {
            ShortcutKind::StartMenu if cfg.program_menu_folder == "." => (
                if cfg.version == WixVersion::V4 {
                    "StandardDirectory"
                } else {
                    "DirectoryRef"
                },
                "ProgramMenuFolder",
            ),
            ShortcutKind::StartMenu => ("DirectoryRef", "PROGRAM_MENU_FOLDER"),
            ShortcutKind::Desktop => (
                if cfg.version == WixVersion::V4 {
                    "StandardDirectory"
                } else {
                    "DirectoryRef"
                },
                "DesktopFolder",
            ),
            ShortcutKind::Startup => (
                if cfg.version == WixVersion::V4 {
                    "StandardDirectory"
                } else {
                    "DirectoryRef"
                },
                "StartupFolder",
            ),
        };
        let suffix = if component_name.is_empty() {
            String::new()
        } else {
            format!("_{component_name}")
        };
        let mut comp_id = String::from("CM_SHORTCUT");
        if !id_prefix.is_empty() {
            comp_id.push('_');
            comp_id.push_str(id_prefix);
        }
        comp_id.push_str(&suffix);

        feats_w.start_element("FeatureRef")?;
        feats_w.attr("Id", feature_id)?;

        files_w.start_element(dir_ref)?;
        files_w.attr("Id", dir_id)?;
        files_w.start_element("Component")?;
        files_w.attr("Id", &comp_id)?;
        files_w.attr("Guid", &component_guid(cfg, &comp_id, false))?;
        patches.apply(&comp_id, files_w)?;

        if let Some(id_map) = shortcuts.by_kind.get(&kind) {
            for (id, list) in id_map {
                for (idx, sc) in list.iter().enumerate() {
                    let sc_id = if idx > 0 {
                        format!("{shortcut_prefix}{id}_{idx}")
                    } else {
                        format!("{shortcut_prefix}{id}")
                    };
                    files_w.start_element("Shortcut")?;
                    files_w.attr("Id", &sc_id)?;
                    files_w.attr("Name", &escape_wix_preprocessor(&sc.label))?;
                    files_w.attr("Target", &format!("[#{}]", sc.file_id))?;
                    files_w.attr("WorkingDirectory", &sc.working_directory_id)?;
                    files_w.end_element("Shortcut")?;
                }
            }
            let reg_name = if component_name.is_empty() {
                format!("installed{reg_suffix}")
            } else {
                format!("{component_name}_installed{reg_suffix}")
            };
            files_w.start_element("RegistryValue")?;
            files_w.attr("Root", "HKCU")?;
            files_w.attr(
                "Key",
                &escape_wix_preprocessor(&format!(
                    "Software\\{}\\{}",
                    cfg.vendor, cfg.package_name
                )),
            )?;
            files_w.attr("Name", &reg_name)?;
            files_w.attr("Type", "integer")?;
            files_w.attr("Value", "1")?;
            files_w.attr("KeyPath", "yes")?;
            files_w.end_element("RegistryValue")?;
        }

        if kind == ShortcutKind::StartMenu && cfg.program_menu_folder != "." {
            files_w.start_element("RemoveFolder")?;
            files_w.attr("Id", &format!("CM_REMOVE_PROGRAM_MENU_FOLDER{suffix}"))?;
            files_w.attr("On", "uninstall")?;
            files_w.end_element("RemoveFolder")?;
        }
        if kind == ShortcutKind::StartMenu && emit_uninstall {
            files_w.start_element("Shortcut")?;
            files_w.attr("Id", "UNINSTALL")?;
            files_w.attr(
                "Name",
                &escape_wix_preprocessor(&format!("Uninstall {}", cfg.package_name)),
            )?;
            files_w.attr(
                "Description",
                &escape_wix_preprocessor(&format!("Uninstalls {}", cfg.package_name)),
            )?;
            files_w.attr("Target", "[SystemFolder]msiexec.exe")?;
            files_w.attr("Arguments", "/x [ProductCode]")?;
            files_w.end_element("Shortcut")?;
        }

        files_w.end_element("Component")?;
        files_w.end_element(dir_ref)?;

        emit_component_ref(feats_w, &comp_id)?;
        feats_w.end_element("FeatureRef")?;
    }
    Ok(())
}
