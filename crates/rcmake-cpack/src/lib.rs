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
//! - [`source`]: source packages (`CPackSourceConfig.cmake`) and staged-tree containment.
//! - [`generators`] (`src/gen/`): generator registry, Linux (`DEB`, `RPM`), External,
//!   macOS (`DragNDrop`, `Bundle`, `productbuild`), Windows (`NSIS`, `NSIS64`, `WIX`),
//!   and cross-platform (`IFW`) package generators.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::hash::HashAlgo;
use rcmake_base::{BString, ByteSlice, list};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake, to_native};
use rcmake_host::{FileKind, Follow, Host, HostResult, ReadRange, Stream, WriteMode};

use crate::config::{CpackConfig, Environment, absolute, script, snapshot};

pub mod archive_gen;
pub mod cli;
pub mod config;
// `gen` is a reserved keyword in edition 2024.
#[path = "gen/mod.rs"]
pub mod generators;
pub mod source;
pub mod stage;

/// The `cpack --help` text.
pub const USAGE: &[u8] = include_bytes!("help.txt");

#[cfg(test)]
mod tests;

/// Error of a `cpack` operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CpackError {
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

/// One staged destination root and the member paths recorded beneath it before pre-build hooks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StagedRoot {
    /// Destination directory created for this component (`<staging>/<subdir>/<comp>[/<top>]`).
    pub(crate) root: PathBuf,
    /// Directory scanned for membership (`root` in `DESTDIR` mode, otherwise `root/<prefix>`).
    pub(crate) scanned: PathBuf,
    /// Canonical path of `root` captured before pre-build hooks run.
    pub(crate) canon_root: PathBuf,
    /// Canonical path of `scanned` captured before pre-build hooks run.
    pub(crate) canon_scanned: PathBuf,
    /// Paths added below `scanned` during project installation, in install order.
    pub(crate) files: Vec<PathBuf>,
}

/// Pre-hook canonical paths for the generator work and staging roots.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CanonicalRoots<'a> {
    pub(crate) work: &'a Path,
    pub(crate) staging: &'a Path,
}

/// One component that the project installs staged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledComponent {
    /// Component name.
    pub name: BString,
    /// Component group (`CPACK_COMPONENT_<NAME>_GROUP`) when the component was installed.
    pub group: Option<BString>,
    /// Paths that the installs of this component added before the pre-build scripts ran,
    /// in install order. Each path is relative to the install prefix directory of the
    /// component, or to its `DESTDIR` directory when `DESTDIR` staging is on.
    pub files: Vec<PathBuf>,
    /// Actual install roots and per-root relative memberships derived by `stage::install`.
    pub(crate) staged_roots: Vec<StagedRoot>,
}

/// Components that the project installs staged, in install order.
pub type ComponentMembers = Vec<InstalledComponent>;

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
    /// Canonical work and staging roots captured before pre-build hooks ran.
    pub(crate) canonical_roots: Option<CanonicalRoots<'a>>,
}

impl PackageContext<'_> {
    /// Verify `output_dir` and return its expected canonical root.
    pub(crate) fn expected_work_root(&self) -> Result<PathBuf, CpackError> {
        match self.canonical_roots {
            Some(roots) => source::verify_expected_root(self.host, self.output_dir, roots.work),
            None => source::canonical_checked_root(self.host, self.output_dir),
        }
    }

    /// Verify `staging_dir` and return its expected canonical root.
    pub(crate) fn expected_staging_root(&self) -> Result<PathBuf, CpackError> {
        match self.canonical_roots {
            Some(roots) => source::verify_expected_root(self.host, self.staging_dir, roots.staging),
            None => source::canonical_checked_root(self.host, self.staging_dir),
        }
    }
}

/// One CPack generator (`CPACK_GENERATOR` entry).
pub trait PackageGenerator {
    /// Generator name (for example `TGZ` or `DragNDrop`).
    fn name(&self) -> &'static str;

    /// Package file extension, with the leading dot (for example `.tar.gz`).
    fn extension(&self) -> &'static str;

    /// Prepare generator-specific configuration defaults and discover required external tools
    /// after `CPACK_PROJECT_CONFIG_FILE` and version completion have run.
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] when required configuration or external tooling is invalid.
    fn prepare_config(
        &self,
        _host: &dyn Host,
        _interp: &mut Interp,
        _cwd: &Path,
    ) -> Result<(), CpackError> {
        Ok(())
    }

    /// Compute the package file extension for the active configuration and host.
    ///
    /// The default implementation returns [`PackageGenerator::extension`].
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] if a configured extension is invalid.
    fn output_extension_for(
        &self,
        _cfg: &CpackConfig,
        _host: &dyn Host,
    ) -> Result<BString, CpackError> {
        Ok(self.extension().into())
    }

    /// Create the package files from the staging directory. Returns the paths of the
    /// package files.
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] when the package can not be created.
    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError>;

    /// Create the package files from the staging directory and the component members
    /// that the install recorded. Returns the paths of the package files.
    ///
    /// The default implementation ignores `members` and calls [`PackageGenerator::package`].
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] when the package can not be created.
    fn package_from_stage(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let _ = members;
        self.package(cx)
    }

    /// Create packages with access to the CPack interpreter.
    ///
    /// # Errors
    ///
    /// Returns an error if staging data or a generator script is invalid.
    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
        _interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        self.package_from_stage(cx, members)
    }
}

type GeneratorFactory = fn(&[u8], &dyn Host) -> Option<Box<dyn PackageGenerator>>;

/// Run `cpack` with `argv` (including `argv[0]`) and return the process exit code.
#[must_use]
pub fn main(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    main_with_factory(argv, host, generators::create_for_host)
}

pub(crate) fn main_with_factory(
    argv: &[BString],
    host: Arc<dyn Host>,
    factory: GeneratorFactory,
) -> i32 {
    let args = argv.get(1..).unwrap_or_default();
    match cli::parse(args)
        .and_then(|options| run_with_factory(&host, options, argv.first(), factory))
    {
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

fn run_with_factory(
    host: &Arc<dyn Host>,
    cli: CPackOptions,
    argv0: Option<&BString>,
    factory: GeneratorFactory,
) -> Result<(), CpackError> {
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
    let mut interp = config::load_base(host, &options.cwd, &options.cli, argv0)?;
    let base = snapshot(&mut interp);
    let generators = base.list(b"CPACK_GENERATOR");
    if generators.is_empty() {
        return Err(error("CPack generator not specified"));
    }
    for name in generators {
        if base.get(b"CPACK_PACKAGE_NAME").is_none() {
            return Err(error("CPack project name not specified"));
        }
        let generator = factory(&name, host.as_ref()).ok_or_else(|| {
            error(format!(
                "Could not create CPack generator: {name}\n\n{}",
                generators::documentation()
            ))
        })?;
        interp.cx().set_var(b"CPACK_GENERATOR", name);
        if let Some(project) = base
            .get(b"CPACK_PROJECT_CONFIG_FILE")
            .filter(|path| !path.is_empty())
        {
            let path = absolute(&options.cwd, project);
            if host.metadata(&path, Follow::Yes).is_ok() {
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
        if matches!(generator.name(), "DEB" | "RPM") {
            initialize_linux_package(&mut interp, generator.name(), &options.cwd)?;
        }
        if generator.name() == "External" {
            generators::external::initialize(&mut interp)?;
        }
        if let Err(err) = generator
            .prepare_config(host.as_ref(), &mut interp, &options.cwd)
            .and_then(|()| {
                package(
                    host,
                    &options.cwd,
                    &options.cli,
                    &mut interp,
                    generator.as_ref(),
                )
            })
        {
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
    let extension = generator.output_extension_for(&cfg, host.as_ref())?;
    let final_package_name = safe_name(&[name, extension.as_slice()].concat())?;
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
    if matches!(generator.name(), "External" | "DEB" | "RPM")
        && (cfg.get(b"CPACK_REMOVE_TOPLEVEL_DIRECTORY").is_none()
            || cfg.on(b"CPACK_REMOVE_TOPLEVEL_DIRECTORY"))
    {
        host.remove_dir_all(&work)?;
    }
    let publish_dir = cfg
        .get(b"CPACK_OUTPUT_FILE_PREFIX")
        .map_or_else(|| output.clone(), |dir| absolute(cwd, dir));
    let staging = work.join(file);
    stage::remove_entry(host.as_ref(), &staging)?;
    host.create_dir_all(&staging)?;
    let canon_work = source::canonical_checked_root(host.as_ref(), &work)?;
    let canon_staging = source::canonical_checked_root(host.as_ref(), &staging)?;
    if !canon_staging.starts_with(&canon_work) {
        return Err(error(format!(
            "CPack staging directory escapes work directory: {}",
            staging.display()
        )));
    }
    let package_file = work.join(final_package_name);
    for (key, path) in [
        (b"CPACK_TOPLEVEL_DIRECTORY".as_slice(), &work),
        (b"CPACK_TEMPORARY_DIRECTORY", &staging),
        (b"CPACK_TEMPORARY_INSTALL_DIRECTORY", &staging),
        (b"CPACK_OUTPUT_FILE_PREFIX", &publish_dir),
        (b"CPACK_TEMPORARY_PACKAGE_FILE_NAME", &package_file),
    ] {
        interp.cx().set_var(key, from_native_cmake(path));
    }
    let output_file_name = package_file
        .file_name()
        .ok_or_else(|| error("No package file name"))?;
    interp.cx().set_var(
        b"CPACK_OUTPUT_FILE_NAME",
        from_native_cmake(Path::new(output_file_name)),
    );
    interp.cx().set_var(
        b"CPACK_OUTPUT_FILE_PATH",
        from_native_cmake(&output.join(output_file_name)),
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
    let members = stage::install(host, cwd, cli, interp, &staging, &canon_staging)?;
    source::verify_expected_root(host.as_ref(), &work, &canon_work)?;
    source::verify_expected_root(host.as_ref(), &staging, &canon_staging)?;
    let vars = snapshot(interp);
    log(host.as_ref(), "Create package");
    let mut packages = generator.package_with_interp(
        &PackageContext {
            host: host.as_ref(),
            vars: &vars.vars,
            staging_dir: &staging,
            output_dir: &work,
            file_name: name,
            grouping: stage::grouping(&vars),
            canonical_roots: Some(CanonicalRoots {
                work: &canon_work,
                staging: &canon_staging,
            }),
        },
        &members,
        interp,
    )?;
    if generator.name() == "External" {
        generators::external::run_script(host.as_ref(), cwd, interp, &mut packages)?;
    }
    let work_check =
        (generator.name() != "External").then_some((work.as_path(), canon_work.as_path()));
    source::verify_expected_root(host.as_ref(), &work, &canon_work)?;
    validate_generated_packages(
        host.as_ref(),
        &packages,
        work_check,
        generator.name() != "External",
    )?;
    interp.cx().set_var(
        b"CPACK_PACKAGE_FILES",
        list::join(packages.iter().map(|path| from_native_cmake(path))),
    );
    stage::run_hooks(
        host.as_ref(),
        cwd,
        interp,
        b"CPACK_POST_BUILD_SCRIPTS",
        "Executing post-build script",
    )?;
    source::verify_expected_root(host.as_ref(), &work, &canon_work)?;
    validate_generated_packages(
        host.as_ref(),
        &packages,
        work_check,
        generator.name() != "External",
    )?;
    let vars = snapshot(interp);
    let publish_dir = vars
        .get(b"CPACK_OUTPUT_FILE_PREFIX")
        .map_or(publish_dir, |dir| absolute(cwd, dir));
    host.create_dir_all(&publish_dir)?;
    publish(host.as_ref(), &vars, &publish_dir, &packages)
}

fn validate_generated_packages(
    host: &dyn Host,
    packages: &[PathBuf],
    expected_work_root: Option<(&Path, &Path)>,
    disallow_symlinks: bool,
) -> Result<(), CpackError> {
    if let Some((work, canon_work)) = expected_work_root {
        source::verify_expected_root(host, work, canon_work)?;
    }
    for package in packages {
        let name = package
            .file_name()
            .ok_or_else(|| error("No package file name"))?;
        let _ = safe_name(&from_native(Path::new(name)))?;
        let follow = if disallow_symlinks {
            Follow::No
        } else {
            Follow::Yes
        };
        let meta = host.metadata_fresh(package, follow).map_err(|err| {
            error(format!(
                "Expected a package file at {}: {err}",
                package.display()
            ))
        })?;
        if meta.kind != FileKind::File {
            return Err(error(format!(
                "Expected a regular package file at {}",
                package.display()
            )));
        }
        if let Some((work, canon_work)) = expected_work_root {
            let rel = package.strip_prefix(work).map_err(|_| {
                error(format!(
                    "Generated package path escapes work directory: {}",
                    package.display()
                ))
            })?;
            source::verify_contained_path(
                host,
                work,
                canon_work,
                rel,
                source::SymlinkPolicy::Reject,
                false,
            )?;
        }
    }
    Ok(())
}

fn initialize_linux_package(
    interp: &mut Interp,
    generator: &str,
    cwd: &Path,
) -> Result<(), CpackError> {
    let mut cx = interp.cx();
    if cx.var(b"CPACK_PACKAGING_INSTALL_PREFIX").is_none() {
        cx.set_var(b"CPACK_PACKAGING_INSTALL_PREFIX", "/usr".into());
    }
    if !cx.var_is_on(b"CPACK_SET_DESTDIR") {
        cx.set_var(b"CPACK_SET_DESTDIR", "I_ON".into());
    }
    if generator == "RPM" {
        for key in [b"CPACK_PACKAGE_NAME".as_slice(), b"CPACK_PACKAGE_FILE_NAME"] {
            if let Some(mut value) = cx.var(key) {
                for byte in value.iter_mut().filter(|byte| **byte == b' ') {
                    *byte = b'-';
                }
                cx.set_var(key, value);
            }
        }
    }
    if cx.var(b"CPACK_PACKAGE_DESCRIPTION").is_none()
        && let Some(file) = cx.var(b"CPACK_PACKAGE_DESCRIPTION_FILE")
    {
        let path = absolute(cwd, &file);
        let default = absolute(cwd, &cx.var_or_empty(b"CMAKE_ROOT"))
            .join("Templates/CPack.GenericDescription.txt");
        if path == default
            || cx.var(b"CPACK_DEFAULT_PACKAGE_DESCRIPTION_FILE").as_ref() == Some(&file)
        {
            cx.set_var(b"CPACK_USED_DEFAULT_PACKAGE_DESCRIPTION_FILE", "ON".into());
        }
        let content = cx
            .host()
            .read(&path, ReadRange::All { limit: 1024 * 1024 })?;
        let mut content = content.replace(b"\r\n", b"\n");
        if !content.is_empty() && !content.ends_with(b"\n") {
            content.push(b'\n');
        }
        cx.set_var(b"CPACK_PACKAGE_DESCRIPTION", BString::from(content));
    }
    Ok(())
}

struct PublishStageDir<'a> {
    host: &'a dyn Host,
    path: PathBuf,
}

impl<'a> PublishStageDir<'a> {
    fn new(host: &'a dyn Host, output: &Path) -> Result<Self, CpackError> {
        static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);
        let pid = host.system().pid;
        for _ in 0..100 {
            let seq = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let candidate = output.join(format!(".rcmake-pub-{pid}-{seq}"));
            match host.create_private_dir(&candidate) {
                Ok(()) => {
                    return Ok(Self {
                        host,
                        path: candidate,
                    });
                }
                Err(err) if err.kind == ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        Err(error("Cannot allocate temporary publication directory"))
    }
}

impl Drop for PublishStageDir<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

fn stage_temp_path(host: &dyn Host, stage_dir: &Path, label: &str) -> Result<PathBuf, CpackError> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let pid = host.system().pid;
    let tag = HashAlgo::Md5.hex_digest(label.as_bytes());
    let short_tag = tag.get(..12).unwrap_or(&tag);
    for _ in 0..100 {
        let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = stage_dir.join(format!(".rcmake-pub-{pid}-{seq}-{short_tag}.tmp"));
        match host.metadata_fresh(&candidate, Follow::No) {
            Err(err) if err.kind == ErrorKind::NotFound => return Ok(candidate),
            Err(err) => return Err(err.into()),
            Ok(_) => {}
        }
    }
    Err(error("Cannot allocate temporary publication path"))
}

fn verify_destination_replaceable(host: &dyn Host, dest: &Path) -> Result<(), CpackError> {
    match host.metadata_fresh(dest, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File => Ok(()),
        Ok(_) => Err(error(format!(
            "Expected a regular package file at {}",
            dest.display()
        ))),
        Err(err) if err.kind == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn backup_existing_destination(
    host: &dyn Host,
    dest: &Path,
    backup: &Path,
) -> Result<bool, CpackError> {
    match host.metadata_fresh(dest, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File => {
            if host.hard_link(dest, backup).is_ok() {
                return Ok(true);
            }
            let mut reader = host.open_read(dest)?;
            host.write_from(backup, WriteMode::CreateNew, &mut *reader, meta.len)?;
            host.set_permissions(backup, meta.mode & 0o7777)?;
            host.set_times(backup, Some(meta.atime), Some(meta.mtime))?;
            Ok(true)
        }
        Ok(_) => Err(error(format!(
            "Expected a regular package file at {}",
            dest.display()
        ))),
        Err(err) if err.kind == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Publish each generated package and its checksums under a per-package destination lock
/// with staged preparation and rollback on rename failure.
///
/// Commit protocol per package:
/// 1. Stage the package and all checksums inside a private scoped temporary directory on the
///    destination volume (`WriteMode::CreateNew`), verifying no-follow metadata and byte lengths
///    before touching any live destination. Dropping the staging directory removes all temporary
///    files, including partial writes left by failed I/O.
/// 2. Acquire the per-package advisory lock, re-verify every destination is replaceable, and
///    retain a backup of any pre-existing destination files inside the private staging directory.
/// 3. Rename the staged package and every staged checksum into place in deterministic order.
///    If any rename fails, roll back all earlier replacements for that package (restoring backed-up
///    files or removing newly created files) before returning the error.
/// 4. Emit package and checksum success log lines only after the entire set commits.
///
/// This provides lock-serialized replacement and error rollback across independent file renames;
/// it is not a crash-consistent single-syscall transaction across multiple files.
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
    let lock_dir = output.join("_CPack_Packages");
    host.create_dir_all(&lock_dir)?;
    for package in packages {
        let name = package
            .file_name()
            .ok_or_else(|| error("No package file name"))?;
        let file = from_native(Path::new(name));
        let dest = output.join(safe_name(&file)?);
        verify_destination_replaceable(host, &dest)?;

        let src_meta = host.metadata_fresh(package, Follow::No)?;
        if src_meta.kind != FileKind::File {
            return Err(error(format!(
                "Expected a regular package file at {}",
                package.display()
            )));
        }

        let stage_dir = PublishStageDir::new(host, output)?;
        let staged_package = if package != &dest {
            let tmp = stage_temp_path(host, &stage_dir.path, &file.to_string())?;
            let mut reader = host.open_read(package)?;
            host.write_from(&tmp, WriteMode::CreateNew, &mut *reader, src_meta.len)?;
            host.set_permissions(&tmp, src_meta.mode & 0o7777)?;
            host.set_times(&tmp, Some(src_meta.atime), Some(src_meta.mtime))?;
            let tmp_meta = host.metadata_fresh(&tmp, Follow::No)?;
            if tmp_meta.kind != FileKind::File || tmp_meta.len != src_meta.len {
                return Err(error(format!(
                    "Staged publication file failed validation: {}",
                    tmp.display()
                )));
            }
            Some(tmp)
        } else {
            None
        };

        let digest_source = staged_package.as_deref().unwrap_or(package);
        let mut staged_checksums = Vec::new();
        if !algorithms.is_empty() {
            let digests = package_digests(host, digest_source, algorithms.values().copied())?;
            for (extension, digest) in algorithms.keys().zip(digests) {
                let checksum_name = [file.as_slice(), b".", extension].concat();
                let checksum_dest = output.join(safe_name(&checksum_name)?);
                verify_destination_replaceable(host, &checksum_dest)?;
                let text = format!("{digest}  {file}\n");
                let tmp = stage_temp_path(
                    host,
                    &stage_dir.path,
                    &BString::from(checksum_name).to_string(),
                )?;
                host.write(&tmp, text.as_bytes(), WriteMode::CreateNew)?;
                let tmp_meta = host.metadata_fresh(&tmp, Follow::No)?;
                if tmp_meta.kind != FileKind::File || tmp_meta.len != text.len() as u64 {
                    return Err(error(format!(
                        "Staged checksum file failed validation: {}",
                        tmp.display()
                    )));
                }
                staged_checksums.push((tmp, checksum_dest));
            }
        }

        let lock_key = HashAlgo::Sha256.hex_digest(&file);
        let lock_path = lock_dir.join(format!(".publish-{lock_key}.lock"));
        let _lock = match host.lock_file(&lock_path, Some(Duration::from_secs(30))) {
            Ok(guard) => Some(guard),
            Err(err) if err.kind == ErrorKind::Unsupported => None,
            Err(err) => return Err(err.into()),
        };

        verify_destination_replaceable(host, &dest)?;
        let checksum_dests: Vec<PathBuf> = staged_checksums
            .iter()
            .map(|(_, checksum_dest)| checksum_dest.clone())
            .collect();
        for checksum_dest in &checksum_dests {
            verify_destination_replaceable(host, checksum_dest)?;
        }

        let mut replacements = Vec::with_capacity(staged_checksums.len() + 1);
        if let Some(tmp) = staged_package {
            replacements.push((tmp, dest.clone()));
        }
        replacements.extend(staged_checksums);

        let mut backups = Vec::with_capacity(replacements.len());
        for (idx, (_, dest_path)) in replacements.iter().enumerate() {
            let backup_path = stage_dir.path.join(format!("backup-{idx}"));
            let existed = backup_existing_destination(host, dest_path, &backup_path)?;
            backups.push(existed.then_some(backup_path));
        }

        let mut committed: Vec<(PathBuf, Option<PathBuf>)> = Vec::with_capacity(replacements.len());
        for ((staged_tmp, dest_path), backup) in replacements.into_iter().zip(backups) {
            if let Err(commit_err) = host.rename(&staged_tmp, &dest_path) {
                let mut rollback_errors = Vec::new();
                for (prev_dest, prev_backup) in committed.into_iter().rev() {
                    let result = match prev_backup {
                        Some(backup_path) => host.rename(&backup_path, &prev_dest),
                        None => host.remove_file(&prev_dest),
                    };
                    if let Err(err) = result {
                        rollback_errors.push(format!("{}: {err}", prev_dest.display()));
                    }
                }
                if rollback_errors.is_empty() {
                    return Err(commit_err.into());
                }
                return Err(error(format!(
                    "{commit_err} (rollback failed for: {})",
                    rollback_errors.join(", ")
                )));
            }
            committed.push((dest_path, backup));
        }

        log(host, format!("- package: {} generated.", dest.display()));
        for checksum_dest in &checksum_dests {
            log(
                host,
                format!("- checksum file: {} generated.", checksum_dest.display()),
            );
        }
    }
    Ok(())
}

fn package_digests(
    host: &dyn Host,
    package: &Path,
    algorithms: impl Iterator<Item = HashAlgo>,
) -> HostResult<Vec<String>> {
    const CHUNK_SIZE: u64 = 1024 * 1024;
    let mut hashers = algorithms.map(HashAlgo::hasher).collect::<Vec<_>>();
    let mut offset = 0;
    loop {
        let bytes = host.read(
            package,
            ReadRange::Slice {
                offset,
                len: Some(CHUNK_SIZE),
            },
        )?;
        for hasher in &mut hashers {
            hasher.update(&bytes);
        }
        if (bytes.len() as u64) < CHUNK_SIZE {
            break;
        }
        offset += CHUNK_SIZE;
    }
    Ok(hashers
        .into_iter()
        .map(|hasher| hasher.finish_hex())
        .collect())
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
