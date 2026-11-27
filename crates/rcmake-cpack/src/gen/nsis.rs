//! Nullsoft Scriptable Install System (`NSIS` and `NSIS64`) package generator.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_archive::{ArchiveFormat, CreateOptions, create_archive};
use rcmake_base::BString;
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake, to_native};
use rcmake_host::{FileKind, Follow, Host, ProcessResult, RegistryRoot, RegistryView, WriteMode};

use super::installer::{
    ComponentCatalog, ComponentMeta, GroupMeta, PROBE_TIMEOUT, build_tool_timeout,
    check_tool_outcome, escape_nsis_literal, find_program_on_host, find_template, is_windows_host,
    option_set, prepare_fresh_output, read_template, reject_nsis_glob_hazard, render_at_template,
    require_utf8, run_installer_tool, to_windows_slashes, verify_fresh_output, warn,
    write_tool_log,
};
use crate::config::{CpackConfig, absolute, snapshot};
use crate::source::{
    SymlinkPolicy, canonical_checked_root, validate_staged_tree, verify_contained_path,
};
use crate::{
    ComponentMembers, CpackError, PackageContext, PackageGenerator, error, log, safe_name,
};

/// The `NSIS` (32-bit) and `NSIS64` (64-bit) generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nsis {
    bits: u8,
}

impl Nsis {
    pub(crate) const fn bits32() -> Self {
        Self { bits: 32 }
    }

    pub(crate) const fn bits64() -> Self {
        Self { bits: 64 }
    }
}

/// Validated configuration for one NSIS run.
struct Settings {
    makensis: BString,
    opt_prefix: &'static str,
    windows_host: bool,
    pre_args: Vec<BString>,
    post_args: Vec<BString>,
    executables_dir: String,
    compressor: String,
    install_root: String,
    install_dir: String,
    registry_key: String,
    display_name: String,
    package_name: String,
    package_vendor: String,
    package_version: String,
    package_version_patch: String,
    uninstall_name: String,
}

impl PackageGenerator for Nsis {
    fn name(&self) -> &'static str {
        if self.bits == 64 { "NSIS64" } else { "NSIS" }
    }

    fn extension(&self) -> &'static str {
        ".exe"
    }

    fn prepare_config(
        &self,
        host: &dyn Host,
        interp: &mut Interp,
        cwd: &Path,
    ) -> Result<(), CpackError> {
        let windows_host = is_windows_host(host);
        let opt_prefix = if windows_host { "/" } else { "-" };

        {
            let mut cx = interp.cx();
            if cx.var_is_on(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY") {
                warn(
                    host,
                    "NSIS Generator cannot work with CPACK_INCLUDE_TOPLEVEL_DIRECTORY set. This option will be reset to 0 (for this generator only).",
                );
                cx.unset_var(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY");
            }
            if !cx.var_defined(b"CPACK_NSIS_EXECUTABLE") {
                cx.set_var(b"CPACK_NSIS_EXECUTABLE", "makensis".into());
            }
        }

        let cfg = snapshot(interp);
        let mut extra_paths = Vec::new();
        let mut got_registry = false;
        if windows_host && let Some(reg_dir) = lookup_nsis_registry(host, self.bits == 64) {
            extra_paths.push(to_native(&reg_dir));
            got_registry = true;
        }

        let configured_exe = option_set(&cfg, b"CPACK_NSIS_EXECUTABLE").unwrap_or(b"makensis");
        let Some(makensis_path) =
            find_program_on_host(host, cwd, configured_exe, &extra_paths, &[])
        else {
            let mut msg = String::from(
                "Cannot find NSIS compiler makensis: likely it is not installed, or not in your PATH",
            );
            if windows_host && !got_registry {
                msg.push_str(
                    "\nCould not read NSIS registry value. This is usually caused by NSIS not being installed.",
                );
            }
            return Err(error(msg));
        };

        // Probe makensis version using /VERSION on Windows or -VERSION on POSIX.
        let version_arg = BString::from(format!("{opt_prefix}VERSION"));
        let makensis_bytes = from_native(&makensis_path);
        let outcome = run_installer_tool(
            host,
            cwd,
            &makensis_bytes,
            std::slice::from_ref(&version_arg),
            Some(PROBE_TIMEOUT),
        )?;
        let log_dir = cfg
            .get(b"CPACK_TOPLEVEL_DIRECTORY")
            .filter(|v| !v.is_empty())
            .map_or_else(|| cwd.to_owned(), |v| absolute(cwd, v));
        let log_path = log_dir.join("NSISOutput.log");
        if !matches!(outcome.status, ProcessResult::Exited(0)) {
            let _ = write_tool_log(
                host,
                &log_path,
                &makensis_bytes,
                std::slice::from_ref(&version_arg),
                &outcome.output,
                &[],
                false,
            );
            return Err(error(format!(
                "Problem checking NSIS version with {} {version_arg} ({:?})",
                BString::from(makensis_bytes.as_slice()),
                outcome.status
            )));
        }
        let version_text = String::from_utf8_lossy(&outcome.output);
        if let Err(reason) = validate_nsis_version(&version_text) {
            let _ = write_tool_log(
                host,
                &log_path,
                &makensis_bytes,
                std::slice::from_ref(&version_arg),
                &outcome.output,
                &[],
                false,
            );
            return Err(reason);
        }

        let mut cx = interp.cx();
        if !cx.var_defined(b"CPACK_INSTALLER_PROGRAM") {
            cx.set_var(b"CPACK_INSTALLER_PROGRAM", makensis_bytes);
        }
        if !cx.var_defined(b"CPACK_NSIS_EXECUTABLES_DIRECTORY") {
            cx.set_var(b"CPACK_NSIS_EXECUTABLES_DIRECTORY", "bin".into());
        }
        if !cx.var_defined(b"CPACK_NSIS_COMPRESSOR") {
            cx.set_var(b"CPACK_NSIS_COMPRESSOR", "lzma".into());
        }
        if !cx.var_defined(b"CPACK_NSIS_INSTALL_ROOT") {
            let root = if self.bits == 64 {
                "$PROGRAMFILES64"
            } else {
                "$PROGRAMFILES"
            };
            cx.set_var(b"CPACK_NSIS_INSTALL_ROOT", root.into());
        }
        if !cx.var_defined(b"CPACK_PACKAGE_VENDOR") {
            cx.set_var(b"CPACK_PACKAGE_VENDOR", "Humanity".into());
        }
        let pkg_name = cx.var_or_empty(b"CPACK_PACKAGE_NAME");
        let pkg_ver = cx.var_or_empty(b"CPACK_PACKAGE_VERSION");
        if !cx.var_defined(b"CPACK_PACKAGE_INSTALL_DIRECTORY") {
            cx.set_var(
                b"CPACK_PACKAGE_INSTALL_DIRECTORY",
                format!("{pkg_name} {pkg_ver}").into(),
            );
        }
        let install_dir = cx.var_or_empty(b"CPACK_PACKAGE_INSTALL_DIRECTORY");
        if !cx.var_defined(b"CPACK_PACKAGE_INSTALL_REGISTRY_KEY") {
            cx.set_var(b"CPACK_PACKAGE_INSTALL_REGISTRY_KEY", install_dir.clone());
        }
        let display_name_set = cx.var_defined(b"CPACK_NSIS_DISPLAY_NAME");
        if !display_name_set {
            cx.set_var(b"CPACK_NSIS_DISPLAY_NAME", install_dir.clone());
        }
        let display_name = cx.var_or_empty(b"CPACK_NSIS_DISPLAY_NAME");
        if !cx.var_defined(b"CPACK_NSIS_PACKAGE_NAME") {
            cx.set_var(
                b"CPACK_NSIS_PACKAGE_NAME",
                if display_name_set {
                    display_name
                } else {
                    install_dir
                },
            );
        }
        if !cx.var_defined(b"CPACK_NSIS_UNINSTALL_NAME") {
            cx.set_var(b"CPACK_NSIS_UNINSTALL_NAME", "Uninstall".into());
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

fn lookup_nsis_registry(host: &dyn Host, is_64: bool) -> Option<BString> {
    let host_view = host.registry_host_view();
    let mut queries = Vec::new();
    if is_64 {
        queries.push((b"SOFTWARE\\NSIS\\Unicode".as_slice(), RegistryView::Bits64));
        queries.push((b"SOFTWARE\\NSIS".as_slice(), RegistryView::Bits64));
    }
    queries.push((b"SOFTWARE\\NSIS\\Unicode".as_slice(), RegistryView::Bits32));
    if let Some(view) = host_view {
        queries.push((b"SOFTWARE\\NSIS\\Unicode".as_slice(), view));
    }
    queries.push((b"SOFTWARE\\NSIS".as_slice(), RegistryView::Bits32));
    if let Some(view) = host_view {
        queries.push((b"SOFTWARE\\NSIS".as_slice(), view));
    }

    for (key, view) in queries {
        if let Ok(Some(value)) = host.registry_value(RegistryRoot::LocalMachine, key, None, view)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// Compare decimal `major.frac` against `3.03` (matching CMake's floating-point `>= 3.03` check).
fn nsis_version_at_least_3_03(major: u64, frac_digits: &str) -> bool {
    if major > 3 {
        return true;
    }
    if major < 3 {
        return false;
    }
    let min_frac = b"03";
    let frac = frac_digits.as_bytes();
    let len = frac.len().max(min_frac.len());
    for i in 0..len {
        let a = frac.get(i).copied().unwrap_or(b'0');
        let b = min_frac.get(i).copied().unwrap_or(b'0');
        match a.cmp(&b) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    true
}

/// Validate `makensis` version output: require decimal version `>= 3.03` or legacy `v*.cvs`.
pub(crate) fn validate_nsis_version(output: &str) -> Result<(), CpackError> {
    let trimmed = output.trim();
    if let Some(v_pos) = trimmed.find('v') {
        let rest = trimmed.get(v_pos + 1..).unwrap_or("");
        let token = rest.split(|c: char| c.is_whitespace()).next().unwrap_or("");
        if token.ends_with(".cvs") && token.len() > 4 {
            return Ok(());
        }
        let mut parts = token.split('.');
        if let (Some(maj_raw), Some(min_raw)) = (parts.next(), parts.next()) {
            let maj_digits: String = maj_raw.chars().take_while(char::is_ascii_digit).collect();
            let min_digits: String = min_raw.chars().take_while(char::is_ascii_digit).collect();
            if !maj_digits.is_empty() && !min_digits.is_empty() {
                let major = maj_digits
                    .parse::<u64>()
                    .map_err(|_| error(format!("Malformed NSIS version string: \"{trimmed}\"")))?;
                if nsis_version_at_least_3_03(major, &min_digits) {
                    return Ok(());
                }
                return Err(error(format!(
                    "CPack requires NSIS Version 3.03 or greater. NSIS found on the system was: {trimmed}"
                )));
            }
        }
    }
    Err(error(format!(
        "Problem checking NSIS version: unrecognized version output \"{trimmed}\""
    )))
}

impl Nsis {
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
        let settings = Settings::from_config(&cfg, host)?;
        let canon_work = cx.expected_work_root()?;
        let canon_staging = cx.expected_staging_root()?;

        // NSIS does not support absolute install destinations.
        if let Some(abs_files) = option_set(&cfg, b"CPACK_ABSOLUTE_DESTINATION_FILES") {
            return Err(error(format!(
                "NSIS generator does not support absolute install destinations: {}",
                BString::from(abs_files)
            )));
        }
        if cfg.on(b"CPACK_SET_DESTDIR") {
            warn(
                host,
                "CPACK_SET_DESTDIR is set to ON; NSIS generator should not be used with DESTDIR",
            );
        }

        // NSIS packages Windows files (reject POSIX symlinks and enforce Windows path rules).
        validate_staged_tree(
            host,
            cx.staging_dir,
            &canon_staging,
            members,
            SymlinkPolicy::Reject,
            true,
        )?;

        let catalog = ComponentCatalog::from_config(&cfg, members)?;
        for name in catalog.components.keys() {
            validate_nsis_identifier(require_utf8(name, "NSIS component ID")?, "component")?;
        }
        for name in catalog.groups.keys() {
            validate_nsis_identifier(require_utf8(name, "NSIS group ID")?, "component group")?;
        }

        let mut comp_files: BTreeMap<BString, Vec<String>> = BTreeMap::new();
        let mut comp_dirs: BTreeMap<BString, Vec<String>> = BTreeMap::new();
        let mut delete_files = String::new();
        let mut delete_dirs = String::new();

        if catalog.components.is_empty() {
            let (files, dirs) = walk_nsis_tree(host, cx.staging_dir, &canon_staging)?;
            for rel in &files {
                reject_nsis_glob_hazard(rel, "NSIS payload file")?;
                let win = escape_nsis_literal(&rel.replace('/', "\\"), false)?;
                delete_files.push_str(&format!("  Delete \"$INSTDIR\\{win}\"\n"));
            }
            for rel in &dirs {
                reject_nsis_glob_hazard(rel, "NSIS payload directory")?;
                let win = escape_nsis_literal(&rel.replace('/', "\\"), false)?;
                delete_dirs.push_str(&format!("  RMDir \"$INSTDIR\\{win}\"\n"));
            }
        } else {
            for comp_name in catalog.components.keys() {
                let comp_str = require_utf8(comp_name, "CPack component name")?;
                let comp_dir =
                    crate::stage::component_dir_for_host(comp_name, settings.windows_host);
                let comp_path = cx.staging_dir.join(&comp_dir);
                match host.metadata_fresh(&comp_path, Follow::No) {
                    Ok(_) => {}
                    Err(err) if err.kind == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err.into()),
                }
                verify_contained_path(
                    host,
                    cx.staging_dir,
                    &canon_staging,
                    &comp_dir,
                    SymlinkPolicy::Reject,
                    false,
                )?;
                let canon_comp = canonical_checked_root(host, &comp_path)?;
                if !canon_comp.starts_with(&canon_staging) {
                    return Err(error(format!(
                        "NSIS component root escapes staging directory: {}",
                        comp_path.display()
                    )));
                }
                let (files, dirs) = walk_nsis_tree(host, &comp_path, &canon_comp)?;
                let out_dir_token = custom_component_install_dir(&cfg, comp_str)?;
                for rel in &files {
                    reject_nsis_glob_hazard(rel, "NSIS component file")?;
                    let win = escape_nsis_literal(&rel.replace('/', "\\"), false)?;
                    delete_files.push_str(&format!("  Delete \"{out_dir_token}\\{win}\"\n"));
                }
                for rel in &dirs {
                    reject_nsis_glob_hazard(rel, "NSIS component directory")?;
                    let win = escape_nsis_literal(&rel.replace('/', "\\"), false)?;
                    delete_dirs.push_str(&format!("  RMDir \"{out_dir_token}\\{win}\"\n"));
                }
                comp_files.insert(comp_name.clone(), files);
                comp_dirs.insert(comp_name.clone(), dirs);
            }
        }

        let mut subs = build_base_substitutions(&cfg, cx, &cwd, &settings)?;
        if !cfg.vars.contains_key(b"CPACK_NSIS_DELETE_FILES".as_slice()) {
            subs.insert("CPACK_NSIS_DELETE_FILES".to_owned(), delete_files);
        }
        if !cfg
            .vars
            .contains_key(b"CPACK_NSIS_DELETE_DIRECTORIES".as_slice())
        {
            subs.insert("CPACK_NSIS_DELETE_DIRECTORIES".to_owned(), delete_dirs);
        }

        let (create_icons, delete_icons) = render_shortcuts(&cfg, &settings)?;
        subs.entry("CPACK_NSIS_CREATE_ICONS".to_owned())
            .or_insert(create_icons);
        subs.entry("CPACK_NSIS_DELETE_ICONS".to_owned())
            .or_insert(delete_icons);

        if catalog.components.is_empty() {
            subs.entry("CPACK_NSIS_INSTALLATION_TYPES".to_owned())
                .or_default();
            subs.entry("CPACK_NSIS_INSTALLER_MUI_COMPONENTS_DESC".to_owned())
                .or_default();
            subs.entry("CPACK_NSIS_PAGE_COMPONENTS".to_owned())
                .or_default();
            subs.entry("CPACK_NSIS_FULL_INSTALL".to_owned())
                .or_insert_with(|| r#"File /r "${INST_DIR}\*.*""#.to_owned());
            subs.entry("CPACK_NSIS_COMPONENT_SECTIONS".to_owned())
                .or_default();
            subs.entry("CPACK_NSIS_COMPONENT_SECTION_LIST".to_owned())
                .or_default();
            subs.entry("CPACK_NSIS_SECTION_SELECTED_VARS".to_owned())
                .or_default();
        } else {
            render_components(
                host,
                &cfg,
                cx,
                &cwd,
                &settings,
                &catalog,
                &comp_files,
                &comp_dirs,
                &mut subs,
            )?;
        }

        let ini_tpl_path = find_template(host, &cfg, &cwd, "NSIS.InstallOptions.ini.in", None)?;
        let nsi_tpl_path = find_template(host, &cfg, &cwd, "NSIS.template.in", None)?;
        let ini_rendered = render_at_template(&read_template(host, &ini_tpl_path)?, &subs);
        let nsi_rendered = render_at_template(&read_template(host, &nsi_tpl_path)?, &subs);

        let ini_out = cx.output_dir.join("NSIS.InstallOptions.ini");
        let nsi_out = cx.output_dir.join("project.nsi");
        prepare_fresh_output(host, &ini_out)?;
        host.write(&ini_out, ini_rendered.as_bytes(), WriteMode::CreateNew)?;
        prepare_fresh_output(host, &nsi_out)?;
        host.write(&nsi_out, nsi_rendered.as_bytes(), WriteMode::CreateNew)?;

        let out_exe = cx
            .output_dir
            .join(safe_name(&[cx.file_name, b".exe"].concat())?);
        prepare_fresh_output(host, &out_exe)?;

        let mut argv = Vec::new();
        argv.extend(settings.pre_args.iter().cloned());
        argv.push(from_native(&nsi_out));
        argv.extend(settings.post_args.iter().cloned());

        let log_path = cx.output_dir.join("NSISOutput.log");
        let outcome = run_installer_tool(
            host,
            cx.output_dir,
            &settings.makensis,
            &argv,
            build_tool_timeout(host),
        )?;
        write_tool_log(
            host,
            &log_path,
            &settings.makensis,
            &argv,
            &outcome.output,
            &[],
            false,
        )?;
        check_tool_outcome(
            &outcome,
            &settings.makensis,
            "makensis",
            Some(&log_path),
            &[],
        )?;
        verify_fresh_output(host, cx.output_dir, &canon_work, &out_exe, "makensis")?;
        Ok(vec![out_exe])
    }
}

impl Settings {
    fn from_config(cfg: &CpackConfig, host: &dyn Host) -> Result<Self, CpackError> {
        let windows_host = is_windows_host(host);
        let opt_prefix = if windows_host { "/" } else { "-" };
        let makensis = option_set(cfg, b"CPACK_INSTALLER_PROGRAM")
            .ok_or_else(|| error("CPACK_INSTALLER_PROGRAM not set for NSIS"))?
            .into();
        let pre_args =
            format_nsis_cli_args(cfg.list(b"CPACK_NSIS_EXECUTABLE_PRE_ARGUMENTS"), opt_prefix)?;
        let post_args = format_nsis_cli_args(
            cfg.list(b"CPACK_NSIS_EXECUTABLE_POST_ARGUMENTS"),
            opt_prefix,
        )?;
        let executables_dir = require_utf8(
            cfg.get(b"CPACK_NSIS_EXECUTABLES_DIRECTORY")
                .unwrap_or(b"bin"),
            "CPACK_NSIS_EXECUTABLES_DIRECTORY",
        )?
        .replace('/', "\\");
        let compressor = validate_nsis_compressor(require_utf8(
            option_set(cfg, b"CPACK_NSIS_COMPRESSOR").unwrap_or(b"lzma"),
            "CPACK_NSIS_COMPRESSOR",
        )?)?;
        let install_root = require_utf8(
            cfg.get(b"CPACK_NSIS_INSTALL_ROOT")
                .unwrap_or(b"$PROGRAMFILES"),
            "CPACK_NSIS_INSTALL_ROOT",
        )?
        .to_owned();
        let install_dir = require_utf8(
            cfg.get(b"CPACK_PACKAGE_INSTALL_DIRECTORY")
                .unwrap_or_default(),
            "CPACK_PACKAGE_INSTALL_DIRECTORY",
        )?
        .to_owned();
        let registry_key = require_utf8(
            cfg.get(b"CPACK_PACKAGE_INSTALL_REGISTRY_KEY")
                .unwrap_or_default(),
            "CPACK_PACKAGE_INSTALL_REGISTRY_KEY",
        )?
        .to_owned();
        let display_name = require_utf8(
            cfg.get(b"CPACK_NSIS_DISPLAY_NAME").unwrap_or_default(),
            "CPACK_NSIS_DISPLAY_NAME",
        )?
        .to_owned();
        let package_name = require_utf8(
            cfg.get(b"CPACK_NSIS_PACKAGE_NAME").unwrap_or_default(),
            "CPACK_NSIS_PACKAGE_NAME",
        )?
        .to_owned();
        let package_vendor = require_utf8(
            cfg.get(b"CPACK_PACKAGE_VENDOR").unwrap_or(b"Humanity"),
            "CPACK_PACKAGE_VENDOR",
        )?
        .to_owned();
        let package_version = require_utf8(
            cfg.get(b"CPACK_PACKAGE_VERSION").unwrap_or_default(),
            "CPACK_PACKAGE_VERSION",
        )?
        .to_owned();
        let package_version_patch = require_utf8(
            cfg.get(b"CPACK_PACKAGE_VERSION_PATCH").unwrap_or(b"0"),
            "CPACK_PACKAGE_VERSION_PATCH",
        )?
        .to_owned();
        let uninstall_name = require_utf8(
            cfg.get(b"CPACK_NSIS_UNINSTALL_NAME")
                .unwrap_or(b"Uninstall"),
            "CPACK_NSIS_UNINSTALL_NAME",
        )?
        .to_owned();
        Ok(Self {
            makensis,
            opt_prefix,
            windows_host,
            pre_args,
            post_args,
            executables_dir,
            compressor,
            install_root,
            install_dir,
            registry_key,
            display_name,
            package_name,
            package_vendor,
            package_version,
            package_version_patch,
            uninstall_name,
        })
    }
}

fn format_nsis_cli_args(items: Vec<BString>, opt_prefix: &str) -> Result<Vec<BString>, CpackError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = require_utf8(&item, "NSIS executable argument")?;
        if s.starts_with('/') || s.starts_with('-') {
            out.push(item);
        } else {
            out.push(format!("{opt_prefix}{s}").into());
        }
    }
    Ok(out)
}

fn validate_nsis_identifier(id: &str, kind: &str) -> Result<(), CpackError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(error(format!("Invalid NSIS {kind} identifier: \"{id}\"")));
    }
    Ok(())
}

fn validate_nsis_compressor(raw: &str) -> Result<String, CpackError> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(error("CPACK_NSIS_COMPRESSOR cannot be empty"));
    }
    let mut saw_algo = false;
    for tok in &tokens {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "/solid" | "/final" => {}
            "zlib" | "bzip2" | "lzma" => {
                if saw_algo {
                    return Err(error(format!(
                        "Duplicate algorithm in CPACK_NSIS_COMPRESSOR: \"{raw}\""
                    )));
                }
                saw_algo = true;
            }
            _ => {
                return Err(error(format!(
                    "Unsupported CPACK_NSIS_COMPRESSOR token: \"{tok}\""
                )));
            }
        }
    }
    if !saw_algo {
        return Err(error(format!(
            "CPACK_NSIS_COMPRESSOR must specify zlib, bzip2, or lzma: \"{raw}\""
        )));
    }
    Ok(tokens.join(" "))
}

fn validate_nsis_install_root(raw: &str) -> Result<String, CpackError> {
    // Preserve well-known NSIS install root tokens ($PROGRAMFILES, $PROGRAMFILES64, etc.),
    // escaping any remaining literal characters safely.
    let known = [
        "$PROGRAMFILES64",
        "$PROGRAMFILES32",
        "$PROGRAMFILES",
        "$LOCALAPPDATA",
        "$APPDATA",
        "$DOCUMENTS",
        "$PROFILE",
        "$INSTDIR",
        "$WINDIR",
        "$SYSDIR",
        "$TEMP",
    ];
    for token in known {
        if let Some(rest) = raw.strip_prefix(token) {
            let escaped_rest = escape_nsis_literal(&rest.replace('/', "\\"), false)?;
            return Ok(format!("{token}{escaped_rest}"));
        }
    }
    escape_nsis_literal(&raw.replace('/', "\\"), false)
}

fn custom_component_install_dir(cfg: &CpackConfig, component: &str) -> Result<String, CpackError> {
    // Documented CMake convention: CPACK_NSIS_<compName>_INSTALL_DIRECTORY preserves case.
    let key = format!("CPACK_NSIS_{component}_INSTALL_DIRECTORY");
    match option_set(cfg, key.as_bytes()) {
        Some(raw) => {
            let text = require_utf8(raw, &key)?;
            validate_nsis_install_root(text)
        }
        None => Ok("$INSTDIR".to_owned()),
    }
}

fn walk_nsis_tree(
    host: &dyn Host,
    root: &Path,
    expected_canon: &Path,
) -> Result<(Vec<String>, Vec<String>), CpackError> {
    let canon = crate::source::verify_expected_root(host, root, expected_canon)?;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut remaining = 100_000_usize;
    walk_nsis_dir(
        host,
        root,
        &canon,
        Path::new(""),
        0,
        &mut remaining,
        &mut files,
        &mut dirs,
    )?;
    Ok((files, dirs))
}

#[allow(clippy::too_many_arguments)]
fn walk_nsis_dir(
    host: &dyn Host,
    root: &Path,
    canon_root: &Path,
    rel: &Path,
    depth: usize,
    remaining: &mut usize,
    files: &mut Vec<String>,
    dirs: &mut Vec<String>,
) -> Result<(), CpackError> {
    if depth >= 256 {
        return Err(error("NSIS staging directory depth exceeds the limit"));
    }
    let current = if rel.as_os_str().is_empty() {
        root.to_owned()
    } else {
        root.join(rel)
    };
    let (mut entries, truncated) = host.read_dir_bounded(&current, *remaining)?;
    if truncated {
        return Err(error("NSIS staging entry count exceeds the limit"));
    }
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| error("NSIS staging entry count exceeds the limit"))?;
        if entry.name.starts_with(b"._") {
            continue;
        }
        let name = to_native(&entry.name);
        let child_rel = rel.join(&name);
        let child_path = current.join(&name);
        let meta = host.metadata_fresh(&child_path, Follow::No)?;
        if !host
            .canonicalize_fresh(&child_path)?
            .starts_with(canon_root)
        {
            return Err(error(format!(
                "NSIS payload entry escapes staging root: {}",
                child_path.display()
            )));
        }
        let rel_str = from_native_cmake(&child_rel).to_string();
        match meta.kind {
            FileKind::File => files.push(rel_str),
            FileKind::Dir => {
                walk_nsis_dir(
                    host,
                    root,
                    canon_root,
                    &child_rel,
                    depth + 1,
                    remaining,
                    files,
                    dirs,
                )?;
                // Post-order so child directories are removed before parent directories.
                dirs.push(rel_str);
            }
            FileKind::Symlink | FileKind::Other => {
                return Err(error(format!(
                    "Unsupported entry kind in NSIS payload: {}",
                    child_path.display()
                )));
            }
        }
    }
    Ok(())
}

fn nsis_win_output_path(cwd: &Path, raw: &[u8], label: &str) -> Result<String, CpackError> {
    let text = require_utf8(raw, label)?;
    let path = absolute(cwd, text.as_bytes());
    let win = to_windows_slashes(&path);
    let escaped = escape_nsis_literal(&win, false)?;
    Ok(format!("\"{escaped}\""))
}

fn build_base_substitutions(
    cfg: &CpackConfig,
    cx: &PackageContext<'_>,
    cwd: &Path,
    settings: &Settings,
) -> Result<BTreeMap<String, String>, CpackError> {
    let mut subs = BTreeMap::new();
    let _ = settings.opt_prefix;

    subs.insert(
        "CPACK_PACKAGE_VERSION".to_owned(),
        escape_nsis_literal(&settings.package_version, false)?,
    );
    subs.insert(
        "CPACK_PACKAGE_VERSION_PATCH".to_owned(),
        escape_nsis_literal(&settings.package_version_patch, false)?,
    );
    subs.insert(
        "CPACK_TEMPORARY_DIRECTORY".to_owned(),
        escape_nsis_literal(&to_windows_slashes(cx.staging_dir), false)?,
    );
    subs.insert(
        "CPACK_TOPLEVEL_DIRECTORY".to_owned(),
        escape_nsis_literal(&from_native_cmake(cx.output_dir).to_string(), false)?,
    );
    subs.insert(
        "CPACK_OUTPUT_FILE_NAME".to_owned(),
        escape_nsis_literal(
            &format!(
                "{}.exe",
                require_utf8(cx.file_name, "CPACK_PACKAGE_FILE_NAME")?
            ),
            false,
        )?,
    );
    subs.insert(
        "CPACK_NSIS_INSTALL_ROOT".to_owned(),
        validate_nsis_install_root(&settings.install_root)?,
    );
    subs.insert(
        "CPACK_PACKAGE_INSTALL_DIRECTORY".to_owned(),
        escape_nsis_literal(&settings.install_dir.replace('/', "\\"), false)?,
    );
    subs.insert(
        "CPACK_PACKAGE_INSTALL_REGISTRY_KEY".to_owned(),
        escape_nsis_literal(&settings.registry_key.replace('/', "\\"), false)?,
    );
    subs.insert(
        "CPACK_NSIS_DISPLAY_NAME".to_owned(),
        escape_nsis_literal(&settings.display_name, false)?,
    );
    subs.insert(
        "CPACK_NSIS_PACKAGE_NAME".to_owned(),
        escape_nsis_literal(&settings.package_name, false)?,
    );
    subs.insert(
        "CPACK_PACKAGE_NAME".to_owned(),
        escape_nsis_literal(
            require_utf8(
                cfg.get(b"CPACK_PACKAGE_NAME").unwrap_or_default(),
                "CPACK_PACKAGE_NAME",
            )?,
            false,
        )?,
    );
    subs.insert(
        "CPACK_PACKAGE_VENDOR".to_owned(),
        escape_nsis_literal(&settings.package_vendor, false)?,
    );
    subs.insert(
        "CPACK_NSIS_UNINSTALL_NAME".to_owned(),
        escape_nsis_literal(&settings.uninstall_name, false)?,
    );
    subs.insert(
        "CPACK_NSIS_COMPRESSOR".to_owned(),
        settings.compressor.clone(),
    );
    subs.insert(
        "CPACK_NSIS_EXECUTABLES_DIRECTORY".to_owned(),
        escape_nsis_literal(&settings.executables_dir, false)?,
    );
    subs.insert(
        "CPACK_NSIS_MODIFY_PATH".to_owned(),
        if cfg.on(b"CPACK_NSIS_MODIFY_PATH") {
            "ON".to_owned()
        } else {
            "OFF".to_owned()
        },
    );
    subs.insert(
        "CPACK_NSIS_ENABLE_UNINSTALL_BEFORE_INSTALL".to_owned(),
        if cfg.on(b"CPACK_NSIS_ENABLE_UNINSTALL_BEFORE_INSTALL") {
            "ON".to_owned()
        } else {
            "OFF".to_owned()
        },
    );

    for (key, allow_nl) in [
        ("CPACK_NSIS_INSTALLED_ICON_NAME", false),
        ("CPACK_NSIS_HELP_LINK", false),
        ("CPACK_NSIS_URL_INFO_ABOUT", false),
        ("CPACK_NSIS_CONTACT", false),
        ("CPACK_DOWNLOAD_SITE", false),
    ] {
        let val = match option_set(cfg, key.as_bytes()) {
            Some(v) => escape_nsis_literal(require_utf8(v, key)?, allow_nl)?,
            None => String::new(),
        };
        subs.insert(key.to_owned(), val);
    }

    if let Some(raw_override) = option_set(cfg, b"CPACK_NSIS_INSTALLER_MUI_ICON_CODE") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_MUI_ICON_CODE".to_owned(),
            require_utf8(raw_override, "CPACK_NSIS_INSTALLER_MUI_ICON_CODE")?.to_owned(),
        );
    } else {
        let mut code = String::new();
        if let Some(v) = option_set(cfg, b"CPACK_NSIS_MUI_ICON") {
            code.push_str(&format!(
                "!define MUI_ICON {}\n",
                nsis_win_output_path(cwd, v, "CPACK_NSIS_MUI_ICON")?
            ));
        }
        if let Some(v) = option_set(cfg, b"CPACK_NSIS_MUI_UNIICON") {
            code.push_str(&format!(
                "!define MUI_UNICON {}\n",
                nsis_win_output_path(cwd, v, "CPACK_NSIS_MUI_UNIICON")?
            ));
        }
        subs.insert("CPACK_NSIS_INSTALLER_MUI_ICON_CODE".to_owned(), code);
    }

    // Header bitmap (CPACK_NSIS_MUI_HEADERIMAGE overrides CPACK_PACKAGE_ICON)
    if let Some(raw_override) = option_set(cfg, b"CPACK_NSIS_INSTALLER_ICON_CODE") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_ICON_CODE".to_owned(),
            require_utf8(raw_override, "CPACK_NSIS_INSTALLER_ICON_CODE")?.to_owned(),
        );
    } else if let Some(img) = option_set(cfg, b"CPACK_NSIS_MUI_HEADERIMAGE")
        .or_else(|| option_set(cfg, b"CPACK_PACKAGE_ICON"))
    {
        subs.insert(
            "CPACK_NSIS_INSTALLER_ICON_CODE".to_owned(),
            format!(
                "!define MUI_HEADERIMAGE_BITMAP {}\n",
                nsis_win_output_path(cwd, img, "CPACK_NSIS_MUI_HEADERIMAGE")?
            ),
        );
    } else {
        subs.insert("CPACK_NSIS_INSTALLER_ICON_CODE".to_owned(), String::new());
    }

    if let Some(v) = option_set(cfg, b"CPACK_NSIS_MUI_WELCOMEFINISHPAGE_BITMAP") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_MUI_WELCOMEFINISH_CODE".to_owned(),
            format!(
                "!define MUI_WELCOMEFINISHPAGE_BITMAP {}\n",
                nsis_win_output_path(cwd, v, "CPACK_NSIS_MUI_WELCOMEFINISHPAGE_BITMAP")?
            ),
        );
    }
    if let Some(v) = option_set(cfg, b"CPACK_NSIS_MUI_UNWELCOMEFINISHPAGE_BITMAP") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_MUI_UNWELCOMEFINISH_CODE".to_owned(),
            format!(
                "!define MUI_UNWELCOMEFINISHPAGE_BITMAP {}\n",
                nsis_win_output_path(cwd, v, "CPACK_NSIS_MUI_UNWELCOMEFINISHPAGE_BITMAP")?
            ),
        );
    }

    if let Some(v) = option_set(cfg, b"CPACK_NSIS_MUI_FINISHPAGE_RUN") {
        let exe = escape_nsis_literal(
            &require_utf8(v, "CPACK_NSIS_MUI_FINISHPAGE_RUN")?.replace('/', "\\"),
            false,
        )?;
        let exec_dir = escape_nsis_literal(&settings.executables_dir, false)?;
        subs.insert(
            "CPACK_NSIS_INSTALLER_MUI_FINISHPAGE_RUN_CODE".to_owned(),
            format!("!define MUI_FINISHPAGE_RUN \"$INSTDIR\\{exec_dir}\\{exe}\"\n"),
        );
    }

    if let Some(v) = option_set(cfg, b"CPACK_NSIS_WELCOME_TITLE") {
        let escaped = escape_nsis_literal(require_utf8(v, "CPACK_NSIS_WELCOME_TITLE")?, true)?;
        subs.insert(
            "CPACK_NSIS_INSTALLER_WELCOME_TITLE_CODE".to_owned(),
            format!("!define MUI_WELCOMEPAGE_TITLE \"{escaped}\""),
        );
    }
    if cfg.on(b"CPACK_NSIS_WELCOME_TITLE_3LINES") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_WELCOME_TITLE_3LINES_CODE".to_owned(),
            "!define MUI_WELCOMEPAGE_TITLE_3LINES".to_owned(),
        );
    }
    if let Some(v) = option_set(cfg, b"CPACK_NSIS_FINISH_TITLE") {
        let escaped = escape_nsis_literal(require_utf8(v, "CPACK_NSIS_FINISH_TITLE")?, true)?;
        subs.insert(
            "CPACK_NSIS_INSTALLER_FINISH_TITLE_CODE".to_owned(),
            format!("!define MUI_FINISHPAGE_TITLE \"{escaped}\""),
        );
    }
    if cfg.on(b"CPACK_NSIS_FINISH_TITLE_3LINES") {
        subs.insert(
            "CPACK_NSIS_INSTALLER_FINISH_TITLE_3LINES_CODE".to_owned(),
            "!define MUI_FINISHPAGE_TITLE_3LINES".to_owned(),
        );
    }

    if cfg.on(b"CPACK_NSIS_MANIFEST_DPI_AWARE") {
        subs.insert(
            "CPACK_NSIS_MANIFEST_DPI_AWARE_CODE".to_owned(),
            "ManifestDPIAware true".to_owned(),
        );
    }

    if let Some(v) = option_set(cfg, b"CPACK_NSIS_BRANDING_TEXT") {
        let pos = match option_set(cfg, b"CPACK_NSIS_BRANDING_TEXT_TRIM_POSITION") {
            Some(raw) => {
                let p = require_utf8(raw, "CPACK_NSIS_BRANDING_TEXT_TRIM_POSITION")?;
                if !matches!(p, "LEFT" | "CENTER" | "RIGHT") {
                    return Err(error(format!(
                        "Unsupported branding text trim position {p}"
                    )));
                }
                p
            }
            None => "LEFT",
        };
        let text = escape_nsis_literal(require_utf8(v, "CPACK_NSIS_BRANDING_TEXT")?, false)?;
        subs.insert(
            "CPACK_NSIS_BRANDING_TEXT_CODE".to_owned(),
            format!("BrandingText /TRIM{pos} \"{text}\"\n"),
        );
    }

    if let Some(v) = option_set(cfg, b"CPACK_NSIS_CRC_CHECK") {
        let crc = require_utf8(v, "CPACK_NSIS_CRC_CHECK")?;
        let lower = crc.to_ascii_lowercase();
        if !matches!(lower.as_str(), "on" | "off" | "force") {
            return Err(error(format!(
                "Invalid CPACK_NSIS_CRC_CHECK value \"{crc}\": expected on, off, or force"
            )));
        }
        subs.insert(
            "CPACK_NSIS_CRC_CHECK_CODE".to_owned(),
            format!("CRCCheck {crc}"),
        );
    }

    if !cfg.on(b"CPACK_NSIS_IGNORE_LICENSE_PAGE")
        && let Some(license) = option_set(cfg, b"CPACK_RESOURCE_FILE_LICENSE")
    {
        subs.insert(
            "CPACK_NSIS_LICENSE_PAGE".to_owned(),
            format!(
                "!insertmacro MUI_PAGE_LICENSE {}\n",
                nsis_win_output_path(cwd, license, "CPACK_RESOURCE_FILE_LICENSE")?
            ),
        );
    }

    // Documented raw NSIS code injection channels (kept strictly separate from metadata).
    for raw_key in [
        "CPACK_NSIS_EXTRA_PREINSTALL_COMMANDS",
        "CPACK_NSIS_EXTRA_INSTALL_COMMANDS",
        "CPACK_NSIS_EXTRA_UNINSTALL_COMMANDS",
        "CPACK_NSIS_CREATE_ICONS_EXTRA",
        "CPACK_NSIS_DELETE_ICONS_EXTRA",
        "CPACK_NSIS_DEFINES",
    ] {
        if let Some(val) = cfg.get(raw_key.as_bytes()) {
            subs.insert(raw_key.to_owned(), require_utf8(val, raw_key)?.to_owned());
        }
    }

    Ok(subs)
}

fn render_shortcuts(
    cfg: &CpackConfig,
    settings: &Settings,
) -> Result<(String, String), CpackError> {
    let mut create = String::new();
    let mut delete = String::new();
    let desktop_links: BTreeSet<BString> = cfg
        .list(b"CPACK_CREATE_DESKTOP_LINKS")
        .into_iter()
        .collect();
    let exec_dir = escape_nsis_literal(&settings.executables_dir, false)?;

    let executables = cfg.list(b"CPACK_PACKAGE_EXECUTABLES");
    if !executables.len().is_multiple_of(2) {
        return Err(error(
            "CPACK_PACKAGE_EXECUTABLES should contain pairs of <executable> and <icon name>.",
        ));
    }
    for [exe_raw, label_raw] in executables.as_chunks::<2>().0 {
        let exe = escape_nsis_literal(
            &require_utf8(exe_raw, "CPACK_PACKAGE_EXECUTABLES")?.replace('/', "\\"),
            false,
        )?;
        let label =
            escape_nsis_literal(require_utf8(label_raw, "CPACK_PACKAGE_EXECUTABLES")?, false)?;
        create.push_str(&format!(
            "  CreateShortCut \"$SMPROGRAMS\\$STARTMENU_FOLDER\\{label}.lnk\" \"$INSTDIR\\{exec_dir}\\{exe}.exe\"\n"
        ));
        delete.push_str(&format!(
            "  Delete \"$SMPROGRAMS\\$MUI_TEMP\\{label}.lnk\"\n"
        ));
        if desktop_links.contains(exe_raw) {
            create.push_str("  StrCmp \"$INSTALL_DESKTOP\" \"1\" 0 +2\n");
            create.push_str(&format!(
                "    CreateShortCut \"$DESKTOP\\{label}.lnk\" \"$INSTDIR\\{exec_dir}\\{exe}.exe\"\n"
            ));
            delete.push_str("  StrCmp \"$INSTALL_DESKTOP\" \"1\" 0 +2\n");
            delete.push_str(&format!("    Delete \"$DESKTOP\\{label}.lnk\"\n"));
        }
    }

    let menu_links = cfg.list(b"CPACK_NSIS_MENU_LINKS");
    if !menu_links.len().is_multiple_of(2) {
        return Err(error(
            "CPACK_NSIS_MENU_LINKS should contain pairs of <shortcut target> and <shortcut label>.",
        ));
    }
    for [src_raw, label_raw] in menu_links.as_chunks::<2>().0 {
        let src_str = require_utf8(src_raw, "CPACK_NSIS_MENU_LINKS")?;
        let label_str = require_utf8(label_raw, "CPACK_NSIS_MENU_LINKS")?;
        let is_url = is_menu_link_url(src_str);
        let label = escape_nsis_literal(label_str, false)?;
        if is_url {
            let url = escape_nsis_literal(src_str, false)?;
            create.push_str(&format!(
                "  WriteINIStr \"$SMPROGRAMS\\$STARTMENU_FOLDER\\{label}.url\" \"InternetShortcut\" \"URL\" \"{url}\"\n"
            ));
            delete.push_str(&format!(
                "  Delete \"$SMPROGRAMS\\$MUI_TEMP\\{label}.url\"\n"
            ));
        } else {
            let target = escape_nsis_literal(&src_str.replace('/', "\\"), false)?;
            create.push_str(&format!(
                "  CreateShortCut \"$SMPROGRAMS\\$STARTMENU_FOLDER\\{label}.lnk\" \"$INSTDIR\\{target}\"\n"
            ));
            delete.push_str(&format!(
                "  Delete \"$SMPROGRAMS\\$MUI_TEMP\\{label}.lnk\"\n"
            ));
        }
        let desktop_var = format!("CPACK_CREATE_DESKTOP_LINK_{label_str}");
        if cfg.on(desktop_var.as_bytes()) {
            let target = escape_nsis_literal(&src_str.replace('/', "\\"), false)?;
            create.push_str("  StrCmp \"$INSTALL_DESKTOP\" \"1\" 0 +2\n");
            create.push_str(&format!(
                "    CreateShortCut \"$DESKTOP\\{label}.lnk\" \"$INSTDIR\\{target}\"\n"
            ));
            delete.push_str("  StrCmp \"$INSTALL_DESKTOP\" \"1\" 0 +2\n");
            delete.push_str(&format!("    Delete \"$DESKTOP\\{label}.lnk\"\n"));
        }
    }

    Ok((create, delete))
}

fn is_menu_link_url(target: &str) -> bool {
    target.starts_with("mailto:")
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("ftp://")
        || target.starts_with("ftps://")
        || target.starts_with("news://")
}

#[allow(clippy::too_many_arguments)]
fn render_components(
    host: &dyn Host,
    cfg: &CpackConfig,
    cx: &PackageContext<'_>,
    cwd: &Path,
    settings: &Settings,
    catalog: &ComponentCatalog,
    comp_files: &BTreeMap<BString, Vec<String>>,
    comp_dirs: &BTreeMap<BString, Vec<String>>,
    subs: &mut BTreeMap<String, String>,
) -> Result<(), CpackError> {
    let mut install_types_sorted: Vec<_> = catalog.install_types.values().collect();
    install_types_sorted.sort_by_key(|t| t.index);
    let mut install_types_code = String::new();
    for ty in install_types_sorted {
        let display = escape_nsis_literal(
            require_utf8(&ty.display_name, "CPACK_INSTALL_TYPE_DISPLAY_NAME")?,
            false,
        )?;
        install_types_code.push_str(&format!("InstType \"{display}\"\n"));
    }

    let mut component_code = String::new();
    let mut macros_out = String::new();
    let mut section_list = String::new();
    let mut selected_vars = String::new();
    let mut comp_descs = String::new();
    let mut group_descs = String::new();
    let mut any_downloaded = false;

    for group in catalog.groups.values() {
        if group.parent_group.is_none() {
            component_code.push_str(&render_group_section(
                host,
                cfg,
                cx,
                cwd,
                settings,
                catalog,
                group,
                comp_files,
                comp_dirs,
                &mut macros_out,
            )?);
        }
        if !group.description.is_empty() {
            let gname = require_utf8(&group.name, "NSIS group name")?;
            let desc = escape_nsis_literal(
                require_utf8(&group.description, "NSIS group description")?,
                true,
            )?;
            group_descs.push_str(&format!(
                "  !insertmacro MUI_DESCRIPTION_TEXT ${{{gname}}} \"{desc}\"\n"
            ));
        }
    }

    for (comp_name, comp) in catalog.components.iter() {
        let files = comp_files.get(comp_name).map_or(&[][..], Vec::as_slice);
        if files.is_empty() {
            continue;
        }
        any_downloaded |= comp.downloaded;
        if comp.group.is_none() {
            component_code.push_str(&render_component_section(
                host,
                cfg,
                cx,
                cwd,
                settings,
                catalog,
                comp,
                files,
                comp_dirs.get(comp_name).map_or(&[][..], Vec::as_slice),
                &mut macros_out,
            )?);
        }
        let cname = require_utf8(comp_name, "NSIS component name")?;
        section_list.push_str(&format!("  !insertmacro \"${{MacroName}}\" \"{cname}\"\n"));
        selected_vars.push_str(&format!(
            "Var {cname}_selected\nVar {cname}_was_installed\n"
        ));
        if !comp.description.is_empty() {
            let desc = escape_nsis_literal(
                require_utf8(&comp.description, "NSIS component description")?,
                true,
            )?;
            comp_descs.push_str(&format!(
                "  !insertmacro MUI_DESCRIPTION_TEXT ${{{cname}}} \"{desc}\"\n"
            ));
        }
    }

    component_code.push_str(&macros_out);

    let desc_code = if comp_descs.is_empty() && group_descs.is_empty() {
        "!define MUI_COMPONENTSPAGE_NODESC".to_owned()
    } else {
        format!(
            "!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN\n{comp_descs}{group_descs}!insertmacro MUI_FUNCTION_DESCRIPTION_END\n"
        )
    };
    subs.entry("CPACK_NSIS_INSTALLER_MUI_COMPONENTS_DESC".to_owned())
        .or_insert(desc_code);

    let mut defines = subs.get("CPACK_NSIS_DEFINES").cloned().unwrap_or_default();
    if any_downloaded {
        defines.push_str("!define CPACK_USES_DOWNLOAD\n");
        if cfg.on(b"CPACK_ADD_REMOVE") {
            defines.push_str("!define CPACK_NSIS_ADD_REMOVE\n");
        }
    }
    subs.insert("CPACK_NSIS_DEFINES".to_owned(), defines);
    subs.entry("CPACK_NSIS_INSTALLATION_TYPES".to_owned())
        .or_insert(install_types_code);
    if !cfg.on(b"CPACK_NSIS_IGNORE_COMPONENTS_PAGE") {
        subs.entry("CPACK_NSIS_PAGE_COMPONENTS".to_owned())
            .or_insert_with(|| "!insertmacro MUI_PAGE_COMPONENTS".to_owned());
    }
    subs.entry("CPACK_NSIS_FULL_INSTALL".to_owned())
        .or_default();
    subs.entry("CPACK_NSIS_COMPONENT_SECTIONS".to_owned())
        .or_insert(component_code);
    subs.entry("CPACK_NSIS_COMPONENT_SECTION_LIST".to_owned())
        .or_insert(section_list);
    subs.entry("CPACK_NSIS_SECTION_SELECTED_VARS".to_owned())
        .or_insert(selected_vars);
    Ok(())
}

enum GroupSectionStep<'a> {
    Enter(&'a GroupMeta),
    EmitComponentsAndClose(&'a GroupMeta),
}

#[allow(clippy::too_many_arguments)]
fn render_group_section(
    host: &dyn Host,
    cfg: &CpackConfig,
    cx: &PackageContext<'_>,
    cwd: &Path,
    settings: &Settings,
    catalog: &ComponentCatalog,
    root_group: &GroupMeta,
    comp_files: &BTreeMap<BString, Vec<String>>,
    comp_dirs: &BTreeMap<BString, Vec<String>>,
    macros_out: &mut String,
) -> Result<String, CpackError> {
    let mut out = String::new();
    let mut stack = vec![GroupSectionStep::Enter(root_group)];
    while let Some(step) = stack.pop() {
        match step {
            GroupSectionStep::Enter(group) => {
                if group.components.is_empty() && group.subgroups.is_empty() {
                    continue;
                }
                let gname = require_utf8(&group.name, "NSIS group name")?;
                let display = escape_nsis_literal(
                    require_utf8(&group.display_name, "NSIS group display name")?,
                    false,
                )?;
                let expanded = if group.expanded { "/e " } else { "" };
                let bold = if group.bold_title { "!" } else { "" };
                out.push_str(&format!(
                    "SectionGroup {expanded}\"{bold}{display}\" {gname}\n"
                ));
                stack.push(GroupSectionStep::EmitComponentsAndClose(group));
                for sub_name in group.subgroups.iter().rev() {
                    if let Some(sub) = catalog.groups.get(sub_name) {
                        stack.push(GroupSectionStep::Enter(sub));
                    }
                }
            }
            GroupSectionStep::EmitComponentsAndClose(group) => {
                for comp_name in &group.components {
                    let files = comp_files.get(comp_name).map_or(&[][..], Vec::as_slice);
                    if files.is_empty() {
                        continue;
                    }
                    if let Some(comp) = catalog.components.get(comp_name) {
                        out.push_str(&render_component_section(
                            host,
                            cfg,
                            cx,
                            cwd,
                            settings,
                            catalog,
                            comp,
                            files,
                            comp_dirs.get(comp_name).map_or(&[][..], Vec::as_slice),
                            macros_out,
                        )?);
                    }
                }
                out.push_str("SectionGroupEnd\n");
            }
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn render_component_section(
    host: &dyn Host,
    cfg: &CpackConfig,
    cx: &PackageContext<'_>,
    cwd: &Path,
    settings: &Settings,
    catalog: &ComponentCatalog,
    comp: &ComponentMeta,
    files: &[String],
    dirs: &[String],
    macros_out: &mut String,
) -> Result<String, CpackError> {
    let cname = require_utf8(&comp.name, "NSIS component name")?;
    let display = escape_nsis_literal(
        require_utf8(&comp.display_name, "NSIS component display name")?,
        false,
    )?;
    let opt = if comp.disabled { "/o " } else { "" };
    let hidden = if comp.hidden { "-" } else { "" };
    let mut code = format!("Section {opt}\"{hidden}{display}\" {cname}\n");
    if comp.required {
        code.push_str("  SectionIn RO\n");
    } else if !comp.install_types.is_empty() {
        let mut indices = Vec::new();
        for ty in &comp.install_types {
            if let Some(meta) = catalog.install_types.get(ty) {
                indices.push(meta.index.to_string());
            }
        }
        if !indices.is_empty() {
            code.push_str(&format!("  SectionIn {}\n", indices.join(" ")));
        }
    }
    let out_dir = custom_component_install_dir(cfg, cname)?;
    code.push_str(&format!("  SetOutPath \"{out_dir}\"\n"));

    let sanitized_dir = crate::stage::component_dir_for_host(&comp.name, settings.windows_host);
    if comp.downloaded {
        let archive_name = match &comp.archive_file {
            Some(f) => require_utf8(f, "CPACK_COMPONENT_ARCHIVE_FILE")?.to_owned(),
            None => format!(
                "{}-{cname}.zip",
                require_utf8(cx.file_name, "CPACK_PACKAGE_FILE_NAME")?
            ),
        };
        let safe_archive = safe_name(archive_name.as_bytes())?;
        let upload_dir = option_set(cfg, b"CPACK_UPLOAD_DIRECTORY").map_or_else(
            || {
                let pkg_dir = cfg
                    .get(b"CPACK_PACKAGE_DIRECTORY")
                    .map_or_else(|| cwd.to_owned(), |d| absolute(cwd, d));
                pkg_dir.join("CPackUploads")
            },
            |d| absolute(cwd, d),
        );
        host.create_dir_all(&upload_dir)?;
        let archive_path = upload_dir.join(&safe_archive);
        log(
            host,
            format!(
                "-   Building downloaded component archive: {}",
                archive_path.display()
            ),
        );
        prepare_fresh_output(host, &archive_path)?;
        let comp_root = cx.staging_dir.join(&sanitized_dir);
        let inputs: Vec<PathBuf> = files.iter().map(|f| to_native(f.as_bytes())).collect();
        create_archive(
            host,
            &archive_path,
            &comp_root,
            &inputs,
            &CreateOptions {
                format: ArchiveFormat::Zip,
                mtime: None,
                compression_level: None,
                verbose: false,
            },
        )
        .map_err(|err| error(err.to_string()))?;
        let total_bytes = super::installer::checked_tree_size_bytes(host, &comp_root)?;
        let kb = (total_bytes.saturating_add(512) / 1024).max(1);
        let escaped_archive = escape_nsis_literal(&archive_name, false)?;
        code.push_str(&format!(
            "  AddSize {kb}\n  Push \"{escaped_archive}\"\n  Call DownloadFile\n  ZipDLL::extractall \"$INSTDIR\\{escaped_archive}\" \"$INSTDIR\"\n  Pop $2 ; error message\n  StrCmp $2 \"success\" +2 0\n  MessageBox MB_OK \"Failed to unzip $2\"\n  Delete $INSTDIR\\$0\n"
        ));
    } else {
        let dir_str = escape_nsis_literal(&from_native(&sanitized_dir).to_string(), false)?;
        code.push_str(&format!("  File /r \"${{INST_DIR}}\\{dir_str}\\*.*\"\n"));
    }
    code.push_str("SectionEnd\n");

    // Uninstall macro (files first, then post-order subdirectories).
    macros_out.push_str(&format!("!macro Remove_${{{cname}}}\n"));
    macros_out.push_str(&format!(
        "  IntCmp ${cname}_was_installed 0 noremove_{cname}\n"
    ));
    for f in files {
        let win = escape_nsis_literal(&f.replace('/', "\\"), false)?;
        macros_out.push_str(&format!("  Delete \"{out_dir}\\{win}\"\n"));
    }
    for d in dirs {
        let win = escape_nsis_literal(&d.replace('/', "\\"), false)?;
        macros_out.push_str(&format!("  RMDir \"{out_dir}\\{win}\"\n"));
    }
    macros_out.push_str(&format!("  noremove_{cname}:\n!macroend\n"));

    let mut visited = BTreeSet::new();
    macros_out.push_str(&format!("!macro Select_{cname}_depends\n"));
    append_select_deps(catalog, comp, &mut visited, macros_out)?;
    macros_out.push_str("!macroend\n");

    visited.clear();
    macros_out.push_str(&format!("!macro Deselect_required_by_{cname}\n"));
    append_deselect_rdeps(catalog, comp, &mut visited, macros_out)?;
    macros_out.push_str("!macroend\n");

    Ok(code)
}

pub(super) fn append_select_deps(
    catalog: &ComponentCatalog,
    root_comp: &ComponentMeta,
    visited: &mut BTreeSet<BString>,
    out: &mut String,
) -> Result<(), CpackError> {
    if !visited.insert(root_comp.name.clone()) {
        return Ok(());
    }
    let mut stack = vec![(root_comp, 0_usize)];
    while let Some((comp, next_idx)) = stack.last_mut() {
        if let Some(dep_name) = comp.depends.get(*next_idx) {
            *next_idx += 1;
            let d = require_utf8(dep_name, "NSIS dependency")?;
            out.push_str(&format!(
                "  SectionGetFlags ${{{d}}} $0\n  IntOp $0 $0 | ${{SF_SELECTED}}\n  SectionSetFlags ${{{d}}} $0\n  IntOp ${d}_selected 0 + ${{SF_SELECTED}}\n"
            ));
            if visited.insert(dep_name.clone())
                && let Some(dep) = catalog.components.get(dep_name)
            {
                stack.push((dep, 0));
            }
        } else {
            stack.pop();
        }
    }
    Ok(())
}

fn append_deselect_rdeps(
    catalog: &ComponentCatalog,
    root_comp: &ComponentMeta,
    visited: &mut BTreeSet<BString>,
    out: &mut String,
) -> Result<(), CpackError> {
    if !visited.insert(root_comp.name.clone()) {
        return Ok(());
    }
    let mut stack = vec![(root_comp, 0_usize)];
    while let Some((comp, next_idx)) = stack.last_mut() {
        if let Some(rdep_name) = comp.reverse_depends.get(*next_idx) {
            *next_idx += 1;
            let d = require_utf8(rdep_name, "NSIS reverse dependency")?;
            out.push_str(&format!(
                "  SectionGetFlags ${{{d}}} $0\n  IntOp $1 ${{SF_SELECTED}} ~\n  IntOp $0 $0 & $1\n  SectionSetFlags ${{{d}}} $0\n  IntOp ${d}_selected 0 + 0\n"
            ));
            if visited.insert(rdep_name.clone())
                && let Some(rdep) = catalog.components.get(rdep_name)
            {
                stack.push((rdep, 0));
            }
        } else {
            stack.pop();
        }
    }
    Ok(())
}
