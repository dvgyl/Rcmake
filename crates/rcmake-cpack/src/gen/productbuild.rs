//! macOS `productbuild` package generator (`.pkg` via `pkgbuild` + `productbuild`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, truth};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, WriteMode};

use super::ScratchDir;
use super::installer::{
    ComponentCatalog, ComponentMeta, GroupMeta, XmlWriter, build_tool_timeout, check_tool_outcome,
    checked_tree_size_bytes, copy_component_payload, copy_monolithic_payload, encode_url_path,
    escape_js_single_quoted, escape_xml_attr, escape_xml_text, find_program_on_host, find_template,
    option_set, prepare_fresh_output, read_template, render_at_template, require_utf8,
    run_installer_tool, verify_fresh_output, write_tool_log,
};
use crate::config::{CpackConfig, absolute, snapshot};
use crate::source::{SymlinkPolicy, copy_checked_tree, validate_staged_tree};
use crate::{
    ComponentMembers, CpackError, PackageContext, PackageGenerator, error, log, safe_name,
};

/// The macOS `productbuild` generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProductBuild;

/// Validated settings for one `productbuild` invocation.
struct Settings {
    pkgbuild: BString,
    productbuild: BString,
    package_name: String,
    vendor: String,
    version: String,
    identifier_override: Option<String>,
    pkgbuild_identity: Option<BString>,
    pkgbuild_keychain: Option<PathBuf>,
    productbuild_identity: Option<BString>,
    productbuild_keychain: Option<PathBuf>,
    domains_enabled: bool,
    domains_anywhere: bool,
    domains_user: bool,
    domains_root: bool,
    resources_dir: Option<PathBuf>,
    download_site: Option<String>,
}

/// One component (or monolithic) leaf `.pkg` built by `pkgbuild`.
struct ComponentPkg {
    component_name: Option<BString>,
    identifier: String,
    leaf_file_name: String,
    leaf_path: PathBuf,
    payload_dir: PathBuf,
    scripts_dir: PathBuf,
    plist: Option<PathBuf>,
    install_kbytes: u64,
}

impl PackageGenerator for ProductBuild {
    fn name(&self) -> &'static str {
        "productbuild"
    }

    fn extension(&self) -> &'static str {
        ".pkg"
    }

    fn prepare_config(
        &self,
        host: &dyn Host,
        interp: &mut Interp,
        cwd: &Path,
    ) -> Result<(), CpackError> {
        {
            let mut cx = interp.cx();
            if !cx.var_defined(b"CPACK_PACKAGING_INSTALL_PREFIX") {
                cx.set_var(b"CPACK_PACKAGING_INSTALL_PREFIX", "/Applications".into());
            }
        }
        let cfg = snapshot(interp);
        resolve_tool(
            host,
            &cfg,
            cwd,
            interp,
            b"pkgbuild",
            b"CPACK_COMMAND_PKGBUILD",
            "Cannot find pkgbuild executable",
        )?;
        let cfg = snapshot(interp);
        resolve_tool(
            host,
            &cfg,
            cwd,
            interp,
            b"productbuild",
            b"CPACK_COMMAND_PRODUCTBUILD",
            "Cannot find productbuild executable",
        )?;
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

fn resolve_tool(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    interp: &mut Interp,
    default_name: &[u8],
    var_name: &[u8],
    missing_message: &str,
) -> Result<(), CpackError> {
    let discovered = find_program_on_host(host, cwd, default_name, &[], &[]);
    if let Some(override_val) = option_set(cfg, var_name) {
        let resolved = find_program_on_host(host, cwd, override_val, &[], &[])
            .unwrap_or_else(|| absolute(cwd, override_val));
        interp.cx().set_var(var_name, from_native(&resolved));
        return Ok(());
    }
    let Some(found) = discovered else {
        return Err(error(missing_message));
    };
    interp.cx().set_var(var_name, from_native(&found));
    Ok(())
}

impl ProductBuild {
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
        let settings = Settings::from_config(&cfg, &cwd)?;
        let canon_work = cx.expected_work_root()?;
        let canon_staging = cx.expected_staging_root()?;

        validate_staged_tree(
            host,
            cx.staging_dir,
            &canon_staging,
            members,
            SymlinkPolicy::PreserveContained,
            false,
        )?;

        let catalog = ComponentCatalog::from_config(&cfg, members)?;
        let scratch = ScratchDir::for_generator(host, cx.output_dir, "productbuild")?;
        let payload_base = scratch.path.join("payload");
        host.create_dir_all(&payload_base)?;

        let mut leaf_plans = Vec::new();
        let mut used_leaf_names = BTreeSet::new();
        let mut used_script_dirs = BTreeSet::new();
        let contents_dir = cx.staging_dir.join("Contents");
        let packages_dir = contents_dir.join("Packages");

        if catalog.components.is_empty() {
            let mono_payload = payload_base.join("monolithic");
            copy_monolithic_payload(
                host,
                cx.staging_dir,
                &canon_staging,
                &mono_payload,
                SymlinkPolicy::PreserveContained,
                false,
            )?;
            let size_bytes = checked_tree_size_bytes(host, &mono_payload)?;
            let leaf_name = settings.package_name.clone();
            let leaf_safe = safe_name(leaf_name.as_bytes())?;
            let leaf_path = packages_dir.join(&leaf_safe);
            let scripts_dir = packages_dir.join("scripts");
            leaf_plans.push(ComponentPkg {
                component_name: None,
                identifier: settings.derive_identifier(None),
                leaf_file_name: leaf_name,
                leaf_path,
                payload_dir: mono_payload,
                scripts_dir,
                plist: None,
                install_kbytes: bytes_to_kbytes(size_bytes),
            });
        } else {
            let file_stem = require_utf8(cx.file_name, "CPACK_PACKAGE_FILE_NAME")?;
            for (comp_name, comp_meta) in &catalog.components {
                let comp_str = require_utf8(comp_name, "CPack component name")?;
                let safe_comp_dir = crate::stage::component_dir_for_host(comp_name, false);
                let comp_payload = payload_base.join(&safe_comp_dir);
                if let Some(member) = members.iter().find(|m| &m.name == comp_name) {
                    copy_component_payload(
                        host,
                        cx.staging_dir,
                        &canon_staging,
                        member,
                        &comp_payload,
                        SymlinkPolicy::PreserveContained,
                        false,
                        true,
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
                        &comp_payload,
                        SymlinkPolicy::PreserveContained,
                        false,
                        true,
                    )?;
                }
                let size_bytes = checked_tree_size_bytes(host, &comp_payload)?;
                let leaf_name = leaf_pkg_file_name(file_stem, comp_meta)?;
                let leaf_safe = safe_name(leaf_name.as_bytes())?;
                if !used_leaf_names.insert(leaf_safe.clone()) {
                    return Err(error(format!(
                        "Duplicate productbuild component package name: {leaf_name}"
                    )));
                }
                if !used_script_dirs.insert(safe_comp_dir.clone()) {
                    return Err(error(format!(
                        "Colliding productbuild script directory for component: {comp_str}"
                    )));
                }
                let plist = match &comp_meta.plist {
                    Some(raw) => {
                        let path = absolute(&cwd, raw);
                        if host.metadata_fresh(&path, Follow::Yes)?.kind != FileKind::File {
                            return Err(error(format!(
                                "Expected regular component plist file at {}",
                                path.display()
                            )));
                        }
                        Some(path)
                    }
                    None => None,
                };
                leaf_plans.push(ComponentPkg {
                    component_name: Some(comp_name.clone()),
                    identifier: settings.derive_identifier(Some(comp_str)),
                    leaf_file_name: leaf_name,
                    leaf_path: packages_dir.join(&leaf_safe),
                    payload_dir: comp_payload,
                    scripts_dir: packages_dir.join(&safe_comp_dir).join("scripts"),
                    plist,
                    install_kbytes: bytes_to_kbytes(size_bytes),
                });
            }
        }

        crate::stage::ensure_dir(host, cx.staging_dir, &packages_dir)?;
        let canon_packages = crate::source::canonical_checked_root(host, &packages_dir)?;
        if !canon_packages.starts_with(&canon_staging) {
            return Err(error(format!(
                "CPack productbuild packages directory escapes staging directory: {}",
                packages_dir.display()
            )));
        }

        let log_path = cx.output_dir.join("ProductBuildOutput.log");
        let mut comp_pkg_info: BTreeMap<BString, (String, u64)> = BTreeMap::new();
        for leaf in &leaf_plans {
            log(
                host,
                format!(
                    "-   Building component package: {}",
                    leaf.leaf_path.display()
                ),
            );
            stage_component_scripts(
                host,
                &cfg,
                &cwd,
                &packages_dir,
                &leaf.scripts_dir,
                leaf.component_name.as_deref().map(Vec::as_slice),
            )?;
            build_component_pkg(
                host,
                cx.output_dir,
                &settings,
                leaf,
                &packages_dir,
                &canon_packages,
                &log_path,
            )?;
            if let Some(ref name) = leaf.component_name {
                comp_pkg_info.insert(
                    name.clone(),
                    (leaf.leaf_file_name.clone(), leaf.install_kbytes),
                );
            }
        }

        stage_resources(host, &settings, &contents_dir)?;
        let welcome_nopath = copy_resource_file(host, &cfg, &cwd, "Welcome", &contents_dir)?;
        let readme_nopath = copy_resource_file(host, &cfg, &cwd, "ReadMe", &contents_dir)?;
        let license_nopath = copy_resource_file(host, &cfg, &cwd, "License", &contents_dir)?;

        let dist_path = contents_dir.join("distribution.dist");
        write_distribution_file(
            host,
            &cfg,
            &cwd,
            &settings,
            &catalog,
            &comp_pkg_info,
            &contents_dir,
            &dist_path,
            &welcome_nopath,
            &readme_nopath,
            &license_nopath,
        )?;

        let out_name = safe_name(&[cx.file_name, b".pkg"].concat())?;
        let final_pkg = cx.output_dir.join(out_name);
        prepare_fresh_output(host, &final_pkg)?;

        let mut args = vec![
            "--distribution".into(),
            from_native(&dist_path),
            "--package-path".into(),
            from_native(&packages_dir),
            "--resources".into(),
            from_native(&contents_dir),
            "--version".into(),
            settings.version.clone().into(),
        ];
        if let Some(ref id) = settings.identifier_override {
            args.extend(["--identifier".into(), id.clone().into()]);
        }
        if let Some(ref identity) = settings.productbuild_identity {
            args.extend(["--sign".into(), identity.clone()]);
        }
        if let Some(ref keychain) = settings.productbuild_keychain {
            args.extend(["--keychain".into(), from_native(keychain)]);
        }
        args.push(from_native(&final_pkg));

        let outcome = run_installer_tool(
            host,
            cx.output_dir,
            &settings.productbuild,
            &args,
            build_tool_timeout(host),
        )?;
        if !matches!(outcome.status, rcmake_host::ProcessResult::Exited(0)) {
            write_tool_log(
                host,
                &log_path,
                &settings.productbuild,
                &args,
                &outcome.output,
                &[],
                false,
            )?;
        }
        check_tool_outcome(
            &outcome,
            &settings.productbuild,
            "productbuild",
            Some(&log_path),
            &[],
        )?;
        verify_fresh_output(host, cx.output_dir, &canon_work, &final_pkg, "productbuild")?;
        Ok(vec![final_pkg])
    }
}

impl Settings {
    fn from_config(cfg: &CpackConfig, cwd: &Path) -> Result<Self, CpackError> {
        let pkgbuild = option_set(cfg, b"CPACK_COMMAND_PKGBUILD")
            .ok_or_else(|| error("Cannot find pkgbuild executable"))?
            .into();
        let productbuild = option_set(cfg, b"CPACK_COMMAND_PRODUCTBUILD")
            .ok_or_else(|| error("Cannot find productbuild executable"))?
            .into();
        let package_name = require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_NAME")
                .ok_or_else(|| error("CPACK_PACKAGE_NAME not specified"))?,
            "CPACK_PACKAGE_NAME",
        )?
        .to_owned();
        let vendor = require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_VENDOR").unwrap_or(b"Humanity"),
            "CPACK_PACKAGE_VENDOR",
        )?
        .to_owned();
        let version = require_utf8(
            option_set(cfg, b"CPACK_PACKAGE_VERSION")
                .ok_or_else(|| error("CPACK_PACKAGE_VERSION not specified"))?,
            "CPACK_PACKAGE_VERSION",
        )?
        .to_owned();
        let identifier_override = option_set(cfg, b"CPACK_PRODUCTBUILD_IDENTIFIER")
            .map(|v| require_utf8(v, "CPACK_PRODUCTBUILD_IDENTIFIER").map(str::to_owned))
            .transpose()?;
        let pkgbuild_identity = option_set(cfg, b"CPACK_PKGBUILD_IDENTITY_NAME").map(BString::from);
        let pkgbuild_keychain =
            option_set(cfg, b"CPACK_PKGBUILD_KEYCHAIN_PATH").map(|v| absolute(cwd, v));
        let productbuild_identity =
            option_set(cfg, b"CPACK_PRODUCTBUILD_IDENTITY_NAME").map(BString::from);
        let productbuild_keychain =
            option_set(cfg, b"CPACK_PRODUCTBUILD_KEYCHAIN_PATH").map(|v| absolute(cwd, v));
        let domains_enabled = cfg.on(b"CPACK_PRODUCTBUILD_DOMAINS");
        let domains_anywhere = !cfg
            .get(b"CPACK_PRODUCTBUILD_DOMAINS_ANYWHERE")
            .is_some_and(truth::is_off);
        let domains_user = cfg.on(b"CPACK_PRODUCTBUILD_DOMAINS_USER");
        let domains_root = !cfg
            .get(b"CPACK_PRODUCTBUILD_DOMAINS_ROOT")
            .is_some_and(truth::is_off);
        let resources_dir =
            option_set(cfg, b"CPACK_PRODUCTBUILD_RESOURCES_DIR").map(|v| absolute(cwd, v));
        let download_site = option_set(cfg, b"CPACK_DOWNLOAD_SITE")
            .map(|v| require_utf8(v, "CPACK_DOWNLOAD_SITE").map(str::to_owned))
            .transpose()?;
        Ok(Self {
            pkgbuild,
            productbuild,
            package_name,
            vendor,
            version,
            identifier_override,
            pkgbuild_identity,
            pkgbuild_keychain,
            productbuild_identity,
            productbuild_keychain,
            domains_enabled,
            domains_anywhere,
            domains_user,
            domains_root,
            resources_dir,
            download_site,
        })
    }

    fn derive_identifier(&self, component: Option<&str>) -> String {
        let base = match &self.identifier_override {
            Some(id) => id.clone(),
            None => format!("com.{}.{}", self.vendor, self.package_name),
        };
        match component {
            Some(comp) => format!("{base}.{comp}"),
            None => base,
        }
    }
}

fn leaf_pkg_file_name(file_stem: &str, comp: &ComponentMeta) -> Result<String, CpackError> {
    if let Some(ref archive_file) = comp.archive_file {
        let stem = require_utf8(archive_file, "CPACK_COMPONENT_ARCHIVE_FILE")?;
        Ok(format!("{stem}.pkg"))
    } else {
        let comp_name = require_utf8(&comp.name, "CPack component name")?;
        Ok(format!("{file_stem}-{comp_name}.pkg"))
    }
}

fn bytes_to_kbytes(bytes: u64) -> u64 {
    let kb = bytes.saturating_add(512) / 1024;
    kb.max(1)
}

fn stage_component_scripts(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    packages_dir: &Path,
    scripts_dir: &Path,
    component: Option<&[u8]>,
) -> Result<(), CpackError> {
    crate::stage::ensure_dir(host, packages_dir, scripts_dir)?;
    for (kind, dest_name) in [("PREFLIGHT", "preinstall"), ("POSTFLIGHT", "postinstall")] {
        let script_val = match component {
            Some(comp) => {
                let key = format!(
                    "CPACK_{kind}_{}_SCRIPT",
                    BString::from(comp.to_ascii_uppercase())
                );
                option_set(cfg, key.as_bytes())
            }
            None => {
                let key = format!("CPACK_{kind}_SCRIPT");
                option_set(cfg, key.as_bytes())
            }
        };
        if let Some(raw_path) = script_val {
            let src = absolute(cwd, raw_path);
            let meta = host.metadata_fresh(&src, Follow::Yes)?;
            if meta.kind != FileKind::File {
                return Err(error(format!(
                    "Expected regular {kind} script file at {}",
                    src.display()
                )));
            }
            let dst = scripts_dir.join(dest_name);
            prepare_fresh_output(host, &dst)?;
            host.copy_file(
                &src,
                &dst,
                CopyOptions {
                    preserve_permissions: false,
                    preserve_mtime: true,
                },
            )?;
            host.set_permissions(&dst, 0o755)?;
        }
    }
    Ok(())
}

fn build_component_pkg(
    host: &dyn Host,
    output_dir: &Path,
    settings: &Settings,
    leaf: &ComponentPkg,
    packages_dir: &Path,
    canon_packages: &Path,
    log_path: &Path,
) -> Result<(), CpackError> {
    prepare_fresh_output(host, &leaf.leaf_path)?;
    let mut args = vec![
        "--root".into(),
        from_native(&leaf.payload_dir),
        "--identifier".into(),
        leaf.identifier.clone().into(),
        "--scripts".into(),
        from_native(&leaf.scripts_dir),
        "--version".into(),
        settings.version.clone().into(),
        "--install-location".into(),
        "/".into(),
    ];
    if let Some(ref identity) = settings.pkgbuild_identity {
        args.extend(["--sign".into(), identity.clone()]);
    }
    if let Some(ref keychain) = settings.pkgbuild_keychain {
        args.extend(["--keychain".into(), from_native(keychain)]);
    }
    args.push(from_native(&leaf.leaf_path));
    if let Some(ref plist) = leaf.plist {
        args.extend(["--component-plist".into(), from_native(plist)]);
    }

    let outcome = run_installer_tool(
        host,
        output_dir,
        &settings.pkgbuild,
        &args,
        build_tool_timeout(host),
    )?;
    if !matches!(outcome.status, rcmake_host::ProcessResult::Exited(0)) {
        write_tool_log(
            host,
            log_path,
            &settings.pkgbuild,
            &args,
            &outcome.output,
            &[],
            false,
        )?;
    }
    check_tool_outcome(
        &outcome,
        &settings.pkgbuild,
        "pkgbuild",
        Some(log_path),
        &[],
    )?;
    verify_fresh_output(
        host,
        packages_dir,
        canon_packages,
        &leaf.leaf_path,
        "pkgbuild",
    )?;
    Ok(())
}

fn stage_resources(
    host: &dyn Host,
    settings: &Settings,
    contents_dir: &Path,
) -> Result<(), CpackError> {
    let Some(ref res_src) = settings.resources_dir else {
        return Ok(());
    };
    for entry in host.read_dir(res_src)? {
        if entry.name == b"Packages" || entry.name == b"distribution.dist" {
            return Err(error(format!(
                "CPACK_PRODUCTBUILD_RESOURCES_DIR collides with generated control entry: {}",
                entry.name
            )));
        }
    }
    copy_checked_tree(
        host,
        res_src,
        contents_dir,
        SymlinkPolicy::PreserveContained,
        false,
    )
}

fn copy_resource_file(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    name: &str,
    contents_dir: &Path,
) -> Result<String, CpackError> {
    let upper = name.to_ascii_uppercase();
    let var = format!("CPACK_RESOURCE_FILE_{upper}");
    let path = match option_set(cfg, var.as_bytes()) {
        Some(raw) => absolute(cwd, raw),
        None => {
            let generic = match name {
                "License" => "CPack.GenericLicense.txt",
                "ReadMe" => "CPack.GenericDescription.txt",
                "Welcome" => "CPack.GenericWelcome.txt",
                _ => "",
            };
            let fallback = cfg
                .get(b"CMAKE_ROOT")
                .filter(|r| !r.is_empty())
                .map(|r| absolute(cwd, r).join("Templates").join(generic));
            match fallback {
                Some(p) if host.metadata(&p, Follow::Yes).is_ok() => p,
                _ => {
                    return Err(error(format!(
                        "CPack option: {var} not specified. It should point to {name}.rtf, {name}.html, or {name}.txt file"
                    )));
                }
            }
        }
    };
    let meta = host.metadata_fresh(&path, Follow::Yes).map_err(|_| {
        error(format!(
            "Cannot find {name} resource file: {}",
            path.display()
        ))
    })?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    if !matches!(ext.as_str(), ".rtfd" | ".rtf" | ".html" | ".txt") {
        return Err(error(format!(
            "Bad file extension specified: {ext}. Currently only .rtfd, .rtf, .html, and .txt files allowed."
        )));
    }
    let dest_name = format!("{name}{ext}");
    let dest = contents_dir.join(&dest_name);
    if ext == ".rtfd" && meta.kind == FileKind::Dir {
        if let Ok(existing) = host.metadata_fresh(&dest, Follow::No) {
            if existing.kind != FileKind::Dir {
                return Err(error(format!(
                    "Resource file collision with non-directory in Contents/: {dest_name}"
                )));
            }
            host.remove_dir_all(&dest)?;
        }
        host.create_dir_all(&dest)?;
        copy_checked_tree(host, &path, &dest, SymlinkPolicy::PreserveContained, false)?;
    } else if meta.kind == FileKind::File {
        prepare_fresh_output(host, &dest)?;
        host.copy_file(
            &path,
            &dest,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: true,
            },
        )?;
    } else {
        return Err(error(format!(
            "Unsupported resource file kind for {name}: {}",
            path.display()
        )));
    }
    Ok(dest_name)
}

#[allow(clippy::too_many_arguments)]
fn write_distribution_file(
    host: &dyn Host,
    cfg: &CpackConfig,
    cwd: &Path,
    settings: &Settings,
    catalog: &ComponentCatalog,
    comp_pkg_info: &BTreeMap<BString, (String, u64)>,
    contents_dir: &Path,
    dist_path: &Path,
    welcome_nopath: &str,
    readme_nopath: &str,
    license_nopath: &str,
) -> Result<(), CpackError> {
    let template_path = find_template(host, cfg, cwd, "CPack.distribution.dist.in", None)?;
    let template = read_template(host, &template_path)?;

    let mut xout = XmlWriter::new("\t", 1);
    xout.start_element("options")?;
    xout.attr("allow-external-scripts", "no")?;
    xout.attr("customize", "allow")?;
    if !settings.domains_enabled {
        xout.attr("rootVolumeOnly", "false")?;
    }
    xout.end_element("options")?;

    if settings.domains_enabled {
        xout.start_element("domains")?;
        xout.attr(
            "enable_anywhere",
            if settings.domains_anywhere {
                "true"
            } else {
                "false"
            },
        )?;
        xout.attr(
            "enable_currentUserHome",
            if settings.domains_user {
                "true"
            } else {
                "false"
            },
        )?;
        xout.attr(
            "enable_localSystem",
            if settings.domains_root {
                "true"
            } else {
                "false"
            },
        )?;
        xout.end_element("domains")?;
    }

    xout.start_element("choices-outline")?;
    for group in catalog.groups.values() {
        if group.parent_group.is_none() {
            write_choice_outline(catalog, group, &mut xout)?;
        }
    }
    for comp in catalog.components.values() {
        if comp.group.is_none() {
            let name = require_utf8(&comp.name, "CPack component name")?;
            xout.start_element("line")?;
            xout.attr("choice", &format!("{name}Choice"))?;
            xout.force_non_self_closing();
            xout.end_element("line")?;
        }
    }
    xout.end_element("choices-outline")?;

    for group in catalog.groups.values() {
        write_group_choice(group, &mut xout)?;
    }

    for comp in catalog.components.values() {
        write_component_choice(settings, catalog, comp, comp_pkg_info, &mut xout)?;
    }

    write_background(host, cfg, contents_dir, None, &mut xout)?;
    write_background(host, cfg, contents_dir, Some("darkAqua"), &mut xout)?;

    let fragment = xout.finish_fragment()?;
    let mut vars = BTreeMap::new();
    vars.insert(
        "CPACK_PACKAGE_NAME".to_owned(),
        escape_xml_text(&settings.package_name)?,
    );
    vars.insert(
        "CPACK_RESOURCE_FILE_WELCOME_NOPATH".to_owned(),
        escape_xml_attr(welcome_nopath)?,
    );
    vars.insert(
        "CPACK_RESOURCE_FILE_README_NOPATH".to_owned(),
        escape_xml_attr(readme_nopath)?,
    );
    vars.insert(
        "CPACK_RESOURCE_FILE_LICENSE_NOPATH".to_owned(),
        escape_xml_attr(license_nopath)?,
    );
    vars.insert("CPACK_APPLE_PKG_INSTALLER_CONTENT".to_owned(), fragment);

    let rendered = render_at_template(&template, &vars);
    prepare_fresh_output(host, dist_path)?;
    host.write(dist_path, rendered.as_bytes(), WriteMode::CreateNew)?;
    Ok(())
}

enum ChoiceOutlineStep<'a> {
    Enter(&'a GroupMeta),
    EmitComponentsAndClose(&'a GroupMeta),
}

fn write_choice_outline(
    catalog: &ComponentCatalog,
    root_group: &GroupMeta,
    xout: &mut XmlWriter,
) -> Result<(), CpackError> {
    let mut stack = vec![ChoiceOutlineStep::Enter(root_group)];
    while let Some(step) = stack.pop() {
        match step {
            ChoiceOutlineStep::Enter(group) => {
                let name = require_utf8(&group.name, "CPack group name")?;
                xout.start_element("line")?;
                xout.attr("choice", &format!("{name}Choice"))?;
                stack.push(ChoiceOutlineStep::EmitComponentsAndClose(group));
                for sub_name in group.subgroups.iter().rev() {
                    if let Some(sub) = catalog.groups.get(sub_name) {
                        stack.push(ChoiceOutlineStep::Enter(sub));
                    }
                }
            }
            ChoiceOutlineStep::EmitComponentsAndClose(group) => {
                for comp_name in &group.components {
                    let comp = require_utf8(comp_name, "CPack component name")?;
                    xout.start_element("line")?;
                    xout.attr("choice", &format!("{comp}Choice"))?;
                    xout.force_non_self_closing();
                    xout.end_element("line")?;
                }
                xout.end_element("line")?;
            }
        }
    }
    Ok(())
}

fn write_group_choice(group: &GroupMeta, xout: &mut XmlWriter) -> Result<(), CpackError> {
    let name = require_utf8(&group.name, "CPack group name")?;
    let title = require_utf8(&group.display_name, "CPack group display name")?;
    let desc = require_utf8(&group.description, "CPack group description")?;
    xout.start_element("choice")?;
    xout.attr("id", &format!("{name}Choice"))?;
    xout.attr("title", title)?;
    xout.attr("start_selected", "true")?;
    xout.attr("start_enabled", "true")?;
    xout.attr("start_visible", "true")?;
    if !desc.is_empty() {
        xout.attr("description", desc)?;
    }
    xout.end_element("choice")
}

fn write_component_choice(
    settings: &Settings,
    catalog: &ComponentCatalog,
    comp: &ComponentMeta,
    comp_pkg_info: &BTreeMap<BString, (String, u64)>,
    xout: &mut XmlWriter,
) -> Result<(), CpackError> {
    let name = require_utf8(&comp.name, "CPack component name")?;
    let title = require_utf8(&comp.display_name, "CPack component display name")?;
    let desc = require_utf8(&comp.description, "CPack component description")?;
    let pkg_id = settings.derive_identifier(Some(name));

    xout.start_element("choice")?;
    xout.attr("id", &format!("{name}Choice"))?;
    xout.attr("title", title)?;
    xout.attr(
        "start_selected",
        if comp.disabled && !comp.required {
            "false"
        } else {
            "true"
        },
    )?;
    xout.attr(
        "start_enabled",
        if comp.required { "false" } else { "true" },
    )?;
    xout.attr("start_visible", if comp.hidden { "false" } else { "true" })?;
    if !desc.is_empty() {
        xout.attr("description", desc)?;
    }
    if !comp.depends.is_empty() || !comp.reverse_depends.is_empty() {
        let mut expr = String::from("my.choice.selected");
        let mut visited = BTreeSet::new();
        append_dep_expr(catalog, comp, &mut visited, &mut expr)?;
        visited.clear();
        append_reverse_dep_expr(catalog, comp, &mut visited, &mut expr)?;
        xout.attr("selected", &expr)?;
    }
    xout.start_element("pkg-ref")?;
    xout.attr("id", &pkg_id)?;
    xout.end_element("pkg-ref")?;
    xout.end_element("choice")?;

    let (leaf_name, install_kbytes) = comp_pkg_info
        .get(&comp.name)
        .cloned()
        .unwrap_or_else(|| (format!("{name}.pkg"), 1));
    xout.start_element("pkg-ref")?;
    xout.attr("id", &pkg_id)?;
    xout.attr("version", &settings.version)?;
    xout.attr("installKBytes", &install_kbytes.to_string())?;
    if !settings.domains_enabled {
        xout.attr("auth", "Admin")?;
    }
    xout.attr("onConclusion", "None")?;
    if comp.downloaded {
        let site = settings.download_site.as_deref().ok_or_else(|| {
            error(format!(
                "CPACK_DOWNLOAD_SITE must be set for downloaded component \"{name}\""
            ))
        })?;
        xout.text(&format!("{site}{leaf_name}"))?;
    } else {
        let rel = format!("Contents/Packages/{leaf_name}");
        xout.text(&format!("file:./{}", encode_url_path(&rel)))?;
    }
    xout.end_element("pkg-ref")
}

pub(super) fn append_dep_expr(
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
            let dep_str = require_utf8(dep_name, "CPack component dependency")?;
            let escaped = escape_js_single_quoted(&format!("{dep_str}Choice"))?;
            out.push_str(&format!(" && choices['{escaped}'].selected"));
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

fn append_reverse_dep_expr(
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
            let rdep_str = require_utf8(rdep_name, "CPack component dependency")?;
            let escaped = escape_js_single_quoted(&format!("{rdep_str}Choice"))?;
            out.push_str(&format!(" || choices['{escaped}'].selected"));
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

fn write_background(
    host: &dyn Host,
    cfg: &CpackConfig,
    contents_dir: &Path,
    theme: Option<&str>,
    xout: &mut XmlWriter,
) -> Result<(), CpackError> {
    let opt = match theme {
        Some(t) => format!("CPACK_PRODUCTBUILD_BACKGROUND_{}", t.to_ascii_uppercase()),
        None => "CPACK_PRODUCTBUILD_BACKGROUND".to_owned(),
    };
    let Some(raw_file) = option_set(cfg, opt.as_bytes()) else {
        return Ok(());
    };
    let file_str = require_utf8(raw_file, &opt)?;
    let rel = super::installer::safe_relative_subpath(file_str, &opt)?;
    let canon_contents = crate::source::canonical_checked_root(host, contents_dir)?;
    let check_path = contents_dir.join(&rel);
    if host.metadata_fresh(&check_path, Follow::No).is_err()
        || !host
            .canonicalize_fresh(&check_path)?
            .starts_with(&canon_contents)
    {
        return Err(error(format!(
            "Background image doesn't exist in the resource directory: {file_str}"
        )));
    }
    let elem = match theme {
        Some(t) => format!("background-{t}"),
        None => "background".to_owned(),
    };
    xout.start_element(&elem)?;
    xout.attr("file", &from_native_cmake(&rel).to_string())?;
    for (suffix, attr) in [
        ("ALIGNMENT", "alignment"),
        ("SCALING", "scaling"),
        ("MIME_TYPE", "mime-type"),
        ("UTI", "uti"),
    ] {
        let key = format!("{opt}_{suffix}");
        if let Some(val) = option_set(cfg, key.as_bytes()) {
            xout.attr(attr, require_utf8(val, &key)?)?;
        }
    }
    xout.end_element(&elem)
}
