//! Native `cpack` for Rcmake (ADR-010).
//!
//! `cpack` reads `CPackConfig.cmake` (or `--config <file>`, or a package preset), stages
//! the install tree of each project into a temporary directory, and runs one
//! [`PackageGenerator`] for each name in `CPACK_GENERATOR`.
//!
//! Modules:
//!
//! - [`cli`]: command-line parsing into [`CPackOptions`].
//! - [`config`]: evaluation of the configuration file with the full command registry.
//! - [`stage`]: install of the projects into the staging directory.
//! - [`archive_gen`]: `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP`.
//! - [`source`]: source packages (`CPackSourceConfig.cmake`).
//! - [`generators`] (`src/gen/`): the generator registry and the `External`, `DragNDrop`, and `Bundle` generators.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rcmake_base::hash::HashAlgo;
use rcmake_base::{BString, Limits, list};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, to_native};
use rcmake_host::{CopyOptions, Host, ReadRange, Stream, WriteMode};

use crate::config::{CpackConfig, Environment, absolute, script, snapshot};

pub mod archive_gen;
pub mod cli;
pub mod config;
// `gen` is a reserved keyword in edition 2024.
#[path = "gen/mod.rs"]
pub mod generators;
pub mod source;
pub mod stage;

#[cfg(test)]
mod tests;

/// Error of a `cpack` operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CpackError {
    /// The operation is not implemented yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    /// A CPack error with the CPack message text.
    #[error("{0}")]
    Message(BString),
    /// A command-line option has no valid value.
    #[error("{0}")]
    Argument(BString),
    /// A host operation failed.
    #[error(transparent)]
    Host(#[from] rcmake_host::HostError),
}

/// Parsed `cpack` command line.
///
/// `cpack` applies `-C` before it runs the configuration file, then `-G`, `-P`, `-R`,
/// `-B`, and `--vendor`, and `-D` last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CPackOptions {
    /// `-G <generators>`: overrides `CPACK_GENERATOR`.
    pub generators: Option<BString>,
    /// `-C <configs>`: `CPACK_BUILD_CONFIG`.
    pub build_config: Option<BString>,
    /// `--config <file>` (default `CPackConfig.cmake`).
    pub config_file: Option<BString>,
    /// `-D <var>=<value>`, in command-line order.
    pub defines: Vec<(BString, BString)>,
    /// `-P <name>`: `CPACK_PACKAGE_NAME`.
    pub package_name: Option<BString>,
    /// `-R <version>`: `CPACK_PACKAGE_VERSION`.
    pub package_version: Option<BString>,
    /// `-B <dir>`: `CPACK_PACKAGE_DIRECTORY`.
    pub package_directory: Option<BString>,
    /// `--vendor <name>`: `CPACK_PACKAGE_VENDOR`.
    pub vendor: Option<BString>,
    /// `--preset <name>`.
    pub preset: Option<BString>,
    /// `--presets-file <file>`.
    pub presets_file: Option<BString>,
    /// `--list-presets`.
    pub list_presets: bool,
    /// `-V` / `--verbose`.
    pub verbose: bool,
    /// `--debug`.
    pub debug: bool,
    /// `--trace`.
    pub trace: bool,
    /// `--trace-expand`.
    pub trace_expand: bool,
    /// Print usage information.
    pub help: bool,
    /// Print the program version.
    pub version: bool,
}

/// How a generator groups install components (`CPACK_COMPONENTS_GROUPING`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComponentGrouping {
    /// `ONE_PER_GROUP`: one package for each component group.
    #[default]
    OnePerGroup,
    /// `ALL_COMPONENTS_IN_ONE`: one package with all components.
    AllComponentsInOne,
    /// `IGNORE`: one package for each component.
    Ignore,
}

/// Everything a [`PackageGenerator`] needs for one package run.
pub struct PackageContext<'a> {
    /// Host for file, process, and clock operations.
    pub host: &'a dyn Host,
    /// `CPACK_*` variables after configuration and overrides.
    pub vars: &'a BTreeMap<BString, BString>,
    /// Staging directory that holds the installed files.
    pub staging_dir: &'a Path,
    /// Directory for package files before CPack publishes them.
    pub output_dir: &'a Path,
    /// Package file name without extension (`CPACK_PACKAGE_FILE_NAME`).
    pub file_name: &'a [u8],
    /// Component grouping.
    pub grouping: ComponentGrouping,
}

/// One CPack generator (`CPACK_GENERATOR` entry).
pub trait PackageGenerator {
    /// Generator name (for example `TGZ` or `DragNDrop`).
    fn name(&self) -> &'static str;

    /// Package file extension, with the leading dot (for example `.tar.gz`).
    fn extension(&self) -> &'static str;

    /// Create the package files from the staging directory. Returns the paths of the
    /// package files.
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] when the package can not be created.
    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError>;
}

/// Run `cpack` with `argv` (including `argv[0]`) and return the process exit code.
#[must_use]
pub fn main(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let args = argv.get(1..).unwrap_or_default();
    match cli::parse(args).and_then(|options| run(&host, options)) {
        Ok(()) => 0,
        Err(err) => {
            let label = if matches!(err, CpackError::Argument(_)) {
                "CMake"
            } else {
                "CPack"
            };
            host.write_out(Stream::Stderr, format!("{label} Error: {err}\n").as_bytes());
            1
        }
    }
}

fn run(host: &Arc<dyn Host>, cli: CPackOptions) -> Result<(), CpackError> {
    if cli::print_information(host.as_ref(), &cli) {
        return Ok(());
    }
    let options = config::resolve_options(host.as_ref(), cli)?;
    if options.cli.list_presets {
        return Ok(());
    }
    if options.cli.debug {
        log(
            host.as_ref(),
            format_args!("{}:{} Enable Debug", file!(), line!()),
        );
    }
    if options.cli.verbose {
        log(host.as_ref(), "Enable Verbose");
    }
    let _environment = Environment::apply(host.as_ref(), &options.environment);
    let mut interp = config::load_base(host, &options.cwd, &options.cli)?;
    let base = snapshot(&mut interp);
    let generators = base.list(b"CPACK_GENERATOR");
    if generators.is_empty() {
        return Err(error("CPack generator not specified"));
    }
    for name in generators {
        if base.get(b"CPACK_PACKAGE_NAME").is_none() {
            return Err(error("CPack project name not specified"));
        }
        let generator = generators::create(&name)
            .ok_or_else(|| error(format!("Could not create CPack generator: {name}")))?;
        interp.cx().set_var(b"CPACK_GENERATOR", name);
        if let Some(project) = base
            .get(b"CPACK_PROJECT_CONFIG_FILE")
            .filter(|path| !path.is_empty())
        {
            let path = absolute(&options.cwd, project);
            if host.metadata(&path, rcmake_host::Follow::Yes).is_ok() {
                script(&mut interp, &path)?;
            } else {
                host.write_out(
                    Stream::Stderr,
                    format!(
                        "CPack Warning: CPACK_PROJECT_CONFIG_FILE not found: {}\n",
                        path.display()
                    )
                    .as_bytes(),
                );
            }
        }
        complete_version(&mut interp)?;
        if generator.name() == "External" {
            generators::external::initialize(&mut interp)?;
        }
        if let Err(err) = package(
            host,
            &options.cwd,
            &options.cli,
            &mut interp,
            generator.as_ref(),
        ) {
            let package_name = interp.cx().var_or_empty(b"CPACK_PACKAGE_NAME");
            return Err(error(format!(
                "{err}\nCPack Error: Error when generating package: {package_name}",
            )));
        }
        let updated = snapshot(&mut interp);
        let mut cx = interp.cx();
        for name in updated
            .vars
            .keys()
            .filter(|name| !base.vars.contains_key(*name))
        {
            cx.unset_var(name);
        }
        for (name, value) in &base.vars {
            cx.set_var(name, value.clone());
        }
    }
    Ok(())
}

fn complete_version(interp: &mut Interp) -> Result<(), CpackError> {
    let mut cx = interp.cx();
    if cx.var_defined(b"CPACK_PACKAGE_VERSION") {
        return Ok(());
    }
    let parts = [
        b"CPACK_PACKAGE_VERSION_MAJOR".as_slice(),
        b"CPACK_PACKAGE_VERSION_MINOR",
        b"CPACK_PACKAGE_VERSION_PATCH",
    ];
    let version = parts.map(|name| cx.var(name));
    if let [Some(major), Some(minor), Some(patch)] = version {
        cx.set_var(
            b"CPACK_PACKAGE_VERSION",
            format!("{major}.{minor}.{patch}").into(),
        );
        return Ok(());
    }
    Err(error(
        "CPack project version not specified\nSpecify CPACK_PACKAGE_VERSION, or CPACK_PACKAGE_VERSION_MAJOR, CPACK_PACKAGE_VERSION_MINOR, and CPACK_PACKAGE_VERSION_PATCH.",
    ))
}

fn package(
    host: &Arc<dyn Host>,
    cwd: &Path,
    cli: &CPackOptions,
    interp: &mut Interp,
    generator: &dyn PackageGenerator,
) -> Result<(), CpackError> {
    let cfg = snapshot(interp);
    if [
        b"CPACK_INSTALL_CMAKE_PROJECTS".as_slice(),
        b"CPACK_INSTALLED_DIRECTORIES",
        b"CPACK_INSTALL_SCRIPTS",
        b"CPACK_INSTALL_SCRIPT",
    ]
    .into_iter()
    .all(|name| cfg.get(name).is_none())
    {
        return Err(error(
            "Please specify build tree of the project that uses CMake using CPACK_INSTALL_CMAKE_PROJECTS, specify CPACK_INSTALL_COMMANDS, CPACK_INSTALL_SCRIPT, or CPACK_INSTALLED_DIRECTORIES.",
        ));
    }
    let name = cfg
        .get(b"CPACK_PACKAGE_FILE_NAME")
        .ok_or_else(|| error("CPACK_PACKAGE_FILE_NAME not specified"))?;
    let file = safe_name(name)?;
    let output = cfg
        .get(b"CPACK_PACKAGE_DIRECTORY")
        .map_or_else(|| cwd.to_owned(), |dir| absolute(cwd, dir));
    let mut work = output.join("_CPack_Packages");
    if let Some(tag) = cfg.get(b"CPACK_TOPLEVEL_TAG").filter(|tag| !tag.is_empty()) {
        work.push(stage::relative_install_path(tag)?);
    }
    work.push(generator.name());
    host.create_dir_all(&output)?;
    stage::ensure_dir(host.as_ref(), &output, &work)?;
    if generator.name() == "External"
        && (cfg.get(b"CPACK_REMOVE_TOPLEVEL_DIRECTORY").is_none()
            || cfg.on(b"CPACK_REMOVE_TOPLEVEL_DIRECTORY"))
    {
        host.remove_dir_all(&work)?;
    }
    let staging = work.join(file);
    stage::remove_entry(host.as_ref(), &staging)?;
    host.create_dir_all(&staging)?;
    let package_file = work.join(safe_name(
        &[name, generator.extension().as_bytes()].concat(),
    )?);
    for (key, path) in [
        (b"CPACK_TOPLEVEL_DIRECTORY".as_slice(), &work),
        (b"CPACK_TEMPORARY_DIRECTORY", &staging),
        (b"CPACK_TEMPORARY_INSTALL_DIRECTORY", &staging),
        (b"CPACK_OUTPUT_FILE_PREFIX", &output),
        (b"CPACK_TEMPORARY_PACKAGE_FILE_NAME", &package_file),
    ] {
        interp.cx().set_var(key, from_native(path));
    }
    let output_file_name = package_file
        .file_name()
        .ok_or_else(|| error("No package file name"))?;
    interp.cx().set_var(
        b"CPACK_OUTPUT_FILE_NAME",
        from_native(Path::new(output_file_name)),
    );
    interp.cx().set_var(
        b"CPACK_OUTPUT_FILE_PATH",
        from_native(&output.join(output_file_name)),
    );
    log(
        host.as_ref(),
        format!("Create package using {}", generator.name()),
    );
    log(host.as_ref(), "Install projects");
    if cli.verbose || cli.debug {
        let level = if cli.debug { "Debug" } else { "Verbose" };
        host.write_out(
            Stream::Stdout,
            format!("CPack {level}: Staging files in {}\n", staging.display()).as_bytes(),
        );
    }
    stage::install(host, cwd, cli, interp, &staging)?;
    let vars = snapshot(interp);
    log(host.as_ref(), "Create package");
    let mut packages = generator.package(&PackageContext {
        host: host.as_ref(),
        vars: &vars.vars,
        staging_dir: &staging,
        output_dir: &work,
        file_name: name,
        grouping: stage::grouping(&vars),
    })?;
    if generator.name() == "External" {
        generators::external::run_script(host.as_ref(), cwd, interp, &mut packages)?;
    }
    interp.cx().set_var(
        b"CPACK_PACKAGE_FILES",
        list::join(packages.iter().map(|path| from_native(path))),
    );
    stage::run_hooks(
        host.as_ref(),
        cwd,
        interp,
        b"CPACK_POST_BUILD_SCRIPTS",
        "Executing post-build script",
    )?;
    publish(host.as_ref(), &snapshot(interp), &output, &packages)
}

fn publish(
    host: &dyn Host,
    cfg: &CpackConfig,
    output: &Path,
    packages: &[PathBuf],
) -> Result<(), CpackError> {
    let algorithms = cfg.list(b"CPACK_PACKAGE_CHECKSUM");
    let algorithms = algorithms
        .iter()
        .map(|name| {
            HashAlgo::parse(name)
                .map(|algo| (name.to_ascii_lowercase(), algo))
                .ok_or_else(|| error(format!("Cannot recognize algorithm: {name}")))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    for package in packages {
        let name = package
            .file_name()
            .ok_or_else(|| error("No package file name"))?;
        let path = output.join(name);
        if package != &path {
            stage::clear_output_file(host, &path)?;
            host.copy_file(
                package,
                &path,
                CopyOptions {
                    preserve_permissions: true,
                    preserve_mtime: true,
                },
            )?;
        }
        log(host, format!("- package: {} generated.", path.display()));
        if !algorithms.is_empty() {
            let bytes = host.read(
                package,
                ReadRange::All {
                    limit: Limits::default().max_file_read,
                },
            )?;
            let file = from_native(Path::new(name));
            for (extension, algo) in &algorithms {
                let checksum_name = [file.as_slice(), b".", extension].concat();
                let checksum = output.join(safe_name(&checksum_name)?);
                let text = format!("{}  {file}\n", algo.hex_digest(&bytes));
                stage::clear_output_file(host, &checksum)?;
                host.write(&checksum, text.as_bytes(), WriteMode::CreateNew)?;
                log(
                    host,
                    format!("- checksum file: {} generated.", checksum.display()),
                );
            }
        }
    }
    Ok(())
}

fn safe_name(value: &[u8]) -> Result<PathBuf, CpackError> {
    let path = to_native(value);
    if !value.iter().any(|byte| matches!(byte, b'/' | b'\\' | 0))
        && matches!(path.components().next(), Some(Component::Normal(_)))
    {
        Ok(path)
    } else {
        Err(error(format!(
            "Invalid CPack file name: {}",
            BString::from(value)
        )))
    }
}

fn error(message: impl Into<BString>) -> CpackError {
    CpackError::Message(message.into())
}

fn log(host: &dyn Host, message: impl std::fmt::Display) {
    host.write_out(Stream::Stdout, format!("CPack: {message}\n").as_bytes());
}
