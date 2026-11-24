//! Stage installed files through the host and the CMake interpreter.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, hash::HashAlgo, truth};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest};

use crate::config::{
    CpackConfig, Environment, absolute, cmake_executable, interpreter, script, snapshot,
};
use crate::{CPackOptions, ComponentGrouping, CpackError, error, log, safe_name};

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
    install(host, &cwd, &options, &mut interp, staging_dir)
}

pub(crate) fn install(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    interp: &mut Interp,
    staging: &Path,
) -> Result<(), CpackError> {
    let cfg = snapshot(interp);
    let stage_files = cfg.get(b"CPACK_GENERATOR") != Some(b"External")
        || cfg.on(b"CPACK_EXTERNAL_ENABLE_STAGING");
    let clear_destdir = BTreeMap::from([("DESTDIR".into(), None)]);
    let _env = stage_files.then(|| Environment::apply(host.as_ref(), &clear_destdir));
    let cfg = if stage_files {
        let install_scripts = if cfg.get(b"CPACK_INSTALL_SCRIPTS").is_some() {
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
    install_projects(host, cwd, options, &cfg, staging)?;
    run_hooks(
        host.as_ref(),
        cwd,
        interp,
        b"CPACK_PRE_BUILD_SCRIPTS",
        "Executing pre-build script",
    )
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

fn install_projects(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    cfg: &CpackConfig,
    staging: &Path,
) -> Result<(), CpackError> {
    let project_key = b"CPACK_INSTALL_CMAKE_PROJECTS";
    if cfg.get(project_key).is_none_or(<[u8]>::is_empty) {
        return Ok(());
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
        return Ok(());
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
            let destinations = install_destinations(cfg, staging, component, subdir)?;
            for (component, destination) in &destinations {
                if separate {
                    log(host.as_ref(), format!("-   Install component: {component}"));
                }
                let location = InstallLocation::new(cfg, destination, prefix)?;
                ensure_dir(host.as_ref(), staging, &location.directory)?;
                install_project(host, options, cfg, &build, config, component, location)?;
            }
        }
    }
    Ok(())
}

fn install_destinations(
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
    components
        .into_iter()
        .map(|name| {
            let mut destination = root.join(component_dir(&name));
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

fn uses_destdir(cfg: &CpackConfig) -> bool {
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
) -> Result<(), CpackError> {
    let mut interp = interpreter(host, build, options, None)?;
    let _env = location.environment(host.as_ref());
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_INSTALL_PREFIX", location.prefix);
    if !config.is_empty() {
        cx.set_var(b"CMAKE_INSTALL_CONFIG_NAME", config.into());
    }
    if !component.is_empty() && component != b"ALL" {
        cx.set_var(b"CMAKE_INSTALL_COMPONENT", component.into());
    }
    if let Some(permissions) = cfg.get(b"CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS") {
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
    let file = build.join("cmake_install.cmake");
    if interp.run_script(&file).exit_code != 0 {
        return Err(error(format!(
            "Error when running the install script: {}",
            file.display()
        )));
    }
    Ok(())
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

pub(crate) fn grouping(cfg: &CpackConfig) -> ComponentGrouping {
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
    if !cfg.on(b"CPACK_ARCHIVE_COMPONENT_INSTALL")
        && cfg.get(b"CPACK_GENERATOR") != Some(b"External")
    {
        return Vec::new();
    }
    project_components(cfg, selection)
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
    if name.iter().any(|byte| b"<>\"/\\|?*`".contains(byte))
        || (cfg!(windows) && name.contains(&b':'))
    {
        return HashAlgo::Md5.hex_digest(name).into();
    }
    safe_name(name).unwrap_or_else(|_| HashAlgo::Md5.hex_digest(name).into())
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
            Err(err) if err.kind == ErrorKind::NotFound => host.create_dir_all(&directory)?,
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
