//! Qt Installer Framework (`IFW`) package generator.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::version;
use rcmake_base::{BString, CMAKE_VERSION_STR, list, truth};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, ReadRange, WriteMode};

use super::ScratchDir;
use super::installer::{
    BUILD_TIMEOUT, ComponentCatalog, ComponentMeta, GroupMeta, MAX_COMPONENT_GROUP_DEPTH,
    VisitState, XmlElement, XmlNode, XmlWriter, check_tool_outcome, copy_component_payload,
    copy_monolithic_payload, escape_xml_attr, escape_xml_text, find_program_on_host, is_macos_host,
    is_set_to_empty, is_set_to_off, option_set, parse_xml_bounded, prepare_fresh_output,
    require_utf8, run_installer_tool, safe_relative_subpath, validate_extension,
    verify_fresh_output, warn, write_tool_log,
};
use crate::config::{CpackConfig, absolute, script, snapshot};
use crate::source::{SymlinkPolicy, copy_checked_tree, validate_staged_tree};
use crate::{
    ComponentGrouping, ComponentMembers, CpackError, PackageContext, PackageGenerator, error, log,
    safe_name,
};

/// The Qt Installer Framework (`IFW`) generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ifw;

/// Parsed QtIFW framework version (defaults to `1.9.9` when unspecified).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameworkVersion(String);

impl FrameworkVersion {
    pub(crate) fn parse(raw: Option<&[u8]>) -> Result<Self, CpackError> {
        let Some(raw) = raw else {
            return Ok(Self("1.9.9".to_owned()));
        };
        let text = require_utf8(raw, "CPACK_IFW_FRAMEWORK_VERSION")?.trim();
        if text.is_empty()
            || !text
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(error(format!(
                "Invalid CPACK_IFW_FRAMEWORK_VERSION value: \"{text}\""
            )));
        }
        Ok(Self(text.to_owned()))
    }

    pub(crate) fn is_less_than(&self, target: &str) -> bool {
        version::compare(self.0.as_bytes(), target.as_bytes()) == Ordering::Less
    }

    pub(crate) fn is_equal(&self, target: &str) -> bool {
        version::compare(self.0.as_bytes(), target.as_bytes()) == Ordering::Equal
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Repository update mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepoUpdateKind {
    None,
    Add,
    Remove,
    Replace,
}

/// Configured QtIFW remote repository or `Updates.xml` mutation.
#[derive(Clone, Debug)]
struct Repository {
    update: RepoUpdateKind,
    url: String,
    old_url: String,
    new_url: String,
    enabled: Option<String>,
    username: Option<String>,
    password: Option<String>,
    display_name: Option<String>,
}

/// Parsed dependency entry (`pkg[:op ver | -op ver]`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DependencySpec {
    name: String,
    op: Option<&'static str>,
    ver: String,
}

impl DependencySpec {
    fn parse(raw: &str) -> Self {
        let sep_pos = raw.find(':').or_else(|| raw.find('-'));
        let Some(pos) = sep_pos else {
            return Self {
                name: raw.to_owned(),
                op: None,
                ver: String::new(),
            };
        };
        let name = raw.get(..pos).unwrap_or("").to_owned();
        let rest = raw.get(pos + 1..).unwrap_or("");
        if rest.is_empty() {
            return Self {
                name,
                op: None,
                ver: String::new(),
            };
        }
        let (op, ver) = if let Some(v) = rest.strip_prefix("<=") {
            (Some("<="), v)
        } else if let Some(v) = rest.strip_prefix(">=") {
            (Some(">="), v)
        } else if let Some(v) = rest.strip_prefix('<') {
            (Some("<"), v)
        } else if let Some(v) = rest.strip_prefix('=') {
            (Some("="), v)
        } else if let Some(v) = rest.strip_prefix('>') {
            (Some(">"), v)
        } else {
            (Some("="), rest)
        };
        Self {
            name,
            op,
            ver: ver.to_owned(),
        }
    }

    fn format_qt(&self) -> String {
        let mut out = self.name.clone();
        if self.name.contains('-') {
            out.push(':');
        } else if self.op.is_some() || !self.ver.is_empty() {
            out.push('-');
        }
        if let Some(op) = self.op {
            out.push_str(op);
        }
        out.push_str(&self.ver);
        out
    }
}

/// Planned QtIFW package (`packages/<id>`).
#[derive(Clone, Debug)]
struct PackagePlan {
    id: String,
    display_name: BTreeMap<String, String>,
    description: BTreeMap<String, String>,
    version: String,
    release_date: Option<String>,
    script: Option<PathBuf>,
    licenses: Vec<(String, PathBuf)>,
    user_interfaces: Vec<PathBuf>,
    translations: Vec<PathBuf>,
    dependencies: BTreeSet<DependencySpec>,
    auto_depend_on: BTreeSet<DependencySpec>,
    sorting_priority: Option<String>,
    update_text: Option<String>,
    default_selected: Option<String>,
    essential: Option<String>,
    is_virtual: Option<String>,
    forced_installation: Option<String>,
    replaces: Vec<String>,
    requires_admin_rights: Option<String>,
    checkable: Option<String>,
    downloaded: bool,
    source_components: Vec<BString>,
}

/// Installer-wide configuration for `config/config.xml` and tool invocations.
struct InstallerSettings {
    binarycreator: BString,
    repogen: Option<BString>,
    framework_version: FrameworkVersion,
    resolve_duplicate_names: bool,
    online_only: bool,
    archive_format: Option<String>,
    archive_compression: Option<String>,
    extra_packages_dirs: Vec<PathBuf>,
    extra_repositories_dirs: Vec<PathBuf>,
    name: String,
    version: String,
    title: String,
    publisher: Option<String>,
    product_url: Option<String>,
    app_icon: Option<PathBuf>,
    window_icon: Option<PathBuf>,
    logo: Option<PathBuf>,
    watermark: Option<PathBuf>,
    banner: Option<PathBuf>,
    background: Option<PathBuf>,
    wizard_style: Option<String>,
    style_sheet: Option<PathBuf>,
    wizard_width: Option<String>,
    wizard_height: Option<String>,
    wizard_show_page_list: Option<String>,
    title_color: Option<String>,
    start_menu_dir: Option<String>,
    target_dir: String,
    admin_target_dir: Option<String>,
    remove_target_dir: Option<String>,
    maintenance_tool_name: Option<String>,
    maintenance_tool_ini: Option<String>,
    allow_non_ascii: Option<String>,
    allow_space_in_path: Option<String>,
    disable_cli: Option<String>,
    control_script: Option<PathBuf>,
    resources_qrc: Vec<PathBuf>,
    product_images: Vec<PathBuf>,
    product_image_urls: Vec<String>,
    run_program: Option<String>,
    run_program_args: Vec<String>,
    run_program_desc: Option<String>,
    signing_identity: Option<String>,
    remote_repositories: Vec<Repository>,
    repository_updates: Vec<Repository>,
}

impl PackageGenerator for Ifw {
    fn name(&self) -> &'static str {
        "IFW"
    }

    fn extension(&self) -> &'static str {
        ".run"
    }

    fn output_extension_for(
        &self,
        cfg: &CpackConfig,
        host: &dyn Host,
    ) -> Result<BString, CpackError> {
        if let Some(custom) = option_set(cfg, b"CPACK_IFW_PACKAGE_FILE_EXTENSION") {
            return validate_extension(custom);
        }
        let sys_name = option_set(cfg, b"CMAKE_SYSTEM_NAME")
            .map(BString::from)
            .unwrap_or_else(|| host.system().sysname);
        let lower = sys_name.to_ascii_lowercase();
        let ext: BString = if lower == b"darwin" || lower.starts_with(b"macos") {
            ".dmg".into()
        } else if lower == b"linux" {
            ".run".into()
        } else if lower.starts_with(b"windows")
            || lower.starts_with(b"mingw")
            || lower.starts_with(b"msys")
            || lower.starts_with(b"cygwin")
        {
            ".exe".into()
        } else if let Some(exe_suffix) = option_set(cfg, b"CMAKE_EXECUTABLE_SUFFIX") {
            validate_extension(exe_suffix)?
        } else {
            ".run".into()
        };
        Ok(ext)
    }

    fn prepare_config(
        &self,
        host: &dyn Host,
        interp: &mut Interp,
        cwd: &Path,
    ) -> Result<(), CpackError> {
        let cfg = snapshot(interp);
        let needs_module = option_set(&cfg, b"CPACK_IFW_BINARYCREATOR_EXECUTABLE").is_none()
            || option_set(&cfg, b"CPACK_IFW_REPOGEN_EXECUTABLE").is_none()
            || option_set(&cfg, b"CPACK_IFW_FRAMEWORK_VERSION").is_none();
        if needs_module {
            let cmake_root = interp.cx().var_or_empty(b"CMAKE_ROOT");
            if !cmake_root.is_empty() {
                let module = absolute(cwd, &cmake_root).join("Modules/CPackIFW.cmake");
                if host
                    .metadata(&module, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::File)
                {
                    let _ = script(interp, &module);
                }
            }
        }

        let cfg = snapshot(interp);
        let mut roots = Vec::new();
        if let Some(v) = option_set(&cfg, b"CPACK_IFW_ROOT") {
            roots.push(absolute(cwd, v));
        }
        if let Some(qtifw) = host.env_get(b"QTIFWDIR").filter(|v| !v.is_empty()) {
            roots.push(absolute(cwd, &qtifw));
        }
        for (var, bin) in [
            (
                b"CPACK_IFW_BINARYCREATOR_EXECUTABLE".as_slice(),
                b"binarycreator".as_slice(),
            ),
            (b"CPACK_IFW_REPOGEN_EXECUTABLE", b"repogen"),
        ] {
            if option_set(&cfg, var).is_none()
                && let Some(found) = find_program_on_host(host, cwd, bin, &roots, &["bin"])
            {
                interp.cx().set_var(var, from_native(&found));
            }
        }

        let cfg = snapshot(interp);
        if option_set(&cfg, b"CPACK_IFW_BINARYCREATOR_EXECUTABLE").is_none() {
            return Err(error(
                "Cannot find QtIFW compiler \"binarycreator\": likely it is not installed, or not in your PATH",
            ));
        }
        let _ = FrameworkVersion::parse(option_set(&cfg, b"CPACK_IFW_FRAMEWORK_VERSION"))?;

        let (remote, _) = collect_repositories(host, &cfg)?;
        if !remote.is_empty() && option_set(&cfg, b"CPACK_IFW_REPOGEN_EXECUTABLE").is_none() {
            return Err(error(
                "Cannot find QtIFW repository generator \"repogen\": likely it is not installed, or not in your PATH",
            ));
        }
        Ok(())
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        self.package_internal(cx, &[])
    }

    fn package_from_stage(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
    ) -> Result<Vec<PathBuf>, CpackError> {
        self.package_internal(cx, members)
    }

    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
        _interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        self.package_internal(cx, members)
    }
}

impl Ifw {
    fn package_internal(
        &self,
        cx: &PackageContext<'_>,
        members: &[crate::InstalledComponent],
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
                "IFW generator does not support absolute install destinations: {}",
                BString::from(abs_files)
            )));
        }

        validate_staged_tree(
            host,
            cx.staging_dir,
            &canon_staging,
            members,
            SymlinkPolicy::PreserveContained,
            false,
        )?;

        let settings = InstallerSettings::from_config(host, &cfg, &cwd)?;
        let catalog = ComponentCatalog::from_config(&cfg, members)?;
        let (packages, external_deps) =
            build_package_plans(host, &cfg, &cwd, &settings, &catalog, cx.grouping)?;

        let scratch = ScratchDir::for_generator(host, cx.output_dir, "ifw")?;
        let snapshot_dir = scratch.path.join("snapshot");
        host.create_dir_all(&snapshot_dir)?;

        let is_monolithic = catalog.components.is_empty();
        if is_monolithic {
            copy_monolithic_payload(
                host,
                cx.staging_dir,
                &canon_staging,
                &snapshot_dir.join("monolithic"),
                SymlinkPolicy::PreserveContained,
                false,
            )?;
        } else {
            for comp_name in catalog.components.keys() {
                let comp_snap =
                    snapshot_dir.join(crate::stage::component_dir_for_host(comp_name, false));
                if let Some(member) = members.iter().find(|m| &m.name == comp_name) {
                    copy_component_payload(
                        host,
                        cx.staging_dir,
                        &canon_staging,
                        member,
                        &comp_snap,
                        SymlinkPolicy::PreserveContained,
                        false,
                        false,
                    )?;
                } else {
                    let fallback = crate::InstalledComponent {
                        name: comp_name.clone(),
                        ..Default::default()
                    };
                    copy_component_payload(
                        host,
                        cx.staging_dir,
                        &canon_staging,
                        &fallback,
                        &comp_snap,
                        SymlinkPolicy::PreserveContained,
                        false,
                        false,
                    )?;
                }
            }
        }

        let config_dir = cx.staging_dir.join("config");
        let packages_dir = cx.staging_dir.join("packages");
        let resources_dir = cx.staging_dir.join("resources");
        for dir in [&config_dir, &packages_dir, &resources_dir] {
            crate::stage::remove_entry(host, dir)?;
        }
        host.create_dir_all(&config_dir)?;
        host.create_dir_all(&packages_dir)?;

        for pkg in packages.values() {
            let pkg_root = packages_dir.join(safe_name(pkg.id.as_bytes())?);
            let data_dir = pkg_root.join("data");
            let meta_dir = pkg_root.join("meta");
            host.create_dir_all(&data_dir)?;
            host.create_dir_all(&meta_dir)?;
            if is_monolithic {
                copy_checked_tree(
                    host,
                    &snapshot_dir.join("monolithic"),
                    &data_dir,
                    SymlinkPolicy::PreserveContained,
                    false,
                )?;
            } else {
                for comp_name in &pkg.source_components {
                    let comp_snap =
                        snapshot_dir.join(crate::stage::component_dir_for_host(comp_name, false));
                    if host.metadata_fresh(&comp_snap, Follow::No).is_ok() {
                        copy_checked_tree(
                            host,
                            &comp_snap,
                            &data_dir,
                            SymlinkPolicy::PreserveContained,
                            false,
                        )?;
                    }
                }
            }
        }

        log(host, "- Configuration");
        let staged_qrc = write_config_xml(host, &settings, &config_dir, &resources_dir)?;

        for pkg in packages.values() {
            let pkg_root = packages_dir.join(safe_name(pkg.id.as_bytes())?);
            write_package_xml(host, &settings, pkg, &pkg_root.join("meta"))?;
        }

        let secrets = collect_repo_secrets(&settings);
        let log_path = cx.output_dir.join("IFWOutput.log");
        let repo_dir = cx.staging_dir.join("repository");

        let downloaded_ids: Vec<String> = packages
            .values()
            .filter(|p| p.downloaded)
            .map(|p| p.id.clone())
            .collect();
        let binary_ids: Vec<String> = packages
            .values()
            .filter(|p| !p.downloaded)
            .map(|p| p.id.clone())
            .collect();

        if !settings.remote_repositories.is_empty() {
            let repogen = settings.repogen.as_deref().ok_or_else(|| {
                error(
                    "Cannot find QtIFW repository generator \"repogen\": likely it is not installed, or not in your PATH",
                )
            })?;
            crate::stage::remove_entry(host, &repo_dir)?;
            let repogen_args = build_repogen_args(
                host,
                &settings,
                &config_dir.join("config.xml"),
                &packages_dir,
                &downloaded_ids,
                &repo_dir,
            );
            log(host, "- Generate repository");
            let outcome =
                run_installer_tool(host, cx.output_dir, repogen, &repogen_args, BUILD_TIMEOUT)?;
            if !matches!(outcome.status, rcmake_host::ProcessResult::Exited(0)) {
                write_tool_log(
                    host,
                    &log_path,
                    repogen,
                    &repogen_args,
                    &outcome.output,
                    &secrets,
                    false,
                )?;
            }
            check_tool_outcome(&outcome, repogen, "repogen", Some(&log_path), &secrets)?;

            if !settings.repository_updates.is_empty()
                && let Err(err) = patch_updates_xml(host, &settings, &repo_dir)
            {
                warn(
                    host,
                    format!(
                        "Problem patch IFW \"Updates\" file: \"{}\" ({err})",
                        repo_dir.join("Updates.xml").display()
                    ),
                );
            }
            log(
                host,
                format!("- repository: \"{}\" generated", repo_dir.display()),
            );
        }

        let ext = self.output_extension_for(&cfg, host)?;
        let out_installer = cx
            .output_dir
            .join(safe_name(&[cx.file_name, ext.as_slice()].concat())?);
        prepare_fresh_output(host, &out_installer)?;

        let bc_args = build_binarycreator_args(
            host,
            &settings,
            &config_dir.join("config.xml"),
            &staged_qrc,
            &packages_dir,
            &binary_ids,
            &downloaded_ids,
            &external_deps,
            &out_installer,
        );
        log(host, "- Generate package");
        let outcome = run_installer_tool(
            host,
            cx.output_dir,
            &settings.binarycreator,
            &bc_args,
            BUILD_TIMEOUT,
        )?;
        if !matches!(outcome.status, rcmake_host::ProcessResult::Exited(0)) {
            write_tool_log(
                host,
                &log_path,
                &settings.binarycreator,
                &bc_args,
                &outcome.output,
                &secrets,
                false,
            )?;
        }
        check_tool_outcome(
            &outcome,
            &settings.binarycreator,
            "binarycreator",
            Some(&log_path),
            &secrets,
        )?;
        verify_fresh_output(
            host,
            cx.output_dir,
            &canon_work,
            &out_installer,
            "binarycreator",
        )?;
        Ok(vec![out_installer])
    }
}

impl InstallerSettings {
    fn from_config(host: &dyn Host, cfg: &CpackConfig, cwd: &Path) -> Result<Self, CpackError> {
        let binarycreator: BString = option_set(cfg, b"CPACK_IFW_BINARYCREATOR_EXECUTABLE")
            .ok_or_else(|| {
                error(
                    "Cannot find QtIFW compiler \"binarycreator\": likely it is not installed, or not in your PATH",
                )
            })?
            .into();
        let repogen = option_set(cfg, b"CPACK_IFW_REPOGEN_EXECUTABLE").map(BString::from);
        let framework_version =
            FrameworkVersion::parse(option_set(cfg, b"CPACK_IFW_FRAMEWORK_VERSION"))?;
        let resolve_duplicate_names = cfg.on(b"CPACK_IFW_RESOLVE_DUPLICATE_NAMES");
        let online_only = if cfg.get(b"CPACK_IFW_DOWNLOAD_ALL").is_some() {
            cfg.on(b"CPACK_IFW_DOWNLOAD_ALL")
        } else {
            cfg.on(b"CPACK_DOWNLOAD_ALL")
        };

        let archive_format = option_set(cfg, b"CPACK_IFW_ARCHIVE_FORMAT")
            .map(|v| require_utf8(v, "CPACK_IFW_ARCHIVE_FORMAT").map(str::to_owned))
            .transpose()?;
        let archive_compression = option_set(cfg, b"CPACK_IFW_ARCHIVE_COMPRESSION")
            .map(|v| require_utf8(v, "CPACK_IFW_ARCHIVE_COMPRESSION").map(str::to_owned))
            .transpose()?;
        let extra_packages_dirs = cfg
            .list(b"CPACK_IFW_PACKAGES_DIRECTORIES")
            .into_iter()
            .map(|d| absolute(cwd, &d))
            .collect();
        let extra_repositories_dirs = cfg
            .list(b"CPACK_IFW_REPOSITORIES_DIRECTORIES")
            .into_iter()
            .map(|d| absolute(cwd, &d))
            .collect();

        let name = require_utf8(
            option_set(cfg, b"CPACK_IFW_PACKAGE_NAME")
                .or_else(|| option_set(cfg, b"CPACK_PACKAGE_NAME"))
                .unwrap_or(b"Your package"),
            "CPACK_IFW_PACKAGE_NAME",
        )?
        .to_owned();
        let title = require_utf8(
            option_set(cfg, b"CPACK_IFW_PACKAGE_TITLE")
                .or_else(|| option_set(cfg, b"CPACK_PACKAGE_DESCRIPTION_SUMMARY"))
                .unwrap_or(b"Your package description"),
            "CPACK_IFW_PACKAGE_TITLE",
        )?
        .to_owned();
        let version = require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_VERSION").unwrap_or(b"1.0.0"),
            "CPACK_PACKAGE_VERSION",
        )?
        .to_owned();
        let publisher = option_set(cfg, b"CPACK_IFW_PACKAGE_PUBLISHER")
            .or_else(|| option_set(cfg, b"CPACK_PACKAGE_VENDOR"))
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_PUBLISHER").map(str::to_owned))
            .transpose()?;
        let product_url = option_set(cfg, b"CPACK_IFW_PRODUCT_URL")
            .map(|v| require_utf8(v, "CPACK_IFW_PRODUCT_URL").map(str::to_owned))
            .transpose()?;

        let opt_existing_file = |var: &str| -> Option<PathBuf> {
            let raw = option_set(cfg, var.as_bytes())?;
            let path = absolute(cwd, raw);
            if host
                .metadata_fresh(&path, Follow::Yes)
                .is_ok_and(|m| m.kind == FileKind::File)
            {
                Some(path)
            } else {
                warn(
                    host,
                    format!(
                        "Option {var} contains the value \"{}\" but will be skipped because the specified file does not exist.",
                        BString::from(raw)
                    ),
                );
                None
            }
        };

        let app_icon = opt_existing_file("CPACK_IFW_PACKAGE_ICON");
        let window_icon = opt_existing_file("CPACK_IFW_PACKAGE_WINDOW_ICON");
        let logo = opt_existing_file("CPACK_IFW_PACKAGE_LOGO");
        let watermark = opt_existing_file("CPACK_IFW_PACKAGE_WATERMARK");
        let banner = opt_existing_file("CPACK_IFW_PACKAGE_BANNER");
        let background = opt_existing_file("CPACK_IFW_PACKAGE_BACKGROUND");
        let style_sheet = opt_existing_file("CPACK_IFW_PACKAGE_STYLE_SHEET");
        let control_script = opt_existing_file("CPACK_IFW_PACKAGE_CONTROL_SCRIPT");

        let wizard_style = option_set(cfg, b"CPACK_IFW_PACKAGE_WIZARD_STYLE")
            .map(|v| -> Result<String, CpackError> {
                let s = require_utf8(v, "CPACK_IFW_PACKAGE_WIZARD_STYLE")?.to_owned();
                if !matches!(s.as_str(), "Modern" | "Aero" | "Mac" | "Classic") {
                    warn(
                        host,
                        format!(
                            "Option CPACK_IFW_PACKAGE_WIZARD_STYLE has unknown value \"{s}\". Expected values are: Modern, Aero, Mac, Classic."
                        ),
                    );
                }
                Ok(s)
            })
            .transpose()?;

        let wizard_width = option_set(cfg, b"CPACK_IFW_PACKAGE_WIZARD_DEFAULT_WIDTH")
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_WIZARD_DEFAULT_WIDTH").map(str::to_owned))
            .transpose()?;
        let wizard_height = option_set(cfg, b"CPACK_IFW_PACKAGE_WIZARD_DEFAULT_HEIGHT")
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_WIZARD_DEFAULT_HEIGHT").map(str::to_owned))
            .transpose()?;
        let wizard_show_page_list = if option_set(cfg, b"CPACK_IFW_PACKAGE_WIZARD_SHOW_PAGE_LIST")
            .is_some()
        {
            if !framework_version.is_less_than("4.0") {
                if is_set_to_off(cfg, b"CPACK_IFW_PACKAGE_WIZARD_SHOW_PAGE_LIST") {
                    Some("false".to_owned())
                } else if cfg.on(b"CPACK_IFW_PACKAGE_WIZARD_SHOW_PAGE_LIST") {
                    Some("true".to_owned())
                } else {
                    None
                }
            } else {
                warn(
                    host,
                    "Option CPACK_IFW_PACKAGE_WIZARD_SHOW_PAGE_LIST is only supported with QtIFW version 4.0 or later.",
                );
                None
            }
        } else {
            None
        };
        let title_color = option_set(cfg, b"CPACK_IFW_PACKAGE_TITLE_COLOR")
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_TITLE_COLOR").map(str::to_owned))
            .transpose()?;
        let start_menu_dir = Some(
            option_set(cfg, b"CPACK_IFW_PACKAGE_START_MENU_DIRECTORY")
                .map(|v| {
                    require_utf8(v, "CPACK_IFW_PACKAGE_START_MENU_DIRECTORY").map(str::to_owned)
                })
                .transpose()?
                .unwrap_or_else(|| name.clone()),
        );
        // Preserve Qt placeholders such as @ApplicationsDir@ and @RootDir@ verbatim.
        let target_dir = if let Some(td) = option_set(cfg, b"CPACK_IFW_TARGET_DIRECTORY") {
            require_utf8(td, "CPACK_IFW_TARGET_DIRECTORY")?.to_owned()
        } else if let Some(install_dir) = option_set(cfg, b"CPACK_PACKAGE_INSTALL_DIRECTORY") {
            format!(
                "@ApplicationsDir@/{}",
                require_utf8(install_dir, "CPACK_PACKAGE_INSTALL_DIRECTORY")?
            )
        } else {
            "@RootDir@/usr/local".to_owned()
        };
        let admin_target_dir = option_set(cfg, b"CPACK_IFW_ADMIN_TARGET_DIRECTORY")
            .map(|v| require_utf8(v, "CPACK_IFW_ADMIN_TARGET_DIRECTORY").map(str::to_owned))
            .transpose()?;
        let remove_target_dir = if is_set_to_off(cfg, b"CPACK_IFW_PACKAGE_REMOVE_TARGET_DIR") {
            Some("false".to_owned())
        } else if cfg.on(b"CPACK_IFW_PACKAGE_REMOVE_TARGET_DIR") {
            Some("true".to_owned())
        } else {
            None
        };
        let maintenance_tool_name = option_set(cfg, b"CPACK_IFW_PACKAGE_MAINTENANCE_TOOL_NAME")
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_MAINTENANCE_TOOL_NAME").map(str::to_owned))
            .transpose()?;
        let maintenance_tool_ini = option_set(cfg, b"CPACK_IFW_PACKAGE_MAINTENANCE_TOOL_INI_FILE")
            .map(|v| {
                require_utf8(v, "CPACK_IFW_PACKAGE_MAINTENANCE_TOOL_INI_FILE").map(str::to_owned)
            })
            .transpose()?;
        let allow_non_ascii = cfg
            .get(b"CPACK_IFW_PACKAGE_ALLOW_NON_ASCII_CHARACTERS")
            .filter(|v| !v.is_empty())
            .map(|v| if truth::is_on(v) { "true" } else { "false" }.to_owned());
        let allow_space_in_path = cfg
            .get(b"CPACK_IFW_PACKAGE_ALLOW_SPACE_IN_PATH")
            .filter(|v| !v.is_empty())
            .map(|v| if truth::is_on(v) { "true" } else { "false" }.to_owned());
        let disable_cli = if cfg.on(b"CPACK_IFW_PACKAGE_DISABLE_COMMAND_LINE_INTERFACE") {
            Some("true".to_owned())
        } else if is_set_to_off(cfg, b"CPACK_IFW_PACKAGE_DISABLE_COMMAND_LINE_INTERFACE") {
            Some("false".to_owned())
        } else {
            None
        };

        let resources_qrc = cfg
            .list(b"CPACK_IFW_PACKAGE_RESOURCES")
            .into_iter()
            .map(|f| absolute(cwd, &f))
            .collect();

        let mut product_images = Vec::new();
        for img in cfg.list(b"CPACK_IFW_PACKAGE_PRODUCT_IMAGES") {
            let path = absolute(cwd, &img);
            if host
                .metadata_fresh(&path, Follow::Yes)
                .is_ok_and(|m| m.kind == FileKind::File)
            {
                product_images.push(path);
            } else {
                warn(
                    host,
                    format!(
                        "Option CPACK_IFW_PACKAGE_PRODUCT_IMAGES contains the value \"{img}\" but will be skipped because the specified file does not exist."
                    ),
                );
            }
        }
        let mut product_image_urls = Vec::new();
        if !product_images.is_empty() {
            let urls: Vec<String> = cfg
                .list(b"CPACK_IFW_PACKAGE_PRODUCT_IMAGE_URLS")
                .into_iter()
                .map(|u| {
                    require_utf8(&u, "CPACK_IFW_PACKAGE_PRODUCT_IMAGE_URLS").map(str::to_owned)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !urls.is_empty() {
                if urls.len() == product_images.len() {
                    product_image_urls = urls;
                } else {
                    warn(
                        host,
                        format!(
                            "Option \"CPACK_IFW_PACKAGE_PRODUCT_IMAGE_URLS\" will be skipped because it contains {} elements while \"CPACK_IFW_PACKAGE_PRODUCT_IMAGES\" contains {} elements.",
                            urls.len(),
                            product_images.len()
                        ),
                    );
                }
            }
        }

        let run_program = option_set(cfg, b"CPACK_IFW_PACKAGE_RUN_PROGRAM")
            .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_RUN_PROGRAM").map(str::to_owned))
            .transpose()?;
        let run_program_args = cfg
            .list(b"CPACK_IFW_PACKAGE_RUN_PROGRAM_ARGUMENTS")
            .into_iter()
            .map(|a| require_utf8(&a, "CPACK_IFW_PACKAGE_RUN_PROGRAM_ARGUMENTS").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        let run_program_desc = option_set(cfg, b"CPACK_IFW_PACKAGE_RUN_PROGRAM_DESCRIPTION")
            .map(|v| {
                require_utf8(v, "CPACK_IFW_PACKAGE_RUN_PROGRAM_DESCRIPTION").map(str::to_owned)
            })
            .transpose()?;

        let signing_identity = if is_macos_host(host) {
            option_set(cfg, b"CPACK_IFW_PACKAGE_SIGNING_IDENTITY")
                .map(|v| require_utf8(v, "CPACK_IFW_PACKAGE_SIGNING_IDENTITY").map(str::to_owned))
                .transpose()?
        } else {
            None
        };

        let (remote_repositories, repository_updates) = collect_repositories(host, cfg)?;
        if !remote_repositories.is_empty() && repogen.is_none() {
            return Err(error(
                "Cannot find QtIFW repository generator \"repogen\": likely it is not installed, or not in your PATH",
            ));
        }

        Ok(Self {
            binarycreator,
            repogen,
            framework_version,
            resolve_duplicate_names,
            online_only,
            archive_format,
            archive_compression,
            extra_packages_dirs,
            extra_repositories_dirs,
            name,
            version,
            title,
            publisher,
            product_url,
            app_icon,
            window_icon,
            logo,
            watermark,
            banner,
            background,
            wizard_style,
            style_sheet,
            wizard_width,
            wizard_height,
            wizard_show_page_list,
            title_color,
            start_menu_dir,
            target_dir,
            admin_target_dir,
            remove_target_dir,
            maintenance_tool_name,
            maintenance_tool_ini,
            allow_non_ascii,
            allow_space_in_path,
            disable_cli,
            control_script,
            resources_qrc,
            product_images,
            product_image_urls,
            run_program,
            run_program_args,
            run_program_desc,
            signing_identity,
            remote_repositories,
            repository_updates,
        })
    }
}

fn collect_repositories(
    host: &dyn Host,
    cfg: &CpackConfig,
) -> Result<(Vec<Repository>, Vec<Repository>), CpackError> {
    let mut remote = Vec::new();
    let mut updates = Vec::new();
    if let Some(site) = option_set(cfg, b"CPACK_DOWNLOAD_SITE") {
        remote.push(Repository {
            update: RepoUpdateKind::None,
            url: require_utf8(site, "CPACK_DOWNLOAD_SITE")?.to_owned(),
            old_url: String::new(),
            new_url: String::new(),
            enabled: None,
            username: None,
            password: None,
            display_name: None,
        });
    }
    let mut seen = BTreeSet::new();
    for raw_name in cfg.list(b"CPACK_IFW_REPOSITORIES_ALL") {
        if !seen.insert(raw_name.clone()) {
            continue;
        }
        let name = require_utf8(&raw_name, "CPACK_IFW_REPOSITORIES_ALL")?.to_owned();
        let upper = name.to_ascii_uppercase();
        let prefix = format!("CPACK_IFW_REPOSITORY_{upper}_");
        let get = |k: &str| option_set(cfg, format!("{prefix}{k}").as_bytes());
        let on = |k: &str| cfg.on(format!("{prefix}{k}").as_bytes());

        let update = if on("ADD") {
            RepoUpdateKind::Add
        } else if on("REMOVE") {
            RepoUpdateKind::Remove
        } else if on("REPLACE") {
            RepoUpdateKind::Replace
        } else {
            RepoUpdateKind::None
        };
        let url = get("URL")
            .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_URL").map(str::to_owned))
            .transpose()?
            .unwrap_or_default();
        let old_url = get("OLD_URL")
            .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_OLD_URL").map(str::to_owned))
            .transpose()?
            .unwrap_or_default();
        let new_url = get("NEW_URL")
            .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_NEW_URL").map(str::to_owned))
            .transpose()?
            .unwrap_or_default();

        let valid = match update {
            RepoUpdateKind::None | RepoUpdateKind::Add | RepoUpdateKind::Remove => !url.is_empty(),
            RepoUpdateKind::Replace => !old_url.is_empty() && !new_url.is_empty(),
        };
        if !valid {
            warn(
                host,
                format!("Invalid repository \"{name}\" configuration. Repository will be skipped."),
            );
            continue;
        }

        let repo = Repository {
            update,
            url,
            old_url,
            new_url,
            enabled: on("DISABLED").then(|| "0".to_owned()),
            username: get("USERNAME")
                .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_USERNAME").map(str::to_owned))
                .transpose()?,
            password: get("PASSWORD")
                .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_PASSWORD").map(str::to_owned))
                .transpose()?,
            display_name: get("DISPLAY_NAME")
                .map(|v| require_utf8(v, "CPACK_IFW_REPOSITORY_DISPLAY_NAME").map(str::to_owned))
                .transpose()?,
        };
        if repo.update == RepoUpdateKind::None {
            remote.push(repo);
        } else {
            updates.push(repo);
        }
    }
    Ok((remote, updates))
}

fn push_secret_variants(secrets: &mut BTreeSet<BString>, raw: &str) {
    if raw.is_empty() {
        return;
    }
    secrets.insert(BString::from(raw));
    if let Ok(xml_text) = escape_xml_text(raw) {
        secrets.insert(BString::from(xml_text.as_str()));
        secrets.insert(BString::from(xml_text.replace('\'', "&apos;")));
    }
    if let Ok(xml_attr) = escape_xml_attr(raw) {
        secrets.insert(BString::from(xml_attr.as_str()));
        secrets.insert(BString::from(xml_attr.replace('\'', "&apos;")));
    }
    if let Some(decoded) = percent_decode_utf8(raw)
        && decoded != raw
    {
        secrets.insert(BString::from(decoded.as_str()));
        if let Ok(xml_text) = escape_xml_text(&decoded) {
            secrets.insert(BString::from(xml_text));
        }
        if let Ok(xml_attr) = escape_xml_attr(&decoded) {
            secrets.insert(BString::from(xml_attr.as_str()));
            secrets.insert(BString::from(xml_attr.replace('\'', "&apos;")));
        }
    }
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    if !input.contains('%') {
        return None;
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'%')
            && let (Some(&h1), Some(&h2)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(n1), Some(n2)) = (hex_value(h1), hex_value(h2))
        {
            out.push((n1 << 4) | n2);
            i += 3;
            continue;
        }
        if let Some(&b) = bytes.get(i) {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn extract_url_credentials(url: &str, secrets: &mut BTreeSet<BString>) {
    let Some(scheme_end) = url.find("://") else {
        return;
    };
    let rest = url.get(scheme_end + 3..).unwrap_or("");
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..auth_end).unwrap_or("");
    let Some(at_pos) = authority.rfind('@') else {
        return;
    };
    let userinfo = authority.get(..at_pos).unwrap_or("");
    if userinfo.is_empty() {
        return;
    }
    push_secret_variants(secrets, userinfo);
    if let Some((_, password)) = userinfo.split_once(':') {
        push_secret_variants(secrets, password);
    }
}

fn collect_repo_secrets(settings: &InstallerSettings) -> Vec<BString> {
    let mut secrets = BTreeSet::new();
    for repo in settings
        .remote_repositories
        .iter()
        .chain(&settings.repository_updates)
    {
        if let Some(ref pwd) = repo.password {
            push_secret_variants(&mut secrets, pwd);
        }
        extract_url_credentials(&repo.url, &mut secrets);
        extract_url_credentials(&repo.old_url, &mut secrets);
        extract_url_credentials(&repo.new_url, &mut secrets);
    }
    secrets.into_iter().collect()
}

type PackagePlanOutput = (
    BTreeMap<String, PackagePlan>,
    BTreeMap<String, DependencySpec>,
);

fn build_package_plans(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    settings: &InstallerSettings,
    catalog: &ComponentCatalog,
    grouping: ComponentGrouping,
) -> Result<PackagePlanOutput, CpackError> {
    let mut plans = BTreeMap::<String, PackagePlan>::new();
    let mut external_deps = BTreeMap::<String, DependencySpec>::new();

    if catalog.components.is_empty() || grouping == ComponentGrouping::AllComponentsInOne {
        let mut root_plan = build_root_package_plan(host, cfg, cwd, settings)?;
        let _ = safe_name(root_plan.id.as_bytes())?;
        if !catalog.components.is_empty() {
            root_plan
                .source_components
                .extend(catalog.components.keys().cloned());
        }
        plans.insert(root_plan.id.clone(), root_plan);
        return Ok((plans, external_deps));
    }

    let mut group_pkg_ids = BTreeMap::<BString, String>::new();
    for group_name in catalog.groups.keys() {
        resolve_group_pkg_id(
            cfg,
            catalog,
            group_name,
            settings.resolve_duplicate_names,
            &mut group_pkg_ids,
        )?;
    }

    for (group_name, group) in &catalog.groups {
        let pkg_id = group_pkg_ids
            .get(group_name)
            .cloned()
            .unwrap_or_else(|| group_name.to_string());
        let _ = safe_name(pkg_id.as_bytes())?;
        if let std::collections::btree_map::Entry::Vacant(slot) = plans.entry(pkg_id.clone()) {
            let plan = build_group_package_plan(
                host,
                cfg,
                cwd,
                settings,
                &pkg_id,
                group,
                &mut external_deps,
            )?;
            slot.insert(plan);
        }
    }

    let mut comp_pkg_ids = BTreeMap::<BString, String>::new();
    for (comp_name, comp) in &catalog.components {
        let upper = BString::from(comp_name.to_ascii_uppercase());
        let prefix = format!("CPACK_IFW_COMPONENT_{upper}_");
        let base_name = option_set(cfg, format!("{prefix}NAME").as_bytes())
            .map(|v| require_utf8(v, "CPACK_IFW_COMPONENT_NAME").map(str::to_owned))
            .transpose()?
            .unwrap_or_else(|| comp_name.to_string());
        let is_common = cfg.on(format!("{prefix}COMMON").as_bytes());
        let pkg_id = if let Some(ref gname) = comp.group {
            let g_pkg = group_pkg_ids
                .get(gname)
                .cloned()
                .unwrap_or_else(|| gname.to_string());
            if grouping == ComponentGrouping::OnePerGroup || is_common {
                g_pkg
            } else if !settings.resolve_duplicate_names && !base_name.starts_with(&g_pkg) {
                format!("{g_pkg}.{base_name}")
            } else {
                base_name
            }
        } else {
            base_name
        };
        let _ = safe_name(pkg_id.as_bytes())?;
        comp_pkg_ids.insert(comp_name.clone(), pkg_id);
    }

    for (comp_name, comp) in &catalog.components {
        let pkg_id = comp_pkg_ids
            .get(comp_name)
            .cloned()
            .unwrap_or_else(|| comp_name.to_string());
        if let Some(existing) = plans.get_mut(&pkg_id) {
            existing.source_components.push(comp_name.clone());
            if comp.downloaded {
                existing.downloaded = true;
            }
            for dep in &comp.depends {
                if let Some(dep_pkg) = comp_pkg_ids.get(dep)
                    && dep_pkg != &pkg_id
                {
                    existing.dependencies.insert(DependencySpec::parse(dep_pkg));
                }
            }
            continue;
        }
        let mut plan = build_component_package_plan(
            host,
            cfg,
            cwd,
            settings,
            &pkg_id,
            comp,
            &comp_pkg_ids,
            &mut external_deps,
        )?;
        plan.source_components.push(comp_name.clone());
        plans.insert(pkg_id, plan);
    }

    // Remove internal package names from external_deps so only truly external dependencies
    // trigger the `-i` binarycreator inclusion mode, and validate acyclic internal package
    // dependencies.
    external_deps.retain(|name, _| !plans.contains_key(name));
    check_ifw_package_cycles(&plans)?;

    Ok((plans, external_deps))
}

fn resolve_group_pkg_id(
    cfg: &CpackConfig,
    catalog: &ComponentCatalog,
    group_name: &BString,
    resolve_duplicates: bool,
    resolved: &mut BTreeMap<BString, String>,
) -> Result<String, CpackError> {
    if let Some(existing) = resolved.get(group_name) {
        return Ok(existing.clone());
    }
    let mut chain: Vec<&BString> = Vec::new();
    let mut seen: BTreeSet<&[u8]> = BTreeSet::new();
    let mut cursor = Some(group_name);
    while let Some(name) = cursor {
        if resolved.contains_key(name) {
            break;
        }
        if !seen.insert(name.as_slice()) {
            return Err(error(format!(
                "Cycle detected in CPack component group hierarchy involving \"{name}\""
            )));
        }
        if chain.len() >= MAX_COMPONENT_GROUP_DEPTH {
            return Err(error(format!(
                "CPack component group hierarchy exceeds maximum depth ({MAX_COMPONENT_GROUP_DEPTH}) involving \"{group_name}\""
            )));
        }
        chain.push(name);
        cursor = catalog
            .groups
            .get(name)
            .and_then(|g| g.parent_group.as_ref());
    }
    for name in chain.into_iter().rev() {
        let upper = BString::from(name.to_ascii_uppercase());
        let key = format!("CPACK_IFW_COMPONENT_GROUP_{upper}_NAME");
        let base = option_set(cfg, key.as_bytes())
            .map(|v| require_utf8(v, &key).map(str::to_owned))
            .transpose()?
            .unwrap_or_else(|| name.to_string());
        let full = if let Some(parent_name) = catalog
            .groups
            .get(name)
            .and_then(|g| g.parent_group.as_ref())
            && let Some(parent_id) = resolved.get(parent_name)
        {
            if !resolve_duplicates && !base.starts_with(parent_id) {
                format!("{parent_id}.{base}")
            } else {
                base
            }
        } else {
            base
        };
        resolved.insert(name.clone(), full);
    }
    Ok(resolved
        .get(group_name)
        .cloned()
        .unwrap_or_else(|| group_name.to_string()))
}

fn build_root_package_plan(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    settings: &InstallerSettings,
) -> Result<PackagePlan, CpackError> {
    if let Some(group_raw) = option_set(cfg, b"CPACK_IFW_PACKAGE_GROUP") {
        let gname = require_utf8(group_raw, "CPACK_IFW_PACKAGE_GROUP")?;
        let upper = gname.to_ascii_uppercase();
        let cg_prefix = format!("CPACK_COMPONENT_GROUP_{upper}_");
        let group = GroupMeta {
            name: gname.into(),
            display_name: option_set(cfg, format!("{cg_prefix}DISPLAY_NAME").as_bytes())
                .map_or_else(|| gname.into(), BString::from),
            description: option_set(cfg, format!("{cg_prefix}DESCRIPTION").as_bytes())
                .map_or_else(BString::default, BString::from),
            parent_group: None,
            expanded: cfg.on(format!("{cg_prefix}EXPANDED").as_bytes()),
            bold_title: cfg.on(format!("{cg_prefix}BOLD_TITLE").as_bytes()),
            subgroups: Vec::new(),
            components: Vec::new(),
        };
        let id = option_set(
            cfg,
            format!("CPACK_IFW_COMPONENT_GROUP_{upper}_NAME").as_bytes(),
        )
        .map(|v| require_utf8(v, "CPACK_IFW_COMPONENT_GROUP_NAME").map(str::to_owned))
        .transpose()?
        .unwrap_or_else(|| gname.to_owned());
        let mut ext = BTreeMap::new();
        let mut plan = build_group_package_plan(host, cfg, cwd, settings, &id, &group, &mut ext)?;
        if cfg
            .get(format!("CPACK_IFW_COMPONENT_GROUP_{upper}_FORCED_INSTALLATION").as_bytes())
            .is_none()
        {
            plan.forced_installation = Some("true".to_owned());
        }
        return Ok(plan);
    }

    let id = require_utf8(
        option_set(cfg, b"CPACK_IFW_PACKAGE_NAME")
            .or_else(|| option_set(cfg, b"CPACK_PACKAGE_NAME"))
            .unwrap_or(b"root"),
        "CPACK_IFW_PACKAGE_NAME",
    )?
    .to_owned();
    let mut display_name = BTreeMap::new();
    display_name.insert(
        String::new(),
        require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_NAME").unwrap_or(b"Your package"),
            "CPACK_PACKAGE_NAME",
        )?
        .to_owned(),
    );
    let mut description = BTreeMap::new();
    description.insert(
        String::new(),
        require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_DESCRIPTION_SUMMARY")
                .unwrap_or(b"Your package description"),
            "CPACK_PACKAGE_DESCRIPTION_SUMMARY",
        )?
        .to_owned(),
    );
    Ok(PackagePlan {
        id,
        display_name,
        description,
        version: settings.version.clone(),
        release_date: None,
        script: None,
        licenses: Vec::new(),
        user_interfaces: Vec::new(),
        translations: Vec::new(),
        dependencies: BTreeSet::new(),
        auto_depend_on: BTreeSet::new(),
        sorting_priority: None,
        update_text: None,
        default_selected: None,
        essential: None,
        is_virtual: None,
        forced_installation: Some("true".to_owned()),
        replaces: Vec::new(),
        requires_admin_rights: None,
        checkable: None,
        downloaded: false,
        source_components: Vec::new(),
    })
}

fn build_group_package_plan(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    settings: &InstallerSettings,
    pkg_id: &str,
    group: &GroupMeta,
    external_deps: &mut BTreeMap<String, DependencySpec>,
) -> Result<PackagePlan, CpackError> {
    let upper = BString::from(group.name.to_ascii_uppercase());
    let prefix = format!("CPACK_IFW_COMPONENT_GROUP_{upper}_");
    let mut display_name = BTreeMap::new();
    display_name.insert(
        String::new(),
        require_utf8(&group.display_name, "CPack group display name")?.to_owned(),
    );
    let mut description = BTreeMap::new();
    description.insert(
        String::new(),
        require_utf8(&group.description, "CPack group description")?.to_owned(),
    );
    let mut plan = PackagePlan {
        id: pkg_id.to_owned(),
        display_name,
        description,
        version: option_set(cfg, format!("{prefix}VERSION").as_bytes())
            .map(|v| require_utf8(v, "CPACK_IFW_COMPONENT_GROUP_VERSION").map(str::to_owned))
            .transpose()?
            .unwrap_or_else(|| settings.version.clone()),
        release_date: None,
        script: None,
        licenses: Vec::new(),
        user_interfaces: Vec::new(),
        translations: Vec::new(),
        dependencies: BTreeSet::new(),
        auto_depend_on: BTreeSet::new(),
        sorting_priority: None,
        update_text: None,
        default_selected: None,
        essential: None,
        is_virtual: None,
        forced_installation: None,
        replaces: Vec::new(),
        requires_admin_rights: None,
        checkable: None,
        downloaded: false,
        source_components: Vec::new(),
    };
    apply_common_ifw_prefix(host, cfg, cwd, &prefix, &mut plan, external_deps)?;
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
fn build_component_package_plan(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    settings: &InstallerSettings,
    pkg_id: &str,
    comp: &ComponentMeta,
    comp_pkg_ids: &BTreeMap<BString, String>,
    external_deps: &mut BTreeMap<String, DependencySpec>,
) -> Result<PackagePlan, CpackError> {
    let upper = BString::from(comp.name.to_ascii_uppercase());
    let prefix = format!("CPACK_IFW_COMPONENT_{upper}_");
    let mut display_name = BTreeMap::new();
    display_name.insert(
        String::new(),
        require_utf8(&comp.display_name, "CPack component display name")?.to_owned(),
    );
    let mut description = BTreeMap::new();
    description.insert(
        String::new(),
        require_utf8(&comp.description, "CPack component description")?.to_owned(),
    );
    let mut dependencies = BTreeSet::new();
    for dep in &comp.depends {
        if let Some(dep_pkg) = comp_pkg_ids.get(dep)
            && dep_pkg != pkg_id
        {
            dependencies.insert(DependencySpec::parse(dep_pkg));
        }
    }
    let mut plan = PackagePlan {
        id: pkg_id.to_owned(),
        display_name,
        description,
        version: option_set(cfg, format!("{prefix}VERSION").as_bytes())
            .map(|v| require_utf8(v, "CPACK_IFW_COMPONENT_VERSION").map(str::to_owned))
            .transpose()?
            .unwrap_or_else(|| settings.version.clone()),
        release_date: None,
        script: None,
        licenses: Vec::new(),
        user_interfaces: Vec::new(),
        translations: Vec::new(),
        dependencies,
        auto_depend_on: BTreeSet::new(),
        sorting_priority: None,
        update_text: None,
        default_selected: Some(if comp.disabled { "false" } else { "true" }.to_owned()),
        essential: cfg
            .on(format!("{prefix}ESSENTIAL").as_bytes())
            .then(|| "true".to_owned()),
        is_virtual: comp.hidden.then(|| "true".to_owned()),
        forced_installation: Some(if comp.required { "true" } else { "false" }.to_owned()),
        replaces: Vec::new(),
        requires_admin_rights: None,
        checkable: None,
        downloaded: comp.downloaded,
        source_components: Vec::new(),
    };
    apply_common_ifw_prefix(host, cfg, cwd, &prefix, &mut plan, external_deps)?;
    Ok(plan)
}

fn apply_common_ifw_prefix(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    prefix: &str,
    plan: &mut PackagePlan,
    external_deps: &mut BTreeMap<String, DependencySpec>,
) -> Result<(), CpackError> {
    let key = |s: &str| format!("{prefix}{s}");

    if is_set_to_empty(cfg, key("DISPLAY_NAME").as_bytes()) {
        plan.display_name.clear();
    } else if let Some(raw) = option_set(cfg, key("DISPLAY_NAME").as_bytes()) {
        expand_localized_list(raw, &mut plan.display_name)?;
    }

    if is_set_to_empty(cfg, key("DESCRIPTION").as_bytes()) {
        plan.description.clear();
    } else if let Some(raw) = option_set(cfg, key("DESCRIPTION").as_bytes()) {
        expand_localized_list(raw, &mut plan.description)?;
    }

    if is_set_to_empty(cfg, key("RELEASE_DATE").as_bytes()) {
        plan.release_date = None;
    } else if let Some(raw) = option_set(cfg, key("RELEASE_DATE").as_bytes()) {
        plan.release_date = Some(require_utf8(raw, "RELEASE_DATE")?.to_owned());
    }

    if let Some(raw) = option_set(cfg, key("PRIORITY").as_bytes()) {
        plan.sorting_priority = Some(require_utf8(raw, "PRIORITY")?.to_owned());
    }
    if is_set_to_empty(cfg, key("SORTING_PRIORITY").as_bytes()) {
        plan.sorting_priority = None;
    } else if let Some(raw) = option_set(cfg, key("SORTING_PRIORITY").as_bytes()) {
        plan.sorting_priority = Some(require_utf8(raw, "SORTING_PRIORITY")?.to_owned());
    }

    if is_set_to_empty(cfg, key("UPDATE_TEXT").as_bytes()) {
        plan.update_text = None;
    } else if let Some(raw) = option_set(cfg, key("UPDATE_TEXT").as_bytes()) {
        plan.update_text = Some(require_utf8(raw, "UPDATE_TEXT")?.to_owned());
    }

    if let Some(raw) = option_set(cfg, key("SCRIPT").as_bytes()) {
        plan.script = Some(absolute(cwd, raw));
    }
    if let Some(raw) = option_set(cfg, key("USER_INTERFACES").as_bytes()) {
        plan.user_interfaces = list::split(raw, list::Empty::Drop)
            .into_iter()
            .map(|p| absolute(cwd, &p))
            .collect();
    }
    if is_set_to_empty(cfg, key("TRANSLATIONS").as_bytes()) {
        plan.translations.clear();
    } else if let Some(raw) = option_set(cfg, key("TRANSLATIONS").as_bytes()) {
        plan.translations = list::split(raw, list::Empty::Drop)
            .into_iter()
            .map(|p| absolute(cwd, &p))
            .collect();
    }
    if let Some(raw) = option_set(cfg, key("LICENSES").as_bytes()) {
        let items = list::split(raw, list::Empty::Drop);
        if items.len().is_multiple_of(2) {
            plan.licenses.clear();
            for [title, path] in items.as_chunks::<2>().0 {
                plan.licenses.push((
                    require_utf8(title, "IFW LICENSES")?.to_owned(),
                    absolute(cwd, path),
                ));
            }
        } else {
            warn(
                host,
                format!("{prefix}LICENSES should contain pairs of <display_name> and <file_path>."),
            );
        }
    }

    for dep_var in ["DEPENDS", "DEPENDENCIES"] {
        for raw in cfg.list(key(dep_var).as_bytes()) {
            let spec = DependencySpec::parse(require_utf8(&raw, dep_var)?);
            external_deps
                .entry(spec.name.clone())
                .or_insert_with(|| spec.clone());
            plan.dependencies.insert(spec);
        }
    }
    if is_set_to_empty(cfg, key("AUTO_DEPEND_ON").as_bytes()) {
        plan.auto_depend_on.clear();
    } else {
        for raw in cfg.list(key("AUTO_DEPEND_ON").as_bytes()) {
            let spec = DependencySpec::parse(require_utf8(&raw, "AUTO_DEPEND_ON")?);
            external_deps
                .entry(spec.name.clone())
                .or_insert_with(|| spec.clone());
            plan.auto_depend_on.insert(spec);
        }
    }

    if is_set_to_empty(cfg, key("VIRTUAL").as_bytes()) {
        plan.is_virtual = None;
    } else if cfg.on(key("VIRTUAL").as_bytes()) {
        plan.is_virtual = Some("true".to_owned());
    }

    if is_set_to_empty(cfg, key("DEFAULT").as_bytes()) {
        plan.default_selected = None;
    } else if let Some(raw) = option_set(cfg, key("DEFAULT").as_bytes()) {
        let s = require_utf8(raw, "DEFAULT")?;
        let lower = s.to_ascii_lowercase();
        plan.default_selected = Some(match lower.as_str() {
            "true" => "true".to_owned(),
            "false" => "false".to_owned(),
            "script" => "script".to_owned(),
            _ => s.to_owned(),
        });
    }

    for (var, field) in [
        ("FORCED_INSTALLATION", &mut plan.forced_installation),
        ("REQUIRES_ADMIN_RIGHTS", &mut plan.requires_admin_rights),
        ("CHECKABLE", &mut plan.checkable),
    ] {
        let k = key(var);
        if is_set_to_empty(cfg, k.as_bytes()) {
            *field = None;
        } else if cfg.on(k.as_bytes()) {
            *field = Some("true".to_owned());
        } else if is_set_to_off(cfg, k.as_bytes()) {
            *field = Some("false".to_owned());
        }
    }

    if is_set_to_empty(cfg, key("REPLACES").as_bytes()) {
        plan.replaces.clear();
    } else if let Some(raw) = option_set(cfg, key("REPLACES").as_bytes()) {
        plan.replaces = list::split(raw, list::Empty::Drop)
            .into_iter()
            .map(|v| require_utf8(&v, "REPLACES").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
    }

    Ok(())
}

fn expand_localized_list(
    raw: &[u8],
    target: &mut BTreeMap<String, String>,
) -> Result<(), CpackError> {
    let items = list::split(raw, list::Empty::Drop);
    if items.is_empty() {
        return Ok(());
    }
    let mut idx = 0;
    if items.len() % 2 == 1 {
        if let Some(first) = items.first() {
            target.insert(
                String::new(),
                require_utf8(first, "IFW localized text")?.to_owned(),
            );
        }
        idx = 1;
    }
    while idx + 1 < items.len() {
        if let (Some(lang), Some(val)) = (items.get(idx), items.get(idx + 1)) {
            target.insert(
                require_utf8(lang, "IFW locale")?.to_owned(),
                require_utf8(val, "IFW localized text")?.to_owned(),
            );
        }
        idx += 2;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn check_deep_package_chain(count: usize, cyclic: bool) -> Result<(), CpackError> {
    let mut plans = BTreeMap::new();
    for i in 0..count {
        let id = format!("pkg.{i}");
        let mut dependencies = BTreeSet::new();
        if i + 1 < count {
            dependencies.insert(DependencySpec::parse(&format!("pkg.{}", i + 1)));
        } else if cyclic && count > 1 {
            dependencies.insert(DependencySpec::parse("pkg.0"));
        }
        plans.insert(
            id.clone(),
            PackagePlan {
                id,
                display_name: BTreeMap::new(),
                description: BTreeMap::new(),
                version: "1.0".to_owned(),
                release_date: None,
                script: None,
                licenses: Vec::new(),
                user_interfaces: Vec::new(),
                translations: Vec::new(),
                dependencies,
                auto_depend_on: BTreeSet::new(),
                sorting_priority: None,
                update_text: None,
                default_selected: None,
                essential: None,
                is_virtual: None,
                forced_installation: None,
                replaces: Vec::new(),
                requires_admin_rights: None,
                checkable: None,
                downloaded: false,
                source_components: Vec::new(),
            },
        );
    }
    check_ifw_package_cycles(&plans)
}

fn check_ifw_package_cycles(plans: &BTreeMap<String, PackagePlan>) -> Result<(), CpackError> {
    let mut state = BTreeMap::<&str, VisitState>::new();
    let internal_deps = |id: &str| -> std::vec::IntoIter<&str> {
        plans
            .get(id)
            .map(|pkg| {
                pkg.dependencies
                    .iter()
                    .chain(&pkg.auto_depend_on)
                    .filter_map(|dep| {
                        plans
                            .get(dep.name.as_str())
                            .map(|target| target.id.as_str())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
    };

    for start in plans.keys() {
        let start_id = start.as_str();
        if state.contains_key(start_id) {
            continue;
        }
        state.insert(start_id, VisitState::Visiting);
        let mut stack = vec![(start_id, internal_deps(start_id))];
        while let Some((node_id, deps_iter)) = stack.last_mut() {
            if let Some(target_id) = deps_iter.next() {
                match state.get(target_id) {
                    Some(VisitState::Visiting) => {
                        return Err(error(format!(
                            "Cycle detected in IFW package dependencies involving \"{node_id}\" and \"{target_id}\""
                        )));
                    }
                    Some(VisitState::Visited) => {}
                    None => {
                        state.insert(target_id, VisitState::Visiting);
                        stack.push((target_id, internal_deps(target_id)));
                    }
                }
            } else {
                let finished = *node_id;
                stack.pop();
                state.insert(finished, VisitState::Visited);
            }
        }
    }
    Ok(())
}

fn generated_by_comment(settings: &InstallerSettings) -> String {
    let ver = if settings.framework_version.is_equal("1.9.9") {
        "less 2.0"
    } else {
        settings.framework_version.as_str()
    };
    format!("Generated by CPack {CMAKE_VERSION_STR} IFW generator for QtIFW {ver} tools")
}

fn copy_meta_asset(
    host: &dyn Host,
    src: &Path,
    dest_dir: &Path,
    override_name: Option<&str>,
    seen_names: &mut BTreeMap<String, PathBuf>,
) -> Result<String, CpackError> {
    let meta = host.metadata_fresh(src, Follow::Yes).map_err(|err| {
        error(format!(
            "Cannot read IFW asset file '{}': {err}",
            src.display()
        ))
    })?;
    if meta.kind != FileKind::File {
        return Err(error(format!(
            "Expected regular IFW asset file at '{}'",
            src.display()
        )));
    }
    let filename = match override_name {
        Some(n) => n.to_owned(),
        None => src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| error(format!("Invalid IFW asset filename: {}", src.display())))?
            .to_owned(),
    };
    let safe = safe_name(filename.as_bytes())?;
    if let Some(prev) = seen_names.get(&filename)
        && prev != src
    {
        return Err(error(format!(
            "Duplicate IFW asset filename collision for '{filename}' ({} vs {})",
            prev.display(),
            src.display()
        )));
    }
    seen_names.insert(filename.clone(), src.to_path_buf());
    let dest = dest_dir.join(safe);
    prepare_fresh_output(host, &dest)?;
    host.copy_file(
        src,
        &dest,
        CopyOptions {
            preserve_permissions: true,
            preserve_mtime: true,
        },
    )?;
    Ok(filename)
}

fn write_config_xml(
    host: &dyn Host,
    settings: &InstallerSettings,
    config_dir: &Path,
    resources_dir: &Path,
) -> Result<Vec<PathBuf>, CpackError> {
    let mut w = XmlWriter::new("    ", 0);
    w.declaration();
    w.comment(&generated_by_comment(settings))?;
    w.start_element("Installer")?;

    w.element_text("Name", &settings.name)?;
    w.element_text("Version", &settings.version)?;
    w.element_text("Title", &settings.title)?;
    if let Some(ref pub_name) = settings.publisher {
        w.element_text("Publisher", pub_name)?;
    }
    if let Some(ref url) = settings.product_url {
        w.element_text("ProductUrl", url)?;
    }

    let mut seen_assets = BTreeMap::new();
    if let Some(ref logo) = settings.logo {
        let ext = logo
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let name = copy_meta_asset(
            host,
            logo,
            config_dir,
            Some(&format!("cm_logo{ext}")),
            &mut seen_assets,
        )?;
        w.element_text("Logo", &name)?;
    }
    if let Some(ref banner) = settings.banner {
        let name = copy_meta_asset(host, banner, config_dir, None, &mut seen_assets)?;
        w.element_text("Banner", &name)?;
    }
    if let Some(ref wm) = settings.watermark {
        let name = copy_meta_asset(host, wm, config_dir, None, &mut seen_assets)?;
        w.element_text("Watermark", &name)?;
    }
    if let Some(ref bg) = settings.background {
        let name = copy_meta_asset(host, bg, config_dir, None, &mut seen_assets)?;
        w.element_text("Background", &name)?;
    }

    if !settings.framework_version.is_less_than("1.4") {
        if let Some(ref app_icon) = settings.app_icon {
            let ext = app_icon
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let _ = copy_meta_asset(
                host,
                app_icon,
                config_dir,
                Some(&format!("cm_appicon{ext}")),
                &mut seen_assets,
            )?;
            w.element_text("InstallerApplicationIcon", "cm_appicon")?;
        }
        if let Some(ref win_icon) = settings.window_icon {
            let ext = win_icon
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let name = copy_meta_asset(
                host,
                win_icon,
                config_dir,
                Some(&format!("cm_winicon{ext}")),
                &mut seen_assets,
            )?;
            w.element_text("InstallerWindowIcon", &name)?;
        }
    }

    if !settings.framework_version.is_less_than("2.0") {
        if let Some(ref width) = settings.wizard_width {
            w.element_text("WizardDefaultWidth", width)?;
        }
        if let Some(ref height) = settings.wizard_height {
            w.element_text("WizardDefaultHeight", height)?;
        }
        if let Some(ref sm) = settings.start_menu_dir {
            w.element_text("StartMenuDir", sm)?;
        }
        if let Some(ref mt) = settings.maintenance_tool_name {
            w.element_text("MaintenanceToolName", mt)?;
        }
        if let Some(ref mti) = settings.maintenance_tool_ini {
            w.element_text("MaintenanceToolIniFile", mti)?;
        }
        if let Some(ref na) = settings.allow_non_ascii {
            w.element_text("AllowNonAsciiCharacters", na)?;
        }
        if let Some(ref sp) = settings.allow_space_in_path {
            w.element_text("AllowSpaceInPath", sp)?;
        }
        if let Some(ref cs) = settings.control_script {
            let name = copy_meta_asset(host, cs, config_dir, None, &mut seen_assets)?;
            w.element_text("ControlScript", &name)?;
        }
    } else {
        w.comment("CPack IFW default policy for QtIFW less 2.0")?;
        w.element_text("AllowNonAsciiCharacters", "true")?;
        w.element_text("AllowSpaceInPath", "true")?;
    }

    w.element_text("TargetDir", &settings.target_dir)?;
    if let Some(ref admin_dir) = settings.admin_target_dir {
        w.element_text("AdminTargetDir", admin_dir)?;
    }

    if !settings.remote_repositories.is_empty() {
        w.start_element("RemoteRepositories")?;
        for repo in &settings.remote_repositories {
            w.start_element("Repository")?;
            w.element_text("Url", &repo.url)?;
            if let Some(ref en) = repo.enabled {
                w.element_text("Enabled", en)?;
            }
            if let Some(ref user) = repo.username {
                w.element_text("Username", user)?;
            }
            if let Some(ref pwd) = repo.password {
                w.element_text("Password", pwd)?;
            }
            if let Some(ref dn) = repo.display_name {
                w.element_text("DisplayName", dn)?;
            }
            w.end_element("Repository")?;
        }
        w.end_element("RemoteRepositories")?;
    }

    if !settings.framework_version.is_less_than("3.0") {
        if let Some(ref style) = settings.wizard_style {
            w.element_text("WizardStyle", style)?;
        }
        if let Some(ref sheet) = settings.style_sheet {
            let name = copy_meta_asset(host, sheet, config_dir, None, &mut seen_assets)?;
            w.element_text("StyleSheet", &name)?;
        }
        if let Some(ref color) = settings.title_color {
            w.element_text("TitleColor", color)?;
        }
    }

    if !settings.framework_version.is_less_than("4.0") {
        if let Some(ref pl) = settings.wizard_show_page_list {
            w.element_text("WizardShowPageList", pl)?;
        }
        if let Some(ref cli) = settings.disable_cli {
            w.element_text("DisableCommandLineInterface", cli)?;
        }
        if let Some(ref rp) = settings.run_program {
            w.element_text("RunProgram", rp)?;
        }
        if !settings.run_program_args.is_empty() {
            w.start_element("RunProgramArguments")?;
            for arg in &settings.run_program_args {
                w.element_text("Argument", arg)?;
            }
            w.end_element("RunProgramArguments")?;
        }
        if let Some(ref desc) = settings.run_program_desc {
            w.element_text("RunProgramDescription", desc)?;
        }
    }

    if let Some(ref rm) = settings.remove_target_dir {
        w.element_text("RemoveTargetDir", rm)?;
    }

    if !settings.framework_version.is_less_than("4.0") && !settings.product_images.is_empty() {
        w.start_element("ProductImages")?;
        let has_urls = !settings.product_image_urls.is_empty();
        for (idx, img) in settings.product_images.iter().enumerate() {
            w.start_element("ProductImage")?;
            let name = copy_meta_asset(host, img, config_dir, None, &mut seen_assets)?;
            w.element_text("Image", &name)?;
            if has_urls && let Some(url) = settings.product_image_urls.get(idx) {
                w.element_text("Url", url)?;
            }
            w.end_element("ProductImage")?;
        }
        w.end_element("ProductImages")?;
    }

    w.end_element("Installer")?;
    let xml = w.finish()?;
    let config_path = config_dir.join("config.xml");
    prepare_fresh_output(host, &config_path)?;
    host.write(&config_path, xml.as_bytes(), WriteMode::CreateNew)?;

    let mut staged_qrc = Vec::new();
    if !settings.resources_qrc.is_empty() {
        host.create_dir_all(resources_dir)?;
        for (idx, qrc_path) in settings.resources_qrc.iter().enumerate() {
            if !host
                .metadata_fresh(qrc_path, Follow::Yes)
                .is_ok_and(|m| m.kind == FileKind::File)
            {
                warn(
                    host,
                    format!(
                        "Can't copy resources from \"{}\". Resource will be skipped.",
                        qrc_path.display()
                    ),
                );
                continue;
            }
            let bytes = match host.read(
                qrc_path,
                ReadRange::All {
                    limit: 4 * 1024 * 1024,
                },
            ) {
                Ok(b) => b,
                Err(_) => {
                    warn(
                        host,
                        format!(
                            "Can't copy resources from \"{}\". Resource will be skipped.",
                            qrc_path.display()
                        ),
                    );
                    continue;
                }
            };
            let root = parse_xml_bounded(&bytes, &qrc_path.display().to_string())?;
            let qrc_base = qrc_path.parent().unwrap_or(Path::new("/"));
            let canon_qrc_base = host.canonicalize_fresh(qrc_base)?;
            let mut file_entries = Vec::new();
            collect_qrc_files(&root, &mut file_entries);
            if file_entries.is_empty() {
                warn(
                    host,
                    format!(
                        "Can't copy resources from \"{}\". Resource will be skipped.",
                        qrc_path.display()
                    ),
                );
                continue;
            }
            let mut validated_files = Vec::with_capacity(file_entries.len());
            let mut missing_asset = false;
            for rel_str in file_entries {
                let rel = safe_relative_subpath(&rel_str, &qrc_path.display().to_string())?;
                let src = qrc_base.join(&rel);
                if !host
                    .metadata_fresh(&src, Follow::Yes)
                    .is_ok_and(|m| m.kind == FileKind::File)
                {
                    missing_asset = true;
                    break;
                }
                let canon_src = host.canonicalize_fresh(&src)?;
                if !canon_src.starts_with(&canon_qrc_base) {
                    return Err(error(format!(
                        "Path in {} escapes base directory: \"{rel_str}\"",
                        qrc_path.display()
                    )));
                }
                validated_files.push((rel, src));
            }
            if missing_asset {
                warn(
                    host,
                    format!(
                        "Can't copy resources from \"{}\". Resource will be skipped.",
                        qrc_path.display()
                    ),
                );
                continue;
            }
            let qrc_stage_dir = resources_dir.join(idx.to_string());
            host.create_dir_all(&qrc_stage_dir)?;
            let mut copy_failed = false;
            for (rel, src) in validated_files {
                let dst = qrc_stage_dir.join(&rel);
                if let Some(parent) = dst.parent() {
                    crate::stage::ensure_dir(host, &qrc_stage_dir, parent)?;
                }
                prepare_fresh_output(host, &dst)?;
                if host
                    .copy_file(
                        &src,
                        &dst,
                        CopyOptions {
                            preserve_permissions: true,
                            preserve_mtime: true,
                        },
                    )
                    .is_err()
                {
                    copy_failed = true;
                    break;
                }
            }
            if copy_failed {
                let _ = host.remove_dir_all(&qrc_stage_dir);
                warn(
                    host,
                    format!(
                        "Can't copy resources from \"{}\". Resource will be skipped.",
                        qrc_path.display()
                    ),
                );
                continue;
            }
            let mut seen_qrc = BTreeMap::new();
            let qrc_name = copy_meta_asset(host, qrc_path, &qrc_stage_dir, None, &mut seen_qrc)?;
            staged_qrc.push(qrc_stage_dir.join(qrc_name));
        }
    }
    Ok(staged_qrc)
}

fn collect_qrc_files(elem: &XmlElement, out: &mut Vec<String>) {
    if elem.name == "file" {
        let mut text = String::new();
        for child in &elem.children {
            if let XmlNode::Text(t) = child {
                text.push_str(t);
            }
        }
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
        return;
    }
    for child in &elem.children {
        if let XmlNode::Element(sub) = child {
            collect_qrc_files(sub, out);
        }
    }
}

fn write_package_xml(
    host: &dyn Host,
    settings: &InstallerSettings,
    pkg: &PackagePlan,
    meta_dir: &Path,
) -> Result<(), CpackError> {
    let mut w = XmlWriter::new("    ", 0);
    w.declaration();
    w.comment(&generated_by_comment(settings))?;
    w.start_element("Package")?;

    for (lang, val) in &pkg.display_name {
        w.start_element("DisplayName")?;
        if !lang.is_empty() {
            w.attr("xml:lang", lang)?;
        }
        w.text(val)?;
        w.end_element("DisplayName")?;
    }
    for (lang, val) in &pkg.description {
        w.start_element("Description")?;
        if !lang.is_empty() {
            w.attr("xml:lang", lang)?;
        }
        w.text(val)?;
        w.end_element("Description")?;
    }
    if let Some(ref update_text) = pkg.update_text {
        w.element_text("UpdateText", update_text)?;
    }
    w.element_text("Name", &pkg.id)?;
    w.element_text("Version", &pkg.version)?;
    let release_date = pkg
        .release_date
        .clone()
        .unwrap_or_else(|| utc_date_from_secs(host.now().secs));
    w.element_text("ReleaseDate", &release_date)?;

    let mut seen_assets = BTreeMap::new();
    if let Some(ref script) = pkg.script {
        let name = copy_meta_asset(host, script, meta_dir, None, &mut seen_assets)?;
        w.element_text("Script", &name)?;
    }
    if !pkg.user_interfaces.is_empty() {
        w.start_element("UserInterfaces")?;
        for ui in &pkg.user_interfaces {
            let name = copy_meta_asset(host, ui, meta_dir, None, &mut seen_assets)?;
            w.element_text("UserInterface", &name)?;
        }
        w.end_element("UserInterfaces")?;
    }
    if !pkg.translations.is_empty() {
        w.start_element("Translations")?;
        for tr in &pkg.translations {
            let name = copy_meta_asset(host, tr, meta_dir, None, &mut seen_assets)?;
            w.element_text("Translation", &name)?;
        }
        w.end_element("Translations")?;
    }

    let hyphens_unsupported = settings.framework_version.is_less_than("3.1");
    let mut warn_hyphens = false;
    if !pkg.dependencies.is_empty() {
        let formatted: Vec<String> = pkg
            .dependencies
            .iter()
            .map(|d| {
                warn_hyphens |= hyphens_unsupported && d.name.contains('-');
                d.format_qt()
            })
            .collect();
        w.element_text("Dependencies", &formatted.join(","))?;
    }
    if !pkg.auto_depend_on.is_empty() {
        let formatted: Vec<String> = pkg
            .auto_depend_on
            .iter()
            .map(|d| {
                warn_hyphens |= hyphens_unsupported && d.name.contains('-');
                d.format_qt()
            })
            .collect();
        w.element_text("AutoDependOn", &formatted.join(","))?;
    }
    if warn_hyphens {
        warn(
            host,
            format!(
                "The dependencies for component \"{}\" specify names that contain hyphens. This requires QtIFW 3.1 or later, but you are using version {}",
                pkg.id,
                settings.framework_version.as_str()
            ),
        );
    }

    if !pkg.licenses.is_empty() {
        w.start_element("Licenses")?;
        for (title, path) in &pkg.licenses {
            let name = copy_meta_asset(host, path, meta_dir, None, &mut seen_assets)?;
            w.start_element("License")?;
            w.attr("name", title)?;
            w.attr("file", &name)?;
            w.end_element("License")?;
        }
        w.end_element("Licenses")?;
    }

    if let Some(ref fi) = pkg.forced_installation {
        w.element_text("ForcedInstallation", fi)?;
    }
    if !pkg.replaces.is_empty() {
        w.element_text("Replaces", &pkg.replaces.join(","))?;
    }
    if let Some(ref admin) = pkg.requires_admin_rights {
        w.element_text("RequiresAdminRights", admin)?;
    }
    if let Some(ref virt) = pkg.is_virtual {
        w.element_text("Virtual", virt)?;
    } else if let Some(ref def) = pkg.default_selected {
        w.element_text("Default", def)?;
    }
    if let Some(ref ess) = pkg.essential {
        w.element_text("Essential", ess)?;
    }
    if let Some(ref prio) = pkg.sorting_priority {
        w.element_text("SortingPriority", prio)?;
    }
    if let Some(ref chk) = pkg.checkable {
        w.element_text("Checkable", chk)?;
    }

    w.end_element("Package")?;
    let xml = w.finish()?;
    let out = meta_dir.join("package.xml");
    prepare_fresh_output(host, &out)?;
    host.write(&out, xml.as_bytes(), WriteMode::CreateNew)?;
    Ok(())
}

fn build_repogen_args(
    host: &dyn Host,
    settings: &InstallerSettings,
    config_xml: &Path,
    packages_dir: &Path,
    downloaded_ids: &[String],
    repo_dir: &Path,
) -> Vec<BString> {
    let mut args = Vec::new();
    if !settings.framework_version.is_less_than("4.2") {
        if let Some(ref fmt) = settings.archive_format {
            args.extend(["--archive-format".into(), fmt.clone().into()]);
        }
        if let Some(ref comp) = settings.archive_compression {
            args.extend(["--compression".into(), comp.clone().into()]);
        }
    }
    if settings.framework_version.is_less_than("2.0.0") {
        args.extend(["-c".into(), from_native_cmake(config_xml)]);
    }
    args.extend(["-p".into(), from_native_cmake(packages_dir)]);
    for extra in &settings.extra_packages_dirs {
        args.extend(["-p".into(), from_native_cmake(extra)]);
    }
    if !settings.extra_repositories_dirs.is_empty() {
        if !settings.framework_version.is_less_than("3.1") {
            for rd in &settings.extra_repositories_dirs {
                args.extend(["--repository".into(), from_native_cmake(rd)]);
            }
        } else {
            warn(
                host,
                "CPACK_IFW_REPOSITORIES_DIRECTORIES is only supported since QtIFW 3.1.",
            );
        }
    }
    if !settings.online_only && !downloaded_ids.is_empty() {
        args.extend(["-i".into(), downloaded_ids.join(",").into()]);
    }
    args.push(from_native_cmake(repo_dir));
    args
}

#[allow(clippy::too_many_arguments)]
fn build_binarycreator_args(
    host: &dyn Host,
    settings: &InstallerSettings,
    config_xml: &Path,
    staged_qrc: &[PathBuf],
    packages_dir: &Path,
    binary_ids: &[String],
    downloaded_ids: &[String],
    external_deps: &BTreeMap<String, DependencySpec>,
    out_installer: &Path,
) -> Vec<BString> {
    let mut args = Vec::new();
    if !settings.framework_version.is_less_than("4.2") {
        if let Some(ref fmt) = settings.archive_format {
            args.extend(["--archive-format".into(), fmt.clone().into()]);
        }
        if let Some(ref comp) = settings.archive_compression {
            args.extend(["--compression".into(), comp.clone().into()]);
        }
    }
    if !settings.framework_version.is_less_than("3.0")
        && let Some(ref sign) = settings.signing_identity
    {
        args.extend(["--sign".into(), sign.clone().into()]);
    }
    args.extend(["-c".into(), from_native_cmake(config_xml)]);
    if !staged_qrc.is_empty() {
        let joined = staged_qrc
            .iter()
            .map(|path| from_native_cmake(path).to_string())
            .collect::<Vec<_>>()
            .join(",");
        args.extend(["-r".into(), joined.into()]);
    }
    args.extend(["-p".into(), from_native_cmake(packages_dir)]);
    for extra in &settings.extra_packages_dirs {
        args.extend(["-p".into(), from_native_cmake(extra)]);
    }
    if !settings.extra_repositories_dirs.is_empty() {
        if !settings.framework_version.is_less_than("3.1") {
            for rd in &settings.extra_repositories_dirs {
                args.extend(["--repository".into(), from_native_cmake(rd)]);
            }
        } else {
            warn(
                host,
                "CPACK_IFW_REPOSITORIES_DIRECTORIES is only supported since QtIFW 3.1.",
            );
        }
    }
    if settings.online_only {
        args.push("--online-only".into());
    } else if !downloaded_ids.is_empty() && !settings.remote_repositories.is_empty() {
        args.extend(["-e".into(), downloaded_ids.join(",").into()]);
    } else if !external_deps.is_empty() {
        let mut include = binary_ids.to_vec();
        for name in external_deps.keys() {
            include.push(name.clone());
        }
        args.extend(["-i".into(), include.join(",").into()]);
    }
    args.push(from_native_cmake(out_installer));
    args
}

/// Patch `<repo>/Updates.xml` safely using the bounded XML parser and atomic rename.
fn patch_updates_xml(
    host: &dyn Host,
    settings: &InstallerSettings,
    repo_dir: &Path,
) -> Result<(), CpackError> {
    let updates_path = repo_dir.join("Updates.xml");
    let meta = host.metadata_fresh(&updates_path, Follow::No)?;
    if meta.kind != FileKind::File {
        return Err(error(format!(
            "Expected regular Updates.xml file at {}",
            updates_path.display()
        )));
    }
    let raw = host.read(
        &updates_path,
        ReadRange::All {
            limit: 4 * 1024 * 1024,
        },
    )?;
    let root = parse_xml_bounded(&raw, &updates_path.display().to_string())?;
    let mut w = XmlWriter::new("    ", 0);
    w.declaration();
    w.comment(&generated_by_comment(settings))?;
    let mut patched = false;
    rewrite_updates_element(&root, settings, &mut patched, &mut w)?;
    let xml = w.finish()?;

    let tmp_path = repo_dir.join("UpdatesPatch.xml");
    prepare_fresh_output(host, &tmp_path)?;
    if let Err(err) = host
        .write(&tmp_path, xml.as_bytes(), WriteMode::CreateNew)
        .and_then(|()| host.rename(&tmp_path, &updates_path))
    {
        let _ = host.remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}

fn rewrite_updates_element(
    elem: &XmlElement,
    settings: &InstallerSettings,
    patched: &mut bool,
    w: &mut XmlWriter,
) -> Result<(), CpackError> {
    w.start_element(&elem.name)?;
    for (k, v) in &elem.attributes {
        w.attr(k, v)?;
    }
    for child in &elem.children {
        match child {
            XmlNode::Text(t) => {
                if !t.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
                    w.text(t)?;
                }
            }
            XmlNode::Element(sub) => {
                rewrite_updates_element(sub, settings, patched, w)?;
                if !*patched && sub.name == "Checksum" {
                    emit_repository_updates(settings, w)?;
                    *patched = true;
                }
            }
        }
    }
    if !*patched && elem.name == "Updates" {
        emit_repository_updates(settings, w)?;
        *patched = true;
    }
    w.end_element(&elem.name)
}

fn emit_repository_updates(
    settings: &InstallerSettings,
    w: &mut XmlWriter,
) -> Result<(), CpackError> {
    if settings.repository_updates.is_empty() {
        return Ok(());
    }
    w.start_element("RepositoryUpdate")?;
    for repo in &settings.repository_updates {
        w.start_element("Repository")?;
        match repo.update {
            RepoUpdateKind::Add => {
                w.attr("action", "add")?;
                w.attr("url", &repo.url)?;
            }
            RepoUpdateKind::Remove => {
                w.attr("action", "remove")?;
                w.attr("url", &repo.url)?;
            }
            RepoUpdateKind::Replace => {
                w.attr("action", "replace")?;
                w.attr("oldUrl", &repo.old_url)?;
                w.attr("newUrl", &repo.new_url)?;
            }
            RepoUpdateKind::None => {}
        }
        if let Some(ref en) = repo.enabled {
            w.attr("enabled", en)?;
        }
        if let Some(ref user) = repo.username {
            w.attr("username", user)?;
        }
        if let Some(ref pwd) = repo.password {
            w.attr("password", pwd)?;
        }
        if let Some(ref dn) = repo.display_name {
            w.attr("displayname", dn)?;
        }
        w.end_element("Repository")?;
    }
    w.end_element("RepositoryUpdate")
}

/// Convert Unix epoch seconds to UTC `YYYY-MM-DD` (Howard Hinnant's civil-from-days algorithm).
fn utc_date_from_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
