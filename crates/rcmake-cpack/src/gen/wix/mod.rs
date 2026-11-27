//! Windows Installer XML (`WIX` v3 and v4) package generator (`.msi`).

mod patch;
pub(crate) mod render;
mod rtf;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_base::hash::HashAlgo;
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, ReadRange, WriteMode};

use self::patch::WixPatchSet;
use self::render::{
    collect_install_props, render_product_fragment_wxi, render_properties_wxi,
    render_variables_wxi, render_wix_sources,
};
use super::installer::{
    ComponentCatalog, build_tool_timeout, check_tool_outcome, escape_xml_attr,
    find_program_on_host, find_template, is_windows_host, option_set, prepare_fresh_output,
    read_template, render_at_template, require_utf8, run_installer_tool, validate_xml_ncname,
    verify_fresh_output, warn, write_tool_log,
};
use crate::config::{CpackConfig, absolute, script, snapshot};
use crate::source::{SymlinkPolicy, validate_staged_tree};
use crate::{ComponentMembers, CpackError, PackageContext, PackageGenerator, error, safe_name};

/// The `WIX` (v3 and v4) generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Wix;

/// Supported WiX Toolset major version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WixVersion {
    V3,
    V4,
}

impl WixVersion {
    pub(crate) fn parse(raw: Option<&[u8]>) -> Result<Self, CpackError> {
        let Some(raw) = raw else {
            return Ok(Self::V3);
        };
        let text = require_utf8(raw, "CPACK_WIX_VERSION")?;
        match text.trim() {
            "3" => Ok(Self::V3),
            "4" => Ok(Self::V4),
            _ => Err(error(format!(
                "CPACK_WIX_VERSION has unknown value '{text}'"
            ))),
        }
    }

    pub(crate) const fn default_xmlns(self) -> &'static str {
        match self {
            Self::V3 => "http://schemas.microsoft.com/wix/2006/wi",
            Self::V4 => "http://wixtoolset.org/schemas/v4/wxs",
        }
    }
}

/// Supported MSI install scopes (`CPACK_WIX_INSTALL_SCOPE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallScope {
    PerUser,
    PerMachine,
    None,
}

impl InstallScope {
    pub(crate) fn parse(raw: Option<&[u8]>, version: WixVersion) -> Result<Self, CpackError> {
        let scope = match raw {
            None => match version {
                WixVersion::V4 => Self::PerMachine,
                WixVersion::V3 => Self::None,
            },
            Some(b"perUser") => Self::PerUser,
            Some(b"perMachine") => Self::PerMachine,
            Some(b"NONE") => Self::None,
            Some(other) => {
                return Err(error(format!(
                    "Invalid CPACK_WIX_INSTALL_SCOPE value: '{}'",
                    BString::from(other)
                )));
            }
        };
        if version == WixVersion::V4 && scope == Self::None {
            return Err(error(
                "CPACK_WIX_INSTALL_SCOPE value 'NONE' is not supported with WiX version 4",
            ));
        }
        Ok(scope)
    }

    pub(crate) const fn as_wix_str(self) -> &'static str {
        match self {
            Self::PerUser => "perUser",
            Self::PerMachine => "perMachine",
            Self::None => "NONE",
        }
    }
}

/// Resolved WiX configuration for one packaging pass.
pub(super) struct WixConfig {
    pub(super) version: WixVersion,
    pub(super) install_scope: InstallScope,
    pub(super) architecture: String,
    pub(super) windows_host: bool,
    pub(super) package_name: String,
    pub(super) package_version: String,
    pub(super) vendor: String,
    pub(super) install_directory: String,
    pub(super) product_guid: String,
    pub(super) upgrade_guid: String,
    pub(super) license_rtf: String,
    pub(super) product_icon: Option<String>,
    pub(super) ui_banner: Option<String>,
    pub(super) ui_dialog: Option<String>,
    pub(super) ui_ref: String,
    pub(super) root_feature_title: String,
    pub(super) root_feature_description: Option<String>,
    pub(super) root_folder_override: Option<String>,
    pub(super) skip_program_folder: bool,
    pub(super) program_menu_folder: String,
    pub(super) cmake_package_registry: Option<String>,
    pub(super) cab_per_component: bool,
    pub(super) component_keys_registry_path: String,
    pub(super) custom_xmlns: BTreeMap<String, String>,
    pub(super) wix_properties: BTreeMap<String, String>,
    pub(super) package_executables: Vec<(String, String)>,
    pub(super) desktop_links: BTreeSet<String>,
    pub(super) wix_extensions: BTreeSet<String>,
    pub(super) candle_extensions: BTreeSet<String>,
    pub(super) light_extensions: BTreeSet<String>,
}

impl WixConfig {
    pub(super) fn root_folder_id(&self) -> String {
        if self.skip_program_folder {
            return String::new();
        }
        let template = if let Some(ref custom) = self.root_folder_override {
            custom.clone()
        } else if self.install_scope == InstallScope::PerUser {
            "LocalAppDataFolder".to_owned()
        } else if self.version == WixVersion::V4 {
            "ProgramFiles6432Folder".to_owned()
        } else {
            "ProgramFiles<64>Folder".to_owned()
        };
        if self.architecture == "x86" {
            template.replace("<64>", "")
        } else {
            template.replace("<64>", "64")
        }
    }
}

impl PackageGenerator for Wix {
    fn name(&self) -> &'static str {
        "WIX"
    }

    fn extension(&self) -> &'static str {
        ".msi"
    }

    fn prepare_config(
        &self,
        host: &dyn Host,
        interp: &mut Interp,
        cwd: &Path,
    ) -> Result<(), CpackError> {
        let cfg = snapshot(interp);
        let version = WixVersion::parse(option_set(&cfg, b"CPACK_WIX_VERSION"))?;
        {
            let mut cx = interp.cx();
            if !cx.var_defined(b"CPACK_WIX_VERSION") {
                cx.set_var(
                    b"CPACK_WIX_VERSION",
                    match version {
                        WixVersion::V3 => "3".into(),
                        WixVersion::V4 => "4".into(),
                    },
                );
            }
        }

        let cmake_root = interp.cx().var_or_empty(b"CMAKE_ROOT");
        let module_path = (!cmake_root.is_empty())
            .then(|| absolute(cwd, &cmake_root).join("Modules/Internal/CPack/CPackWIX.cmake"));
        if let Some(module) = module_path.filter(|p| {
            host.metadata(p, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        }) {
            if script(interp, &module).is_err() {
                return Err(error("Error while executing CPackWIX.cmake"));
            }
        } else {
            fallback_wix_discovery(host, interp, cwd, version)?;
        }

        let cfg = snapshot(interp);
        let _ = InstallScope::parse(option_set(&cfg, b"CPACK_WIX_INSTALL_SCOPE"), version)?;

        match version {
            WixVersion::V4 => {
                if option_set(&cfg, b"CPACK_WIX_EXECUTABLE").is_none() {
                    return Err(error("Could not find the 'wix' executable."));
                }
            }
            WixVersion::V3 => {
                if option_set(&cfg, b"CPACK_WIX_CANDLE_EXECUTABLE").is_none() {
                    return Err(error("Could not find the WiX candle executable."));
                }
                if option_set(&cfg, b"CPACK_WIX_LIGHT_EXECUTABLE").is_none() {
                    return Err(error("Could not find the WiX light executable."));
                }
            }
        }
        Ok(())
    }

    fn package(&self, _cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        Err(error("CPack WIX generator requires a CPack interpreter"))
    }

    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
        interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let host = cx.host;
        let cwd = host.initial_cwd();
        let canon_work = cx.expected_work_root()?;
        let canon_staging = cx.expected_staging_root()?;

        if let Some(abs_files) = option_set(&cfg, b"CPACK_ABSOLUTE_DESTINATION_FILES") {
            return Err(error(format!(
                "WiX generator does not support absolute install destinations: {}",
                BString::from(abs_files)
            )));
        }

        validate_staged_tree(
            host,
            cx.staging_dir,
            &canon_staging,
            members,
            SymlinkPolicy::Reject,
            true,
        )?;

        let catalog = ComponentCatalog::from_config(&cfg, members)?;
        let version = WixVersion::parse(option_set(&cfg, b"CPACK_WIX_VERSION"))?;
        let install_scope =
            InstallScope::parse(option_set(&cfg, b"CPACK_WIX_INSTALL_SCOPE"), version)?;

        let mut fixed_guids = cfg
            .list(b"CPACK_WIX_TEST_FIXED_GUIDS")
            .into_iter()
            .map(|g| validate_guid(require_utf8(&g, "CPACK_WIX_TEST_FIXED_GUIDS")?))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        let mut next_guid = move || -> String {
            fixed_guids
                .next()
                .unwrap_or_else(|| generate_host_guid(host))
        };

        let wix_cfg = build_wix_config(
            host,
            &cfg,
            cx.output_dir,
            &cwd,
            version,
            install_scope,
            &catalog,
            &mut next_guid,
        )?;

        let mut patches = WixPatchSet::default();
        for patch_entry in cfg.list(b"CPACK_WIX_PATCH_FILE") {
            let patch_path = absolute(&cwd, &patch_entry);
            patches.load_file(host, &patch_path)?;
        }

        let install_props = collect_install_props(interp)?;

        let vars_wxi = render_variables_wxi(&wix_cfg)?;
        write_wix_file(host, &cx.output_dir.join("cpack_variables.wxi"), &vars_wxi)?;
        let props_wxi = render_properties_wxi(&wix_cfg)?;
        write_wix_file(host, &cx.output_dir.join("properties.wxi"), &props_wxi)?;
        let prod_wxi = render_product_fragment_wxi(&wix_cfg, &catalog, &mut patches)?;
        write_wix_file(host, &cx.output_dir.join("product_fragment.wxi"), &prod_wxi)?;

        let rendered = render_wix_sources(
            host,
            cx.staging_dir,
            &canon_staging,
            &wix_cfg,
            &catalog,
            &install_props,
            &mut patches,
            &mut next_guid,
        )?;
        let dirs_path = cx.output_dir.join("directories.wxs");
        let files_path = cx.output_dir.join("files.wxs");
        let feats_path = cx.output_dir.join("features.wxs");
        write_wix_file(host, &dirs_path, &rendered.directories_wxs)?;
        write_wix_file(host, &files_path, &rendered.files_wxs)?;
        write_wix_file(host, &feats_path, &rendered.features_wxs)?;

        let main_template_path = match option_set(&cfg, b"CPACK_WIX_TEMPLATE") {
            Some(tpl) => absolute(&cwd, tpl),
            None => {
                let alt = (version == WixVersion::V3).then_some("WIX-v3/");
                find_template(host, &cfg, &cwd, "WIX.template.in", alt)?
            }
        };
        let main_template = read_template(host, &main_template_path)?;
        let xmlns_expanded = build_expanded_xmlns(&wix_cfg)?;
        let mut main_vars = BTreeMap::new();
        for (k, v) in &cfg.vars {
            if let (Ok(ks), Ok(vs)) = (std::str::from_utf8(k), std::str::from_utf8(v)) {
                main_vars.insert(ks.to_owned(), vs.to_owned());
            }
        }
        main_vars.insert("CPACK_WIX_CUSTOM_XMLNS_EXPANDED".to_owned(), xmlns_expanded);
        let main_wxs = render_at_template(&main_template, &main_vars);
        let main_path = cx.output_dir.join("main.wxs");
        write_wix_file(host, &main_path, &main_wxs)?;

        patches.check_all_applied()?;

        let mut sources = vec![dirs_path, files_path, feats_path, main_path];
        for extra in cfg.list(b"CPACK_WIX_EXTRA_SOURCES") {
            sources.push(absolute(&cwd, &extra));
        }

        let out_msi = cx
            .output_dir
            .join(safe_name(&[cx.file_name, b".msi"].concat())?);
        prepare_fresh_output(host, &out_msi)?;

        let log_path = cx.output_dir.join("wix.log");
        prepare_fresh_output(host, &log_path)?;

        match version {
            WixVersion::V3 => {
                run_wix_v3(
                    host,
                    &cfg,
                    cx.output_dir,
                    &canon_work,
                    &cwd,
                    &wix_cfg,
                    &sources,
                    &out_msi,
                    &log_path,
                )?;
            }
            WixVersion::V4 => {
                run_wix_v4(
                    host,
                    &cfg,
                    cx.output_dir,
                    &wix_cfg,
                    &sources,
                    &out_msi,
                    &log_path,
                )?;
            }
        }
        verify_fresh_output(host, cx.output_dir, &canon_work, &out_msi, "WiX")?;
        Ok(vec![out_msi])
    }
}

fn fallback_wix_discovery(
    host: &dyn Host,
    interp: &mut Interp,
    cwd: &Path,
    version: WixVersion,
) -> Result<(), CpackError> {
    let cfg = snapshot(interp);
    let mut roots = Vec::new();
    if let Some(root) = option_set(&cfg, b"CPACK_WIX_ROOT") {
        roots.push(absolute(cwd, root));
    } else if let Some(env_wix) = host.env_get(b"WIX").filter(|v| !v.is_empty()) {
        roots.push(absolute(cwd, &env_wix));
    }
    match version {
        WixVersion::V4 => {
            if option_set(&cfg, b"CPACK_WIX_EXECUTABLE").is_none()
                && let Some(found) = find_program_on_host(host, cwd, b"wix", &roots, &["bin"])
            {
                interp
                    .cx()
                    .set_var(b"CPACK_WIX_EXECUTABLE", from_native(&found));
            }
            if !interp.cx().var_defined(b"CPACK_WIX_INSTALL_SCOPE") {
                interp
                    .cx()
                    .set_var(b"CPACK_WIX_INSTALL_SCOPE", "perMachine".into());
            }
        }
        WixVersion::V3 => {
            for (var, bin) in [
                (
                    b"CPACK_WIX_CANDLE_EXECUTABLE".as_slice(),
                    b"candle".as_slice(),
                ),
                (b"CPACK_WIX_LIGHT_EXECUTABLE", b"light"),
            ] {
                if option_set(&cfg, var).is_none()
                    && let Some(found) = find_program_on_host(host, cwd, bin, &roots, &["bin"])
                {
                    interp.cx().set_var(var, from_native(&found));
                }
            }
            if !interp.cx().var_defined(b"CPACK_WIX_INSTALL_SCOPE") {
                interp
                    .cx()
                    .set_var(b"CPACK_WIX_INSTALL_SCOPE", "NONE".into());
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_wix_config(
    host: &dyn Host,
    cfg: &CpackConfig,
    output_dir: &Path,
    cwd: &Path,
    version: WixVersion,
    install_scope: InstallScope,
    catalog: &ComponentCatalog,
    next_guid: &mut impl FnMut() -> String,
) -> Result<WixConfig, CpackError> {
    let windows_host = is_windows_host(host);
    let package_name = require_utf8(
        option_set(cfg, b"CPACK_PACKAGE_NAME")
            .ok_or_else(|| error("Required variable CPACK_PACKAGE_NAME not set"))?,
        "CPACK_PACKAGE_NAME",
    )?
    .to_owned();
    let package_version = require_utf8(
        option_set(cfg, b"CPACK_PACKAGE_VERSION")
            .ok_or_else(|| error("Required variable CPACK_PACKAGE_VERSION not set"))?,
        "CPACK_PACKAGE_VERSION",
    )?
    .to_owned();
    let vendor = require_utf8(
        option_set(cfg, b"CPACK_PACKAGE_VENDOR").unwrap_or(b"Humanity"),
        "CPACK_PACKAGE_VENDOR",
    )?
    .to_owned();
    let install_directory = match option_set(cfg, b"CPACK_PACKAGE_INSTALL_DIRECTORY") {
        Some(raw) => require_utf8(raw, "CPACK_PACKAGE_INSTALL_DIRECTORY")?.to_owned(),
        None => format!("{package_name} {package_version}"),
    };

    let product_guid = match option_set(cfg, b"CPACK_WIX_PRODUCT_GUID") {
        Some(raw) => validate_guid(require_utf8(raw, "CPACK_WIX_PRODUCT_GUID")?)?,
        None => next_guid(),
    };
    let upgrade_guid = match option_set(cfg, b"CPACK_WIX_UPGRADE_GUID") {
        Some(raw) => validate_guid(require_utf8(raw, "CPACK_WIX_UPGRADE_GUID")?)?,
        None => {
            let guid = next_guid();
            warn(
                host,
                format!(
                    "CPACK_WIX_UPGRADE_GUID implicitly set to {guid} . Please refer to the documentation on how and why you might want to set this explicitly."
                ),
            );
            guid
        }
    };

    let license_rtf = match option_set(cfg, b"CPACK_WIX_LICENSE_RTF") {
        Some(raw) => from_native_cmake(&absolute(cwd, raw)).to_string(),
        None => {
            let dest = output_dir.join("License.rtf");
            create_license_rtf(host, cfg, cwd, &dest)?;
            from_native_cmake(&dest).to_string()
        }
    };

    let architecture = if let Some(raw) = option_set(cfg, b"CPACK_WIX_ARCHITECTURE") {
        let arch = require_utf8(raw, "CPACK_WIX_ARCHITECTURE")?;
        if !arch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(error(format!("Invalid CPACK_WIX_ARCHITECTURE: '{arch}'")));
        }
        arch.to_owned()
    } else if let Some(sz) = option_set(cfg, b"CPACK_WIX_SIZEOF_VOID_P") {
        if sz == b"8" {
            "x64".to_owned()
        } else {
            "x86".to_owned()
        }
    } else {
        let machine = host.system().machine;
        if machine.windows(2).any(|w| w == b"64") {
            "x64".to_owned()
        } else {
            "x86".to_owned()
        }
    };

    let ui_ref = match option_set(cfg, b"CPACK_WIX_UI_REF") {
        Some(v) => require_utf8(v, "CPACK_WIX_UI_REF")?.to_owned(),
        None if !catalog.components.is_empty() => "WixUI_FeatureTree".to_owned(),
        None => "WixUI_InstallDir".to_owned(),
    };

    let mut wix_properties = BTreeMap::new();
    for (k, v) in &cfg.vars {
        if let Some(prop_id) = k.strip_prefix(b"CPACK_WIX_PROPERTY_")
            && !prop_id.is_empty()
        {
            let id = require_utf8(prop_id, "CPACK_WIX_PROPERTY_*")?;
            validate_xml_ncname(id)?;
            let val = require_utf8(v, id)?;
            wix_properties.insert(id.to_owned(), val.to_owned());
        }
    }
    if !wix_properties.contains_key("ARPCONTACT")
        && let Some(contact) = option_set(cfg, b"CPACK_PACKAGE_CONTACT")
    {
        wix_properties.insert(
            "ARPCONTACT".to_owned(),
            require_utf8(contact, "CPACK_PACKAGE_CONTACT")?.to_owned(),
        );
    }

    let mut wix_extensions = BTreeSet::new();
    let mut candle_extensions = BTreeSet::new();
    let mut light_extensions = BTreeSet::new();
    let skip_ui = cfg.on(b"CPACK_WIX_SKIP_WIX_UI_EXTENSION");
    match version {
        WixVersion::V4 => {
            for ext in cfg.list(b"CPACK_WIX_EXTENSIONS") {
                wix_extensions.insert(require_utf8(&ext, "CPACK_WIX_EXTENSIONS")?.to_owned());
            }
            if !skip_ui {
                wix_extensions.insert("WixToolset.UI.wixext".to_owned());
            }
        }
        WixVersion::V3 => {
            for ext in cfg.list(b"CPACK_WIX_EXTENSIONS") {
                let s = require_utf8(&ext, "CPACK_WIX_EXTENSIONS")?.to_owned();
                candle_extensions.insert(s.clone());
                light_extensions.insert(s);
            }
            for ext in cfg.list(b"CPACK_WIX_CANDLE_EXTENSIONS") {
                candle_extensions
                    .insert(require_utf8(&ext, "CPACK_WIX_CANDLE_EXTENSIONS")?.to_owned());
            }
            if !skip_ui {
                light_extensions.insert("WixUIExtension".to_owned());
            }
            for ext in cfg.list(b"CPACK_WIX_LIGHT_EXTENSIONS") {
                light_extensions
                    .insert(require_utf8(&ext, "CPACK_WIX_LIGHT_EXTENSIONS")?.to_owned());
            }
        }
    }

    let mut custom_xmlns = BTreeMap::new();
    for entry in cfg.list(b"CPACK_WIX_CUSTOM_XMLNS") {
        let s = require_utf8(&entry, "CPACK_WIX_CUSTOM_XMLNS")?;
        let Some((prefix, uri)) = s.split_once('=') else {
            return Err(error(format!(
                "Invalid element in CPACK_WIX_CUSTOM_XMLNS: \"{s}\""
            )));
        };
        validate_xml_ncname(prefix)?;
        custom_xmlns.insert(prefix.to_owned(), uri.to_owned());
    }

    let mut package_executables = Vec::new();
    let exec_list = cfg.list(b"CPACK_PACKAGE_EXECUTABLES");
    if !exec_list.len().is_multiple_of(2) {
        return Err(error(
            "CPACK_PACKAGE_EXECUTABLES should contain pairs of <executable> and <text label>.",
        ));
    }
    for [exe, label] in exec_list.as_chunks::<2>().0 {
        package_executables.push((
            require_utf8(exe, "CPACK_PACKAGE_EXECUTABLES")?.to_owned(),
            require_utf8(label, "CPACK_PACKAGE_EXECUTABLES")?.to_owned(),
        ));
    }
    let mut desktop_links = BTreeSet::new();
    for item in cfg.list(b"CPACK_CREATE_DESKTOP_LINKS") {
        desktop_links.insert(require_utf8(&item, "CPACK_CREATE_DESKTOP_LINKS")?.to_owned());
    }

    let component_keys_registry_path =
        match option_set(cfg, b"CPACK_WIX_COMPONENT_KEYS_REGISTRY_PATH") {
            Some(v) => {
                require_utf8(v, "CPACK_WIX_COMPONENT_KEYS_REGISTRY_PATH")?.replace('/', "\\")
            }
            None => format!("Software\\{vendor}\\{package_name}\\Components"),
        };

    let program_menu_folder = match option_set(cfg, b"CPACK_WIX_PROGRAM_MENU_FOLDER") {
        Some(v) => require_utf8(v, "CPACK_WIX_PROGRAM_MENU_FOLDER")?.to_owned(),
        None => package_name.clone(),
    };
    let root_feature_title = match option_set(cfg, b"CPACK_WIX_ROOT_FEATURE_TITLE") {
        Some(v) => require_utf8(v, "CPACK_WIX_ROOT_FEATURE_TITLE")?.to_owned(),
        None => package_name.clone(),
    };
    let root_feature_description = option_set(cfg, b"CPACK_WIX_ROOT_FEATURE_DESCRIPTION")
        .map(|v| require_utf8(v, "CPACK_WIX_ROOT_FEATURE_DESCRIPTION").map(str::to_owned))
        .transpose()?;
    let root_folder_override = option_set(cfg, b"CPACK_WIX_ROOT_FOLDER_ID")
        .map(|v| require_utf8(v, "CPACK_WIX_ROOT_FOLDER_ID").map(str::to_owned))
        .transpose()?;
    let cmake_package_registry = option_set(cfg, b"CPACK_WIX_CMAKE_PACKAGE_REGISTRY")
        .map(|v| require_utf8(v, "CPACK_WIX_CMAKE_PACKAGE_REGISTRY").map(str::to_owned))
        .transpose()?;

    Ok(WixConfig {
        version,
        install_scope,
        architecture,
        windows_host,
        package_name,
        package_version,
        vendor,
        install_directory,
        product_guid,
        upgrade_guid,
        license_rtf,
        product_icon: option_set(cfg, b"CPACK_WIX_PRODUCT_ICON")
            .map(|v| from_native_cmake(&absolute(cwd, v)).to_string()),
        ui_banner: option_set(cfg, b"CPACK_WIX_UI_BANNER")
            .map(|v| from_native_cmake(&absolute(cwd, v)).to_string()),
        ui_dialog: option_set(cfg, b"CPACK_WIX_UI_DIALOG")
            .map(|v| from_native_cmake(&absolute(cwd, v)).to_string()),
        ui_ref,
        root_feature_title,
        root_feature_description,
        root_folder_override,
        skip_program_folder: cfg.on(b"CPACK_WIX_SKIP_PROGRAM_FOLDER"),
        program_menu_folder,
        cmake_package_registry,
        cab_per_component: cfg.on(b"CPACK_WIX_CAB_PER_COMPONENT"),
        component_keys_registry_path,
        custom_xmlns,
        wix_properties,
        package_executables,
        desktop_links,
        wix_extensions,
        candle_extensions,
        light_extensions,
    })
}

fn build_expanded_xmlns(cfg: &WixConfig) -> Result<String, CpackError> {
    let mut out = String::new();
    if cfg.version == WixVersion::V4
        && cfg.wix_extensions.contains("WixToolset.UI.wixext")
        && !cfg.custom_xmlns.contains_key("ui")
    {
        out.push_str("\n    xmlns:ui=\"http://wixtoolset.org/schemas/v4/wxs/ui\"");
    }
    for (prefix, uri) in &cfg.custom_xmlns {
        let escaped = escape_xml_attr(uri)?;
        out.push_str(&format!("\n    xmlns:{prefix}=\"{escaped}\""));
    }
    Ok(out)
}

fn create_license_rtf(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    dest: &Path,
) -> Result<(), CpackError> {
    let src = match option_set(cfg, b"CPACK_RESOURCE_FILE_LICENSE") {
        Some(raw) => absolute(cwd, raw),
        None => {
            let fallback = cfg
                .get(b"CMAKE_ROOT")
                .filter(|r| !r.is_empty())
                .map(|r| absolute(cwd, r).join("Templates/CPack.GenericLicense.txt"));
            match fallback {
                Some(p) if host.metadata(&p, Follow::Yes).is_ok() => p,
                _ => {
                    return Err(error(
                        "Required variable CPACK_RESOURCE_FILE_LICENSE not set",
                    ));
                }
            }
        }
    };
    let meta = host.metadata_fresh(&src, Follow::Yes)?;
    if meta.kind != FileKind::File {
        return Err(error(format!(
            "Expected regular WiX license file at {}",
            src.display()
        )));
    }
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    prepare_fresh_output(host, dest)?;
    match ext.as_str() {
        ".rtf" => {
            host.copy_file(
                &src,
                dest,
                CopyOptions {
                    preserve_permissions: true,
                    preserve_mtime: true,
                },
            )?;
        }
        ".txt" => {
            let bytes = host.read(
                &src,
                ReadRange::All {
                    limit: 4 * 1024 * 1024,
                },
            )?;
            let rtf_bytes = rtf::plain_text_bytes_to_rtf(&bytes);
            host.write(dest, &rtf_bytes, WriteMode::CreateNew)?;
        }
        _ => {
            return Err(error(format!(
                "unsupported WiX License file extension '{ext}'"
            )));
        }
    }
    Ok(())
}

fn write_wix_file(host: &dyn Host, path: &Path, contents: &str) -> Result<(), CpackError> {
    prepare_fresh_output(host, path)?;
    host.write(path, contents.as_bytes(), WriteMode::CreateNew)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_wix_v3(
    host: &dyn Host,
    cfg: &CpackConfig,
    work_dir: &Path,
    canon_work: &Path,
    cwd: &Path,
    wix_cfg: &WixConfig,
    sources: &[PathBuf],
    out_msi: &Path,
    log_path: &Path,
) -> Result<(), CpackError> {
    let candle = option_set(cfg, b"CPACK_WIX_CANDLE_EXECUTABLE")
        .ok_or_else(|| error("Required variable CPACK_WIX_CANDLE_EXECUTABLE not set"))?;
    let light = option_set(cfg, b"CPACK_WIX_LIGHT_EXECUTABLE")
        .ok_or_else(|| error("Required variable CPACK_WIX_LIGHT_EXECUTABLE not set"))?;
    let candle_flags = cfg.list(b"CPACK_WIX_CANDLE_EXTRA_FLAGS");
    let light_flags = cfg.list(b"CPACK_WIX_LIGHT_EXTRA_FLAGS");

    let mut used_basenames = BTreeSet::new();
    let mut object_files = Vec::new();
    for source in sources {
        let base = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("source")
            .to_owned();
        let mut unique = base.clone();
        let mut counter = 0_usize;
        while !used_basenames.insert(unique.clone()) {
            counter += 1;
            unique = format!("{base}{counter}");
        }
        let obj = work_dir.join(format!("{unique}.wixobj"));
        prepare_fresh_output(host, &obj)?;

        let mut args = vec![
            "-nologo".into(),
            "-arch".into(),
            wix_cfg.architecture.clone().into(),
            "-out".into(),
            from_native_cmake(&obj),
        ];
        for ext in &wix_cfg.candle_extensions {
            args.extend(["-ext".into(), ext.clone().into()]);
        }
        if !source.starts_with(work_dir) {
            args.push(format!("-I{}", from_native_cmake(work_dir)).into());
        }
        args.extend(candle_flags.iter().cloned());
        args.push(from_native_cmake(source));

        let outcome = run_installer_tool(host, work_dir, candle, &args, build_tool_timeout(host))?;
        write_tool_log(host, log_path, candle, &args, &outcome.output, &[], true)?;
        check_tool_outcome(&outcome, candle, "WiX candle", Some(log_path), &[])?;
        verify_fresh_output(host, work_dir, canon_work, &obj, "WiX candle")?;
        object_files.push(obj);
    }

    let mut light_args = vec!["-nologo".into(), "-out".into(), from_native_cmake(out_msi)];
    for ext in &wix_cfg.light_extensions {
        light_args.extend(["-ext".into(), ext.clone().into()]);
    }
    if let Some(cultures) = option_set(cfg, b"CPACK_WIX_CULTURES") {
        let c = require_utf8(cultures, "CPACK_WIX_CULTURES")?;
        light_args.push(format!("-cultures:{c}").into());
    }
    light_args.extend(light_flags);
    for obj in &object_files {
        light_args.push(from_native_cmake(obj));
    }
    for extra_obj in cfg.list(b"CPACK_WIX_EXTRA_OBJECTS") {
        let path = absolute(cwd, &extra_obj);
        light_args.push(from_native_cmake(&path));
    }

    let outcome = run_installer_tool(host, work_dir, light, &light_args, build_tool_timeout(host))?;
    write_tool_log(
        host,
        log_path,
        light,
        &light_args,
        &outcome.output,
        &[],
        true,
    )?;
    check_tool_outcome(&outcome, light, "WiX light", Some(log_path), &[])?;
    Ok(())
}

fn run_wix_v4(
    host: &dyn Host,
    cfg: &CpackConfig,
    work_dir: &Path,
    wix_cfg: &WixConfig,
    sources: &[PathBuf],
    out_msi: &Path,
    log_path: &Path,
) -> Result<(), CpackError> {
    let wix = option_set(cfg, b"CPACK_WIX_EXECUTABLE")
        .ok_or_else(|| error("Required variable CPACK_WIX_EXECUTABLE not set"))?;
    let mut args = vec![
        "build".into(),
        "-arch".into(),
        wix_cfg.architecture.clone().into(),
        "-out".into(),
        from_native_cmake(out_msi),
    ];
    for ext in &wix_cfg.wix_extensions {
        args.extend(["-ext".into(), ext.clone().into()]);
    }
    for culture in cfg.list(b"CPACK_WIX_CULTURES") {
        args.extend(["-culture".into(), culture]);
    }
    args.extend(cfg.list(b"CPACK_WIX_BUILD_EXTRA_FLAGS"));
    let mut needs_include = false;
    for source in sources {
        args.extend(["-src".into(), from_native_cmake(source)]);
        if !source.starts_with(work_dir) {
            needs_include = true;
        }
    }
    if needs_include {
        args.extend(["-i".into(), from_native_cmake(work_dir)]);
    }

    let outcome = run_installer_tool(host, work_dir, wix, &args, build_tool_timeout(host))?;
    write_tool_log(host, log_path, wix, &args, &outcome.output, &[], true)?;
    check_tool_outcome(&outcome, wix, "WiX build", Some(log_path), &[])?;
    Ok(())
}

/// Validate a GUID string (`XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, optional `{...}`),
/// returning the normalized uppercase canonical representation without braces.
pub(crate) fn validate_guid(raw: &str) -> Result<String, CpackError> {
    let trimmed = raw
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(raw.trim());
    let parts: Vec<&str> = trimmed.split('-').collect();
    let expected_lens = [8, 4, 4, 4, 12];
    if parts.len() != 5
        || !parts
            .iter()
            .zip(expected_lens)
            .all(|(p, len)| p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(error(format!("Invalid WiX GUID format: '{raw}'")));
    }
    Ok(trimmed.to_ascii_uppercase())
}

/// Deterministic UUIDv3 matching `cmWIXSourceWriter::CreateCmakeGeneratedGuidFromComponentId`:
/// MD5 of the hex MD5 string of `component_id`, formatted as RFC 4122 version 3 lowercase UUID.
pub(super) fn guid_from_md5_component_id(component_id: &str) -> String {
    let first_hex = HashAlgo::Md5.hex_digest(component_id.as_bytes());
    let second_hex = HashAlgo::Md5.hex_digest(first_hex.as_bytes());
    let mut bytes = hex_to_16(&second_hex);
    bytes[6] = (bytes[6] & 0x0F) | 0x30;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format_uuid_lower(&bytes)
}

fn generate_host_guid(host: &dyn Host) -> String {
    static NEXT_GUID_SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = NEXT_GUID_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = host.now();
    let mono = host.monotonic();
    let sys = host.system();
    let seed = format!(
        "rcmake-wix-guid:{}:{}:{}:{}:{}:{seq}",
        sys.hostname,
        sys.pid,
        now.secs,
        now.nanos,
        mono.as_nanos()
    );
    let digest = HashAlgo::Sha256.hex_digest(seed.as_bytes());
    let mut bytes = hex_to_16(&digest);
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format_uuid_lower(&bytes).to_ascii_uppercase()
}

fn hex_to_16(hex: &str) -> [u8; 16] {
    let mut out = [0_u8; 16];
    let raw = hex.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_nibble(raw.get(i * 2).copied().unwrap_or(b'0'));
        let lo = hex_nibble(raw.get(i * 2 + 1).copied().unwrap_or(b'0'));
        *slot = (hi << 4) | lo;
    }
    out
}

fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn format_uuid_lower(b: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}
