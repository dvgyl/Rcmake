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
        interp
            .cx()
            .set_var(b"CMAKE_INSTALL_PREFIX", from_native(staging));
        let install_scripts = if cfg.get(b"CPACK_INSTALL_SCRIPTS").is_some() {
            b"CPACK_INSTALL_SCRIPTS".as_slice()
        } else {
            b"CPACK_INSTALL_SCRIPT"
        };
        run_hooks(
            host.as_ref(),
            cwd,
            interp,
            install_scripts,
            "- Install script",
        )?;
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
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    if !projects.is_empty() && cfg.get(b"CPACK_CMAKE_GENERATOR").is_none() {
        return Err(error(
            "CPACK_INSTALL_CMAKE_PROJECTS is specified, but CPACK_CMAKE_GENERATOR is not. CPACK_CMAKE_GENERATOR is required to install the project.",
        ));
    }
    let (tuples, remainder) = projects.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(error(
            "Not enough items on list: CPACK_INSTALL_CMAKE_PROJECTS. CPACK_INSTALL_CMAKE_PROJECTS should hold quadruplet of install directory, install project name, install component, and install subdirectory.",
        ));
    }
    let mut configs = cfg.list(b"CPACK_BUILD_CONFIG");
    configs.sort();
    configs.dedup();
    if configs.is_empty() {
        configs.push(BString::default());
    }
    let external = cfg.get(b"CPACK_GENERATOR") == Some(b"External");
    let stage_project = !external || cfg.on(b"CPACK_EXTERNAL_ENABLE_STAGING");
    if !tuples.is_empty() && cfg.get(b"CPACK_CMAKE_GENERATOR") == Some(b"") {
        return Err(error(
            "Specified package generator not found. CPACK_CMAKE_GENERATOR value is invalid.",
        ));
    }
    for [build, project, component, subdir] in tuples {
        let build = absolute(cwd, build);
        for config in &configs {
            if stage_project {
                preinstall(host.as_ref(), cfg, &build, config)?;
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
                ensure_dir(host.as_ref(), staging, destination)?;
                install_project(host, options, cfg, &build, config, component, destination)?;
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
    let subdir = relative_install_path(subdir)?;
    let prefix = relative_install_path(
        cfg.get(b"CPACK_PACKAGING_INSTALL_PREFIX")
            .unwrap_or_default(),
    )?;
    let components = package_components(cfg, component);
    if components.is_empty() {
        return Ok(vec![(component.clone(), staging.join(subdir).join(prefix))]);
    }
    components
        .into_iter()
        .map(|name| {
            let mut destination = staging.join(component_dir(&name));
            if cfg.on(b"CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
                let file_name = safe_name(cfg.get(b"CPACK_PACKAGE_FILE_NAME").unwrap_or_default())?;
                destination.push(file_name);
            }
            Ok((name, destination.join(&subdir).join(&prefix)))
        })
        .collect()
}

fn install_project(
    host: &Arc<dyn Host>,
    options: &CPackOptions,
    cfg: &CpackConfig,
    build: &Path,
    config: &[u8],
    component: &[u8],
    destination: &Path,
) -> Result<(), CpackError> {
    let mut interp = interpreter(host, build, options, None)?;
    let destdir = cfg.on(b"CPACK_SET_DESTDIR");
    let prefix = if destdir {
        cfg.get(b"CPACK_INSTALL_PREFIX").unwrap_or(b"/").into()
    } else {
        from_native(destination)
    };
    if destdir {
        relative_install_path(&prefix)?;
    }
    let env = BTreeMap::from([("DESTDIR".into(), destdir.then(|| from_native(destination)))]);
    let _env = Environment::apply(host.as_ref(), &env);
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_INSTALL_PREFIX", prefix);
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

fn preinstall(
    host: &dyn Host,
    cfg: &CpackConfig,
    build: &Path,
    config: &[u8],
) -> Result<(), CpackError> {
    if cfg.on(b"CPACK_SKIP_PREINSTALL")
        || !matches!(
            cfg.get(b"CPACK_CMAKE_GENERATOR"),
            Some(
                b"Unix Makefiles"
                    | b"MSYS Makefiles"
                    | b"MinGW Makefiles"
                    | b"NMake Makefiles"
                    | b"NMake Makefiles JOM"
                    | b"Watcom WMake"
                    | b"Borland Makefiles"
            )
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
