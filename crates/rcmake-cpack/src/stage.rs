//! Stage installed files through the host and the CMake interpreter.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, ByteSlice, hash::HashAlgo, list, truth};
use rcmake_eval::{CmdError, CmdResult, Cx, Interp};
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, HostResult, Input, Output, ProcessResult, RunRequest,
};

use crate::archive_gen::ArchiveKind;
use crate::config::{
    CpackConfig, Environment, absolute, cmake_executable, interpreter, script, snapshot,
};
use crate::{
    CPackOptions, ComponentGrouping, ComponentMembers, CpackError, InstalledComponent, error, log,
    safe_name,
};

pub(crate) mod component_files;

/// Install the configured files into the staging directory.
///
/// # Errors
///
/// Returns an error if a staging operation or install script fails.
pub fn stage(
    host: &Arc<dyn Host>,
    config: &CpackConfig,
    staging_dir: &Path,
) -> Result<(), CpackError> {
    let cwd = host.initial_cwd();
    let options = CPackOptions::default();
    let mut interp = interpreter(host, &cwd, &options, None)?;
    for (name, value) in &config.vars {
        interp.cx().set_var(name, value.clone());
    }
    host.create_dir_all(staging_dir)?;
    let canon_staging = crate::source::canonical_checked_root(host.as_ref(), staging_dir)?;
    install(
        host,
        &cwd,
        &options,
        &mut interp,
        staging_dir,
        &canon_staging,
    )
    .map(|_| ())
}

/// Stage the configured files and run the pre-build scripts.
///
/// Returns the archive components that the project installs staged, with the paths
/// that each install added before the pre-build scripts ran.
pub(crate) fn install(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    interp: &mut Interp,
    staging: &Path,
    canon_staging: &Path,
) -> Result<ComponentMembers, CpackError> {
    let cfg = snapshot(interp);
    staging_directory_mode(&cfg)?;
    let stage_files = cfg.get(b"CPACK_GENERATOR") != Some(b"External")
        || cfg.on(b"CPACK_EXTERNAL_ENABLE_STAGING");
    let clear_destdir = BTreeMap::from([("DESTDIR".into(), None)]);
    let _env = stage_files.then(|| Environment::apply(host.as_ref(), &clear_destdir));
    let cfg = if stage_files {
        let install_scripts = if cfg.get(b"CPACK_INSTALL_SCRIPTS").is_some() {
            if cfg.get(b"CPACK_INSTALL_SCRIPT").is_some() {
                host.write_out(
                    rcmake_host::Stream::Stderr,
                    b"CPack Warning: Both CPACK_INSTALL_SCRIPTS and CPACK_INSTALL_SCRIPT are set, the latter will be ignored.\n",
                );
            }
            b"CPACK_INSTALL_SCRIPTS".as_slice()
        } else {
            b"CPACK_INSTALL_SCRIPT"
        };
        let scripts = cfg.list(install_scripts);
        if !scripts.is_empty() {
            let prefix = install_prefix(&cfg, b"CPACK_INSTALL_PREFIX");
            let location = InstallLocation::new(&cfg, staging, prefix)?;
            let _env = location.environment(host.as_ref());
            for file in scripts {
                interp
                    .cx()
                    .set_var(b"CMAKE_INSTALL_PREFIX", location.prefix.clone());
                log(host.as_ref(), format!("- Install script: {file}"));
                script(interp, &absolute(cwd, &file))?;
            }
        }
        let cfg = snapshot(interp);
        crate::source::copy_directories(host.as_ref(), &cfg, cwd, staging, interp)?;
        cfg
    } else {
        cfg
    };
    let (members, destinations) =
        install_projects(host, cwd, options, &cfg, staging, canon_staging)?;
    if matches!(
        cfg.get(b"CPACK_GENERATOR"),
        Some(b"RPM" | b"NSIS" | b"NSIS64" | b"WIX" | b"IFW")
    ) {
        let mut all = Vec::new();
        for (suffix, files) in destinations {
            if !suffix.is_empty() && cfg.get(b"CPACK_GENERATOR") == Some(b"RPM") {
                let key = [b"CPACK_ABSOLUTE_DESTINATION_FILES_".as_slice(), &suffix].concat();
                interp.cx().set_var(&key, list::join(&files));
            }
            all.extend(files);
        }
        if cfg.get(b"CPACK_GENERATOR") == Some(b"RPM") || !all.is_empty() {
            interp
                .cx()
                .set_var(b"CPACK_ABSOLUTE_DESTINATION_FILES", list::join(&all));
        }
    }
    crate::source::verify_expected_root(host.as_ref(), staging, canon_staging)?;
    run_hooks(
        host.as_ref(),
        cwd,
        interp,
        b"CPACK_PRE_BUILD_SCRIPTS",
        "Executing pre-build script",
    )?;
    crate::source::verify_expected_root(host.as_ref(), staging, canon_staging)?;
    Ok(members)
}

pub(crate) fn run_hooks(
    host: &dyn Host,
    cwd: &Path,
    interp: &mut Interp,
    variable: &[u8],
    label: &str,
) -> Result<(), CpackError> {
    for file in snapshot(interp).list(variable) {
        log(host, format!("{label}: {file}"));
        script(interp, &absolute(cwd, &file))?;
    }
    Ok(())
}

type AbsoluteDestinations = BTreeMap<BString, Vec<BString>>;

fn install_projects(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    cfg: &CpackConfig,
    staging: &Path,
    canon_staging: &Path,
) -> Result<(ComponentMembers, AbsoluteDestinations), CpackError> {
    let mut members = ComponentMembers::new();
    let mut absolute_files = AbsoluteDestinations::new();
    let project_key = b"CPACK_INSTALL_CMAKE_PROJECTS";
    if cfg.get(project_key).is_none_or(<[u8]>::is_empty) {
        return Ok((members, absolute_files));
    }
    let generator = cfg.get(b"CPACK_CMAKE_GENERATOR").ok_or_else(|| {
        error(
            "CPACK_INSTALL_CMAKE_PROJECTS is specified, but CPACK_CMAKE_GENERATOR is not. CPACK_CMAKE_GENERATOR is required to install the project.",
        )
    })?;
    let projects = cfg.list(project_key);
    let (tuples, remainder) = projects.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(error(
            "Not enough items on list: CPACK_INSTALL_CMAKE_PROJECTS. CPACK_INSTALL_CMAKE_PROJECTS should hold quadruplet of install directory, install project name, install component, and install subdirectory.",
        ));
    }
    if tuples.is_empty() {
        return Ok((members, absolute_files));
    }
    let build_generator = build_generator(generator).ok_or_else(|| {
        error("Specified package generator not found. CPACK_CMAKE_GENERATOR value is invalid.")
    })?;
    let mut configs = cfg.list(b"CPACK_BUILD_CONFIG");
    configs.sort();
    configs.dedup();
    if configs.is_empty() {
        configs.push(BString::default());
    }
    let external = cfg.get(b"CPACK_GENERATOR") == Some(b"External");
    let stage_project = !external || cfg.on(b"CPACK_EXTERNAL_ENABLE_STAGING");
    let prefix = project_install_prefix(cfg);
    let record = records_members(cfg);
    let directory_mode = staging_directory_mode(cfg)?;
    for [build, project, component, subdir] in tuples {
        let build = absolute(cwd, build);
        for config in &configs {
            if stage_project {
                preinstall(host.as_ref(), cfg, build_generator, &build, config)?;
            }
            log(
                host.as_ref(),
                format!("- Install project: {project} [{config}]"),
            );
            if !stage_project {
                continue;
            }
            let separate = !package_components(cfg, component).is_empty();
            let destinations =
                install_destinations(host.as_ref(), cfg, staging, component, subdir)?;
            for (component, destination) in &destinations {
                if separate {
                    log(host.as_ref(), format!("-   Install component: {component}"));
                }
                let location = InstallLocation::new(cfg, destination, prefix)?;
                ensure_dir_with_mode(host.as_ref(), staging, &location.directory, directory_mode)?;
                // CPack lists the install prefix directory, or the DESTDIR directory
                // because DESTDIR installs can write outside the install prefix.
                let scanned = if location.destdir.is_some() {
                    destination.clone()
                } else {
                    location.directory.clone()
                };
                let before = (record && separate)
                    .then(|| component_files::snapshot(host.as_ref(), &scanned))
                    .transpose()?;
                let installed =
                    install_project(host, options, cfg, &build, config, component, location)?;
                let suffix = if separate {
                    linux_suffix(cfg, component)
                } else {
                    b""
                };
                absolute_files
                    .entry(suffix.into())
                    .or_default()
                    .extend(installed);
                if let Some(before) = before {
                    let after = component_files::snapshot(host.as_ref(), &scanned)?;
                    let added: Vec<PathBuf> = after.difference(&before).cloned().collect();
                    let index = match members.iter().position(|item| &item.name == component) {
                        Some(index) => index,
                        None => {
                            members.push(InstalledComponent {
                                name: component.clone(),
                                group: component_group(cfg, component).map(BString::from),
                                files: Vec::new(),
                                staged_roots: Vec::new(),
                            });
                            members.len() - 1
                        }
                    };
                    let canon_root =
                        crate::source::canonical_checked_root(host.as_ref(), destination)?;
                    let canon_scanned =
                        crate::source::canonical_checked_root(host.as_ref(), &scanned)?;
                    if !canon_root.starts_with(canon_staging)
                        || !canon_scanned.starts_with(&canon_root)
                    {
                        return Err(error(format!(
                            "CPack component root escapes staging directory: {}",
                            destination.display()
                        )));
                    }
                    if let Some(item) = members.get_mut(index) {
                        item.files.extend(added.iter().cloned());
                        if let Some(existing_root) = item
                            .staged_roots
                            .iter_mut()
                            .find(|entry| entry.root == *destination && entry.scanned == scanned)
                        {
                            existing_root.files.extend(added);
                        } else {
                            item.staged_roots.push(crate::StagedRoot {
                                root: destination.clone(),
                                scanned,
                                canon_root,
                                canon_scanned,
                                files: added,
                            });
                        }
                    }
                }
            }
        }
    }
    Ok((members, absolute_files))
}

fn records_members(cfg: &CpackConfig) -> bool {
    cfg.get(b"CPACK_GENERATOR").is_some_and(|name| {
        matches!(
            name,
            b"productbuild" | b"NSIS" | b"NSIS64" | b"WIX" | b"IFW" | b"NuGet"
        ) || ArchiveKind::ALL
            .iter()
            .any(|kind| kind.name().as_bytes() == name)
    })
}

fn install_destinations(
    host: &dyn Host,
    cfg: &CpackConfig,
    staging: &Path,
    component: &BString,
    subdir: &[u8],
) -> Result<Vec<(BString, PathBuf)>, CpackError> {
    let mut root = staging.to_owned();
    root.extend(relative_install_path(subdir)?.iter());
    let components = package_components(cfg, component);
    if components.is_empty() {
        return Ok(vec![(component.clone(), root)]);
    }
    let windows_rules = crate::generators::installer::is_windows_host(host);
    components
        .into_iter()
        .map(|name| {
            let mut destination = root.join(component_dir_for_host(
                linux_suffix(cfg, &name),
                windows_rules,
            ));
            if cfg.on(b"CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
                let file_name = safe_name(cfg.get(b"CPACK_PACKAGE_FILE_NAME").unwrap_or_default())?;
                destination.push(file_name);
            }
            Ok((name, destination))
        })
        .collect()
}

pub(crate) fn project_install_prefix(cfg: &CpackConfig) -> &[u8] {
    let destdir_key = if cfg.get(b"CPACK_SET_DESTDIR") == Some(b"I_ON") {
        b"CPACK_PACKAGING_INSTALL_PREFIX".as_slice()
    } else {
        b"CPACK_INSTALL_PREFIX"
    };
    install_prefix(cfg, destdir_key)
}

fn install_prefix<'a>(cfg: &'a CpackConfig, destdir_key: &[u8]) -> &'a [u8] {
    let key = if uses_destdir(cfg) {
        destdir_key
    } else {
        b"CPACK_PACKAGING_INSTALL_PREFIX"
    };
    cfg.get(key).unwrap_or_default()
}

pub(crate) fn uses_destdir(cfg: &CpackConfig) -> bool {
    cfg.on(b"CPACK_SET_DESTDIR") || cfg.get(b"CPACK_SET_DESTDIR") == Some(b"I_ON")
}

struct InstallLocation {
    prefix: BString,
    destdir: Option<BString>,
    directory: PathBuf,
}

impl InstallLocation {
    fn new(cfg: &CpackConfig, root: &Path, configured_prefix: &[u8]) -> Result<Self, CpackError> {
        let use_destdir = uses_destdir(cfg);
        let mut directory = root.to_owned();
        directory.extend(relative_install_path(configured_prefix)?.iter());
        let prefix = if use_destdir {
            configured_prefix.into()
        } else {
            from_native(&directory)
        };
        Ok(Self {
            prefix,
            destdir: use_destdir.then(|| from_native(root)),
            directory,
        })
    }

    fn environment<'a>(&self, host: &'a dyn Host) -> Environment<'a> {
        Environment::apply(
            host,
            &BTreeMap::from([("DESTDIR".into(), self.destdir.clone())]),
        )
    }
}

fn install_project(
    host: &Arc<dyn Host>,
    options: &CPackOptions,
    cfg: &CpackConfig,
    build: &Path,
    config: &[u8],
    component: &[u8],
    location: InstallLocation,
) -> Result<Vec<BString>, CpackError> {
    let install_host: Arc<dyn Host> = Arc::new(InstallHost {
        inner: Arc::clone(host),
        verbose: options.verbose,
    });
    let mut interp = interpreter(&install_host, build, options, None)?;
    let _env = location.environment(host.as_ref());
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_INSTALL_PREFIX", location.prefix);
    // The install script derives CMAKE_INSTALL_CONFIG_NAME from BUILD_TYPE.
    if !config.is_empty() {
        cx.set_var(b"BUILD_TYPE", config.into());
    }
    if !component.is_empty() && component != b"ALL" {
        cx.set_var(b"CMAKE_INSTALL_COMPONENT", component.into());
    }
    if let Some(permissions) = configured_permissions(cfg).filter(|p| !p.is_empty()) {
        cx.set_var(
            b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
            permissions.into(),
        );
    }
    if cfg
        .get(b"CPACK_STRIP_FILES")
        .is_some_and(|value| !truth::is_off(value))
    {
        cx.set_var(b"CMAKE_INSTALL_DO_STRIP", "1".into());
    }
    for (option, variable) in [
        (
            b"CPACK_WARN_ON_ABSOLUTE_INSTALL_DESTINATION".as_slice(),
            b"CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION".as_slice(),
        ),
        (
            b"CPACK_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION",
            b"CMAKE_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION",
        ),
    ] {
        if let Some(value) = cfg.get(option) {
            cx.set_var(variable, value.into());
        }
    }
    for entry in cfg.list(b"CPACK_CUSTOM_INSTALL_VARIABLES") {
        let (name, value) = entry.split_once_str(b"=").unwrap_or((&entry, b""));
        if !name.is_empty() {
            cx.set_var(name, value.into());
        }
    }
    let file = build.join("cmake_install.cmake");
    if interp.run_script(&file).exit_code != 0 {
        return Err(error(format!(
            "Error when running the install script: {}",
            file.display()
        )));
    }
    Ok(list::split(
        &interp
            .cx()
            .var_or_empty(b"CMAKE_ABSOLUTE_DESTINATION_FILES"),
        list::Empty::Drop,
    ))
}

const CPACK_DIRECTORY_PERMISSIONS: &[u8] = b"CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS";
const CMAKE_DIRECTORY_PERMISSIONS: &[u8] = b"CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS";

fn configured_permissions(cfg: &CpackConfig) -> Option<&[u8]> {
    cfg.get(CPACK_DIRECTORY_PERMISSIONS)
        .or_else(|| cfg.get(CMAKE_DIRECTORY_PERMISSIONS))
}

pub(crate) fn staging_directory_mode(cfg: &CpackConfig) -> Result<Option<u32>, CpackError> {
    directory_mode(configured_permissions(cfg).unwrap_or_default()).map_err(|permission| {
        error(format!(
            "Invalid permission value '{permission}'. CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS value is invalid."
        ))
    })
}

fn directory_mode(value: &[u8]) -> Result<Option<u32>, BString> {
    if value.is_empty() {
        return Ok(None);
    }
    let mut mode = 0;
    for permission in list::iter(value, list::Empty::Drop) {
        mode |= match permission.as_ref() {
            b"OWNER_READ" => 0o400,
            b"OWNER_WRITE" => 0o200,
            b"OWNER_EXECUTE" => 0o100,
            b"GROUP_READ" => 0o040,
            b"GROUP_WRITE" => 0o020,
            b"GROUP_EXECUTE" => 0o010,
            b"WORLD_READ" => 0o004,
            b"WORLD_WRITE" => 0o002,
            b"WORLD_EXECUTE" => 0o001,
            b"SETUID" => 0o4000,
            b"SETGID" => 0o2000,
            _ => return Err(permission.as_ref().into()),
        };
    }
    Ok(Some(mode))
}

thread_local! {
    // The file command creates directories through the install host on this thread.
    static INSTALL_DIRECTORY_MODE: Cell<Option<u32>> = const { Cell::new(None) };
}

pub(crate) fn with_install_directory_mode(
    cx: &mut Cx<'_>,
    install: impl FnOnce(&mut Cx<'_>) -> CmdResult,
) -> CmdResult {
    let mode = directory_mode(&cx.var_or_empty(CMAKE_DIRECTORY_PERMISSIONS)).map_err(|arg| {
        CmdError::status(format!(
            "INSTALL given invalid permission \"{arg}\". Set with CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS variable."
        ))
    })?;
    INSTALL_DIRECTORY_MODE.with(|active| {
        let previous = active.replace(mode);
        let result = install(cx);
        active.set(previous);
        result
    })
}

/// The host of a CPack install script.
///
/// CPack sends the status messages of `cmake_install.cmake` (`-- Installing:`,
/// `message(STATUS)`) to its verbose log: `CPack Verbose: <message>` with `-V`,
/// and nothing otherwise. Notices, warnings, and errors on stderr are unchanged.
struct InstallHost {
    inner: Arc<dyn Host>,
    verbose: bool,
}

impl Host for InstallHost {
    rcmake_host::forward_host!(inner, env, process);
    rcmake_host::forward_host!(@method inner, metadata(p: &::std::path::Path, follow: rcmake_host::Follow) -> rcmake_host::HostResult<rcmake_host::Metadata>);
    rcmake_host::forward_host!(@method inner, metadata_fresh(p: &::std::path::Path, follow: rcmake_host::Follow) -> rcmake_host::HostResult<rcmake_host::Metadata>);
    rcmake_host::forward_host!(@ inner, read);
    rcmake_host::forward_host!(@ inner, read_with_origin);
    rcmake_host::forward_host!(@ inner, read_chunks);
    rcmake_host::forward_host!(@method inner, write(p: &::std::path::Path, data: &[u8], mode: rcmake_host::WriteMode) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, read_dir(p: &::std::path::Path) -> rcmake_host::HostResult<Vec<rcmake_host::DirEntry>>);
    rcmake_host::forward_host!(@method inner, create_private_dir(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_file(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_dir(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, remove_dir_all(p: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, rename(from: &::std::path::Path, to: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, rename_noreplace(from: &::std::path::Path, to: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, lock_file(path: &::std::path::Path,
            timeout: Option<::core::time::Duration>,
        ) -> rcmake_host::HostResult<Box<dyn rcmake_host::FileLockGuard>>);
    rcmake_host::forward_host!(@method inner, copy_file(from: &::std::path::Path, to: &::std::path::Path, opts: rcmake_host::CopyOptions) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, symlink(target: &::std::path::Path, link: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, hard_link(src: &::std::path::Path, dst: &::std::path::Path) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@ inner, read_link);
    rcmake_host::forward_host!(@method inner, canonicalize(p: &::std::path::Path) -> rcmake_host::HostResult<::std::path::PathBuf>);
    rcmake_host::forward_host!(@method inner, canonicalize_fresh(p: &::std::path::Path) -> rcmake_host::HostResult<::std::path::PathBuf>);
    rcmake_host::forward_host!(@method inner, set_permissions(p: &::std::path::Path, mode: u32) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, set_times(p: &::std::path::Path,
            atime: Option<rcmake_host::FileTime>,
            mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@method inner, set_symlink_times(path: &::std::path::Path,
            atime: Option<rcmake_host::FileTime>,
            mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()>);
    rcmake_host::forward_host!(@ inner, open_read);
    rcmake_host::forward_host!(@ inner, scratch_file);
    rcmake_host::forward_host!(@ inner, scratch_file_in);
    rcmake_host::forward_host!(@method inner, write_from(path: &::std::path::Path,
            mode: rcmake_host::WriteMode,
            input: &mut dyn ::std::io::Read,
            len: u64,
        ) -> rcmake_host::HostResult<()>);

    fn create_dir_all(&self, path: &Path) -> HostResult<()> {
        let Some(mode) = INSTALL_DIRECTORY_MODE.get() else {
            return self.inner.create_dir_all(path);
        };
        let mut missing = Vec::new();
        for directory in path.ancestors() {
            match self.inner.metadata(directory, Follow::Yes) {
                Ok(_) => break,
                Err(err) if err.kind == ErrorKind::NotFound => missing.push(directory),
                Err(err) => return Err(err),
            }
        }
        self.inner.create_dir_all(path)?;
        for directory in missing {
            self.inner.set_permissions(directory, mode)?;
        }
        Ok(())
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> rcmake_host::HostResult<Option<BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }

    fn write_out(&self, stream: rcmake_host::Stream, bytes: &[u8]) {
        if stream != rcmake_host::Stream::Stdout {
            self.inner.write_out(stream, bytes);
            return;
        }
        if !self.verbose {
            return;
        }
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let message = line.strip_prefix(b"-- ").unwrap_or(line);
            let mut out = b"CPack Verbose: ".to_vec();
            out.extend_from_slice(message);
            out.push(b'\n');
            self.inner.write_out(stream, &out);
        }
    }

    fn read_stdin(&self, limit: u64) -> rcmake_host::HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: rcmake_host::Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn terminal_width(&self, stream: rcmake_host::Stream) -> Option<usize> {
        self.inner.terminal_width(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> rcmake_host::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> rcmake_host::HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.inner.system()
    }

    fn effective_uid(&self) -> Option<u32> {
        self.inner.effective_uid()
    }
}

// CPack can install a tree generated by CMake, including generators that Rcmake does not implement.
fn build_generator(name: &[u8]) -> Option<&[u8]> {
    let windows = cfg!(windows);
    let name: &[u8] = if let Some(separator) = name.windows(3).position(|part| part == b" - ") {
        let extra = name.get(..separator)?;
        let base = name.get(separator + 3..)?;
        match (extra, base) {
            (
                b"CodeBlocks" | b"CodeLite" | b"Eclipse CDT4" | b"Kate" | b"Sublime Text 2",
                b"Ninja" | b"Unix Makefiles",
            )
            | (b"Kate", b"Ninja Multi-Config") => base,
            (
                b"CodeBlocks" | b"CodeLite" | b"Eclipse CDT4" | b"Kate" | b"Sublime Text 2",
                b"MinGW Makefiles" | b"NMake Makefiles",
            )
            | (b"CodeBlocks", b"NMake Makefiles JOM")
                if windows =>
            {
                base
            }
            _ => return None,
        }
    } else {
        match name {
            b"CodeBlocks" | b"CodeLite" | b"Kate" | b"Sublime Text 2" if windows => {
                b"MinGW Makefiles"
            }
            b"Eclipse CDT4" if windows => b"NMake Makefiles",
            b"CodeBlocks" | b"CodeLite" | b"Eclipse CDT4" | b"Kate" | b"Sublime Text 2" => b"Ninja",
            _ => name,
        }
    };
    match name {
        b"Ninja" | b"Ninja Multi-Config" | b"Unix Makefiles" | b"FASTBuild" | b"Watcom WMake" => {
            Some(name)
        }
        b"Xcode" if cfg!(target_os = "macos") => Some(name),
        b"Green Hills MULTI" if windows || cfg!(target_os = "linux") => Some(name),
        b"MSYS Makefiles"
        | b"MinGW Makefiles"
        | b"Borland Makefiles"
        | b"NMake Makefiles"
        | b"NMake Makefiles JOM"
        | b"Visual Studio 15"
        | b"Visual Studio 15 2017"
        | b"Visual Studio 15 2017 Win64"
        | b"Visual Studio 15 2017 ARM"
        | b"Visual Studio 16"
        | b"Visual Studio 16 2019"
        | b"Visual Studio 17"
        | b"Visual Studio 17 2022"
        | b"Visual Studio 18"
        | b"Visual Studio 18 2026"
            if windows =>
        {
            Some(name)
        }
        _ => None,
    }
}

fn preinstall(
    host: &dyn Host,
    cfg: &CpackConfig,
    generator: &[u8],
    build: &Path,
    config: &[u8],
) -> Result<(), CpackError> {
    if cfg.on(b"CPACK_SKIP_PREINSTALL")
        || !matches!(
            generator,
            b"Unix Makefiles"
                | b"MSYS Makefiles"
                | b"MinGW Makefiles"
                | b"NMake Makefiles"
                | b"NMake Makefiles JOM"
                | b"Watcom WMake"
                | b"Borland Makefiles"
        )
        || host
            .metadata(&build.join("CMakeCache.txt"), Follow::Yes)
            .is_err()
    {
        return Ok(());
    }
    let mut args = vec![
        "--build".into(),
        from_native(build),
        "--target".into(),
        "preinstall".into(),
    ];
    if !config.is_empty() {
        args.extend(["--config".into(), config.into()]);
    }
    let program = from_native(&cmake_executable(host)?);
    let output = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec { program, args }],
            cwd: build.to_owned(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::SharedWithStdout,
            timeout: None,
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    )?;
    if output.results == [ProcessResult::Exited(0)] {
        return Ok(());
    }
    let details = BString::from(output.stdout);
    if details
        .windows(b"No rule to make target".len())
        .any(|text| text == b"No rule to make target")
        && details
            .windows(b"preinstall".len())
            .any(|text| text == b"preinstall")
    {
        return Ok(());
    }
    Err(error(format!(
        "Problem running preinstall in {}\n{details}",
        build.display()
    )))
}

fn linux_suffix<'a>(cfg: &'a CpackConfig, component: &'a [u8]) -> &'a [u8] {
    if !matches!(cfg.get(b"CPACK_GENERATOR"), Some(b"DEB" | b"RPM")) {
        return component;
    }
    match grouping(cfg) {
        ComponentGrouping::AllComponentsInOne => b"ALL_COMPONENTS_IN_ONE",
        ComponentGrouping::OnePerGroup => component_group(cfg, component).unwrap_or(component),
        ComponentGrouping::Ignore => component,
    }
}

pub(crate) fn grouping(cfg: &CpackConfig) -> ComponentGrouping {
    if matches!(cfg.get(b"CPACK_GENERATOR"), Some(b"DEB" | b"RPM")) {
        return match cfg.get(b"CPACK_COMPONENTS_GROUPING") {
            Some(b"ALL_COMPONENTS_IN_ONE") => ComponentGrouping::AllComponentsInOne,
            Some(b"IGNORE") => ComponentGrouping::Ignore,
            Some(b"ONE_PER_GROUP") => ComponentGrouping::OnePerGroup,
            _ if cfg.get(b"CPACK_COMPONENTS_ONE_PACKAGE_PER_GROUP").is_some() => {
                ComponentGrouping::OnePerGroup
            }
            _ if cfg.get(b"CPACK_COMPONENTS_IGNORE_GROUPS").is_some() => ComponentGrouping::Ignore,
            _ if cfg.get(b"CPACK_COMPONENTS_ALL_IN_ONE_PACKAGE").is_some() => {
                ComponentGrouping::AllComponentsInOne
            }
            _ => ComponentGrouping::OnePerGroup,
        };
    }
    match cfg.get(b"CPACK_COMPONENTS_GROUPING") {
        Some(value) if value.eq_ignore_ascii_case(b"ALL_COMPONENTS_IN_ONE") => {
            ComponentGrouping::AllComponentsInOne
        }
        Some(value) if value.eq_ignore_ascii_case(b"IGNORE") => ComponentGrouping::Ignore,
        Some(value) if value.eq_ignore_ascii_case(b"ONE_PER_GROUP") => {
            ComponentGrouping::OnePerGroup
        }
        _ if cfg.on(b"CPACK_COMPONENTS_ALL_IN_ONE_PACKAGE") => {
            ComponentGrouping::AllComponentsInOne
        }
        _ if cfg.on(b"CPACK_COMPONENTS_IGNORE_GROUPS") => ComponentGrouping::Ignore,
        _ => ComponentGrouping::OnePerGroup,
    }
}

pub(crate) fn package_components(cfg: &CpackConfig, selection: &[u8]) -> Vec<BString> {
    let enabled = match cfg.get(b"CPACK_GENERATOR") {
        Some(b"External" | b"productbuild" | b"NSIS" | b"NSIS64" | b"WIX" | b"IFW") => true,
        Some(b"DEB") => cfg.on(b"CPACK_DEB_COMPONENT_INSTALL"),
        Some(b"RPM") => cfg.on(b"CPACK_RPM_COMPONENT_INSTALL"),
        Some(b"NuGet") => cfg.on(b"CPACK_NUGET_COMPONENT_INSTALL"),
        Some(b"AppImage") => false,
        Some(b"DragNDrop") => cfg
            .get(b"CPACK_DMG_COMPONENT_INSTALL")
            .is_none_or(truth::is_on),
        _ => cfg.on(b"CPACK_ARCHIVE_COMPONENT_INSTALL"),
    };
    if enabled {
        project_components(cfg, selection)
    } else {
        Vec::new()
    }
}

pub(crate) fn component_roots(
    cfg: &CpackConfig,
) -> Result<BTreeMap<BString, BTreeSet<PathBuf>>, CpackError> {
    let mut roots = BTreeMap::<_, BTreeSet<_>>::new();
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    for [_, _, selection, subdir] in projects.as_chunks::<4>().0 {
        let components = package_components(cfg, selection);
        if components.is_empty() {
            continue;
        }
        let subdir = relative_install_path(subdir)?;
        for component in components {
            let root = subdir.join(component_dir(&component));
            roots.entry(component).or_default().insert(root);
        }
    }
    Ok(roots)
}

pub(crate) fn project_components(cfg: &CpackConfig, selection: &[u8]) -> Vec<BString> {
    if cfg.on(b"CPACK_MONOLITHIC_INSTALL") {
        return Vec::new();
    }
    let name = format!(
        "CPACK_COMPONENTS_{}",
        BString::from(selection.to_ascii_uppercase())
    );
    cfg.list(name.as_bytes())
}

pub(crate) fn component_dir(name: &[u8]) -> PathBuf {
    component_dir_for_host(name, cfg!(windows))
}

pub(crate) fn component_dir_for_host(name: &[u8], windows_rules: bool) -> PathBuf {
    if windows_rules {
        if is_exact_windows_reserved(name) {
            let mut prefixed = Vec::with_capacity(name.len() + 1);
            prefixed.push(b'_');
            prefixed.extend_from_slice(name);
            return to_native(&prefixed);
        }
        if name.ends_with(b".") {
            let mut suffixed = Vec::with_capacity(name.len() + 1);
            suffixed.extend_from_slice(name);
            suffixed.push(b'_');
            return to_native(&suffixed);
        }
    }
    if name.iter().any(|byte| b"<>\"/\\|?*`".contains(byte))
        || (windows_rules && name.contains(&b':'))
    {
        return HashAlgo::Md5.hex_digest(name).into();
    }
    safe_name(name).unwrap_or_else(|_| HashAlgo::Md5.hex_digest(name).into())
}

fn is_exact_windows_reserved(name: &[u8]) -> bool {
    let trimmed = name.strip_suffix(b".").unwrap_or(name);
    let upper = trimmed.to_ascii_uppercase();
    matches!(
        upper.as_slice(),
        b"CON"
            | b"PRN"
            | b"AUX"
            | b"NUL"
            | b"COM1"
            | b"COM2"
            | b"COM3"
            | b"COM4"
            | b"COM5"
            | b"COM6"
            | b"COM7"
            | b"COM8"
            | b"COM9"
            | b"LPT1"
            | b"LPT2"
            | b"LPT3"
            | b"LPT4"
            | b"LPT5"
            | b"LPT6"
            | b"LPT7"
            | b"LPT8"
            | b"LPT9"
    )
}

pub(crate) fn component_group<'a>(cfg: &'a CpackConfig, component: &[u8]) -> Option<&'a [u8]> {
    let key = format!(
        "CPACK_COMPONENT_{}_GROUP",
        BString::from(component.to_ascii_uppercase())
    );
    cfg.get(key.as_bytes()).filter(|name| !name.is_empty())
}

pub(crate) fn relative_install_path(value: &[u8]) -> Result<PathBuf, CpackError> {
    let path = to_native(value);
    if value.contains(&0)
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(error(format!(
            "Invalid CPack install path: {}",
            BString::from(value)
        )));
    }
    Ok(path
        .components()
        .filter(|part| matches!(part, Component::Normal(_)))
        .collect())
}

pub(crate) fn ensure_dir(host: &dyn Host, root: &Path, path: &Path) -> Result<(), CpackError> {
    ensure_dir_with_mode(host, root, path, None)
}

pub(crate) fn ensure_dir_with_mode(
    host: &dyn Host,
    root: &Path,
    path: &Path,
    mode: Option<u32>,
) -> Result<(), CpackError> {
    let suffix = path
        .strip_prefix(root)
        .map_err(|err| error(err.to_string()))?;
    let mut directory = root.to_owned();
    for part in suffix.components() {
        directory.push(part.as_os_str());
        match host.metadata(&directory, Follow::No) {
            Ok(meta) if meta.kind == FileKind::Dir => {}
            Ok(_) => {
                return Err(error(format!(
                    "Expected a staging directory at {}",
                    directory.display()
                )));
            }
            Err(err) if err.kind == ErrorKind::NotFound => {
                host.create_dir_all(&directory)?;
                if let Some(mode) = mode {
                    host.set_permissions(&directory, mode)?;
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub(crate) fn remove_entry(host: &dyn Host, path: &Path) -> Result<(), CpackError> {
    match host.metadata(path, Follow::No) {
        Ok(meta) if meta.kind == FileKind::Dir => host.remove_dir_all(path)?,
        Ok(_) => host.remove_file(path)?,
        Err(err) if err.kind == ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

pub(crate) fn clear_output_file(host: &dyn Host, path: &Path) -> Result<(), CpackError> {
    match host.metadata(path, Follow::No) {
        Ok(meta) if matches!(meta.kind, FileKind::File | FileKind::Symlink) => {
            host.remove_file(path)?
        }
        Ok(_) => {
            return Err(error(format!(
                "Expected a package file at {}",
                path.display()
            )));
        }
        Err(err) if err.kind == ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}
