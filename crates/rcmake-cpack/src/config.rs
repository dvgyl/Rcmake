//! Read CPack configuration scripts and apply command-line settings.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rcmake_base::list::{self, Empty};
use rcmake_base::{BString, truth};
use rcmake_eval::{
    BuiltinFn, Flow, IncludeOptions, Interp, InterpConfig, Registry, RunMode, TraceConfig,
};
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_host::{FileKind, Follow, Host, Stream};
use rcmake_presets::{EnvSnapshot, PresetKind, PresetSettings};

use crate::{CPackOptions, CpackError, error};

/// Variables from a CPack configuration script and its overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpackConfig {
    /// Variables by name.
    pub vars: BTreeMap<BString, BString>,
}

impl CpackConfig {
    pub(crate) fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.vars.get(name).map(AsRef::<[u8]>::as_ref)
    }

    pub(crate) fn list(&self, name: &[u8]) -> Vec<BString> {
        list::split(self.get(name).unwrap_or_default(), Empty::Drop)
    }

    pub(crate) fn on(&self, name: &[u8]) -> bool {
        self.get(name).is_some_and(truth::is_on)
    }
}

/// Run the selected configuration file and apply command-line overrides.
///
/// # Errors
///
/// Returns an error if the preset or configuration cannot be loaded.
pub fn load(host: &Arc<dyn Host>, options: &CPackOptions) -> Result<CpackConfig, CpackError> {
    let options = resolve_options(host.as_ref(), options.clone())?;
    let _environment = Environment::apply(host.as_ref(), &options.environment);
    let mut interp = load_base(host, &options.cwd, &options.cli, None)?;
    Ok(snapshot(&mut interp))
}

pub(crate) struct ResolvedOptions {
    pub(crate) cwd: PathBuf,
    pub(crate) cli: CPackOptions,
    pub(crate) environment: BTreeMap<BString, Option<BString>>,
}

pub(crate) fn resolve_options(
    host: &dyn Host,
    mut cli: CPackOptions,
) -> Result<ResolvedOptions, CpackError> {
    let mut cwd = host.initial_cwd();
    let mut environment = BTreeMap::new();
    if cli.list_presets || cli.preset.is_some() {
        let presets = match &cli.presets_file {
            Some(file) => rcmake_presets::load_file(&cwd, &absolute(&cwd, file), host),
            None => rcmake_presets::load(&cwd, host),
        }
        .map_err(|err| {
            if err.message.starts_with("Could not read presets from ") {
                error(err.message)
            } else {
                error(format!(
                    "Could not read presets from {}:\n{err}",
                    cwd.display()
                ))
            }
        })?;
        let env = EnvSnapshot::from_host(host);
        if cli.list_presets {
            let mut visible = presets.list_visible(PresetKind::Package, &env);
            visible.retain(|preset| {
                presets
                    .resolve(PresetKind::Package, &preset.name, &env)
                    .is_ok_and(|resolved| match resolved.settings {
                        PresetSettings::Package(settings) => {
                            settings.generators.is_none_or(|names| {
                                names
                                    .iter()
                                    .all(|name| crate::generators::create(name).is_some())
                            })
                        }
                        _ => false,
                    })
            });
            if !visible.is_empty() {
                host.write_out(Stream::Stdout, b"Available package presets:\n\n");
                let width = visible
                    .iter()
                    .map(|preset| preset.name.len())
                    .max()
                    .unwrap_or(0);
                for preset in visible {
                    let name = format!("\"{}\"", preset.name);
                    let line = match preset.display_name {
                        Some(display) => {
                            format!("  {name:width$} - {display}\n", width = width + 2)
                        }
                        None => format!("  {name}\n"),
                    };
                    host.write_out(Stream::Stdout, line.as_bytes());
                }
            }
        } else if let Some(name) = &cli.preset {
            let preset = presets
                .resolve(PresetKind::Package, name, &env)
                .map_err(|err| error(err.to_string()))?;
            if let Some(dir) = preset.binary_dir {
                cwd = absolute(&cwd, &dir);
            }
            environment = preset.environment;
            if let PresetSettings::Package(settings) = preset.settings {
                // An empty value on either side counts as absent.
                let choose = |given: Option<BString>, preset: Option<BString>| {
                    given
                        .filter(|value| !value.is_empty())
                        .or(preset.filter(|value| !value.is_empty()))
                };
                cli.config_file = choose(cli.config_file.take(), settings.config_file);
                cli.generators = choose(
                    cli.generators.take(),
                    settings.generators.as_ref().map(list::join),
                );
                cli.build_config = choose(
                    cli.build_config.take(),
                    settings.configurations.as_ref().map(list::join),
                );
                cli.package_name = choose(cli.package_name.take(), settings.package_name);
                cli.package_version = choose(cli.package_version.take(), settings.package_version);
                cli.package_directory =
                    choose(cli.package_directory.take(), settings.package_directory);
                cli.vendor = choose(cli.vendor.take(), settings.vendor_name);
                let mut defines: Vec<_> =
                    settings.variables.unwrap_or_default().into_iter().collect();
                defines.append(&mut cli.defines);
                cli.defines = defines;
                if let Some(output) = settings.output {
                    cli.verbose |= output.verbose == Some(true);
                    cli.debug |= output.debug == Some(true);
                }
            }
        }
    }
    Ok(ResolvedOptions {
        cwd,
        cli,
        environment,
    })
}

pub(crate) struct Environment<'a> {
    host: &'a dyn Host,
    previous: Vec<(BString, Option<BString>)>,
}

impl<'a> Environment<'a> {
    pub(crate) fn apply(host: &'a dyn Host, vars: &BTreeMap<BString, Option<BString>>) -> Self {
        let previous = vars
            .iter()
            .map(|(name, value)| {
                let previous = host.env_get(name);
                host.env_set(name, value.as_ref().map(AsRef::<[u8]>::as_ref));
                (name.clone(), previous)
            })
            .collect();
        Self { host, previous }
    }
}

impl Drop for Environment<'_> {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            self.host
                .env_set(name, value.as_ref().map(AsRef::<[u8]>::as_ref));
        }
    }
}

pub(crate) fn absolute(cwd: &Path, value: &[u8]) -> PathBuf {
    let joined = cwd.join(to_native(value));
    let mut path = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir => {}
            _ => path.push(part),
        }
    }
    path
}

pub(crate) fn cmake_executable(host: &dyn Host) -> Result<PathBuf, CpackError> {
    let exe = host
        .current_exe()
        .map_err(|err| error(format!("Cannot find the CPack executable: {err}")))?;
    let bin = exe
        .parent()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| error("Cannot find the directory of the CPack executable"))?;
    Ok(bin.join(if cfg!(windows) { "cmake.exe" } else { "cmake" }))
}

fn interpreter(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    argv0: Option<&BString>,
) -> Result<Interp, CpackError> {
    let cmake_command = cmake_executable(host.as_ref())?;
    let (_, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv0);
    let mut interp = Interp::new(
        InterpConfig::new(
            Arc::clone(host),
            RunMode::Script,
            cmake_root.unwrap_or_default(),
            cmake_command,
            cwd.to_owned(),
        ),
        register_cpack_commands,
    );
    if options.trace || options.trace_expand {
        interp.set_trace_config(Some(TraceConfig {
            expand: options.trace_expand,
            ..Default::default()
        }));
    }
    Ok(interp)
}

fn register_cpack_commands(registry: &mut Registry) {
    rcmake_commands::register_all(registry);

    static GET_PROPERTY: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((builtin, flags)) = registry.get(b"get_property") {
        GET_PROPERTY.get_or_init(|| builtin);
        registry.add(
            "get_property",
            |cx, args| {
                if let [output, scope, property, name] = args
                    && scope.value == b"GLOBAL"
                    && property.value == b"PROPERTY"
                    && name.value == b"CMAKE_ROLE"
                {
                    cx.set_var(&output.value, "CPACK".into());
                    return Ok(Flow::Normal);
                }
                GET_PROPERTY
                    .get()
                    .map_or(Ok(Flow::Normal), |builtin| builtin(cx, args))
            },
            flags,
        );
    }

    static GET_CMAKE_PROPERTY: OnceLock<BuiltinFn> = OnceLock::new();
    if let Some((builtin, flags)) = registry.get(b"get_cmake_property") {
        GET_CMAKE_PROPERTY.get_or_init(|| builtin);
        registry.add(
            "get_cmake_property",
            |cx, args| {
                if let [output, name] = args
                    && name.value == b"CMAKE_ROLE"
                {
                    cx.set_var(&output.value, "CPACK".into());
                    return Ok(Flow::Normal);
                }
                GET_CMAKE_PROPERTY
                    .get()
                    .map_or(Ok(Flow::Normal), |builtin| builtin(cx, args))
            },
            flags,
        );
    }
}

pub(crate) fn load_base(
    host: &Arc<dyn Host>,
    cwd: &Path,
    options: &CPackOptions,
    argv0: Option<&BString>,
) -> Result<Interp, CpackError> {
    let mut interp = interpreter(host, cwd, options, argv0)?;
    // The generated config can use this value when it includes CPackProperties.cmake.
    if let Some(value) = &options.build_config {
        interp.cx().set_var(b"CPACK_BUILD_CONFIG", value.clone());
    }
    let config = absolute(
        cwd,
        options
            .config_file
            .as_ref()
            .map_or(b"CPackConfig.cmake", AsRef::<[u8]>::as_ref),
    );
    match host.metadata(&config, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => {
            if interp.run_script(&config).exit_code != 0 {
                return Err(error(format!(
                    "Problem reading CPack config file: \"{}\"",
                    from_native_cmake(&config)
                )));
            }
        }
        Err(err) if err.kind == ErrorKind::NotFound && options.config_file.is_none() => {}
        _ => {
            return Err(error(format!(
                "Cannot find CPack config file: \"{}\"",
                from_native_cmake(&config)
            )));
        }
    }
    for (name, value) in [
        (b"CPACK_GENERATOR".as_slice(), &options.generators),
        (b"CPACK_PACKAGE_NAME", &options.package_name),
        (b"CPACK_PACKAGE_VERSION", &options.package_version),
        (b"CPACK_PACKAGE_VENDOR", &options.vendor),
    ] {
        if let Some(value) = value {
            interp.cx().set_var(name, value.clone());
        }
    }
    let output_dir = options
        .package_directory
        .clone()
        .or_else(|| interp.cx().var(b"CPACK_PACKAGE_DIRECTORY"));
    let output_dir = output_dir.map_or_else(|| cwd.to_owned(), |dir| absolute(cwd, &dir));
    interp
        .cx()
        .set_var(b"CPACK_PACKAGE_DIRECTORY", from_native_cmake(&output_dir));
    for (name, value) in &options.defines {
        interp.cx().set_var(name, value.clone());
    }
    if let Some(path) = interp.cx().var(b"CPACK_MODULE_PATH") {
        interp.cx().set_var(b"CMAKE_MODULE_PATH", path);
    }
    Ok(interp)
}

pub(crate) fn snapshot(interp: &mut Interp) -> CpackConfig {
    let cx = interp.cx();
    let vars = cx
        .variable_names()
        .into_iter()
        .filter(|name| cx.var_defined(name))
        .map(|name| {
            let value = cx.var_or_empty(&name);
            (name, value)
        })
        .collect();
    CpackConfig { vars }
}

pub(crate) fn script(interp: &mut Interp, file: &Path) -> Result<(), CpackError> {
    if interp
        .cx()
        .include_file(
            file,
            IncludeOptions {
                no_policy_scope: true,
                ..Default::default()
            },
        )
        .is_err()
        || interp.exit_code() != 0
    {
        return Err(error(format!("Error processing file: {}", file.display())));
    }
    Ok(())
}
