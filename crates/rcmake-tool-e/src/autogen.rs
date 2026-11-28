use std::{
    collections::BTreeSet,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::{
    CommandSpec, FileKind, FileTime, Follow, Input, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, Stream, WriteMode,
    path::{from_native_cmake as from_native, to_native},
};

use super::ToolCtx;

mod cache;
mod depfile;
mod info;
mod moc;
mod parse;
mod rcc;
mod uic;

#[cfg(test)]
mod tests;

type Result<T> = std::result::Result<T, String>;

const DATA_LIMIT: u64 = 16 * 1024 * 1024;
const ITEM_LIMIT: usize = 262_144;

pub(super) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    dispatch(args, cx, false)
}

pub(super) fn run_rcc(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    dispatch(args, cx, true)
}

fn dispatch(args: &[BString], cx: &ToolCtx<'_>, rcc: bool) -> i32 {
    let name = if rcc { "AutoRcc" } else { "AutoGen" };
    let (input, config, executable_config) = match args {
        [input, config, executable_config, ..] => {
            (input, config.as_slice(), executable_config.as_slice())
        }
        [input, config] => (input, config.as_slice(), config.as_slice()),
        [input] if rcc => (input, b"".as_slice(), b"".as_slice()),
        _ => {
            let requirement = if rcc {
                "An info file is required."
            } else {
                "An info file and configuration are required."
            };
            cx.host.write_out(
                Stream::Stderr,
                format!("{name} error: {requirement}\n").as_bytes(),
            );
            return 1;
        }
    };
    if config.len() > 1024 || executable_config.len() > 1024 {
        cx.host.write_out(
            Stream::Stderr,
            format!("{name} error: The configuration name exceeds the size limit.\n").as_bytes(),
        );
        return 1;
    }
    let path = cx.cwd.join(to_native(input));
    let result = info::Reader::read(cx, &path, config, executable_config).and_then(|reader| {
        if rcc {
            rcc::run(cx, info::Rcc::read(&reader)?)
        } else {
            generate(cx, info::Autogen::read(&reader)?)
        }
    });
    if let Err(error) = result {
        cx.host.write_out(
            Stream::Stderr,
            format!("{name} error in \"{}\":\n  {error}\n", path.display()).as_bytes(),
        );
        return 1;
    }
    0
}

fn generate(cx: &ToolCtx<'_>, info: info::Autogen) -> Result<()> {
    ensure_dir(cx, &info.common.include)?;
    let cmake_time = required_time(cx, &info.cmake)?;
    let previous = cache::settings(cx, &info.common.settings);
    let moc_hash = info
        .moc
        .as_ref()
        .map(|moc| moc::settings_digest(moc, info.qt_version));
    let uic_hash = info.uic.as_ref().map(uic::settings_digest);
    let moc_changed = previous.get(b"moc".as_slice()) != moc_hash.as_ref();
    let uic_changed = previous.get(b"uic".as_slice()) != uic_hash.as_ref();
    if moc_changed || uic_changed {
        remove(cx, &info.common.settings)?;
    }
    let moc_time = info
        .moc
        .as_ref()
        .map(|moc| required_time(cx, &moc.executable))
        .transpose()?;
    let uic_time = info
        .uic
        .as_ref()
        .map(|uic| required_time(cx, &uic.executable))
        .transpose()?;

    let mut cache = cache::ParseCache::read(cx, &info.parse_cache);
    let mut dependencies: BTreeSet<_> = info.list_files.iter().cloned().collect();
    let stamps = cache::ScanTimes {
        moc: (!moc_changed).then_some(moc_time).flatten(),
        uic: (!uic_changed).then_some(uic_time.unwrap_or_default()),
        metadata: cmake_time.max(required_time(cx, &info.info_file)?),
    };

    for row in &info.rows {
        dependencies.insert(row.path.clone());
        let time = required_time(cx, &row.path)?;
        cache.scan(cx, row, time, &info, &stamps)?;
    }
    let mut jobs = Jobs::default();
    moc::plan(cx, &info, &cache, &mut jobs)?;
    uic::plan(cx, &info, &cache, &mut jobs)?;
    for job in jobs.outputs.values() {
        if job.kind == JobKind::Header && cache.get(&job.input).is_none() {
            let row = info::ScanRow {
                path: job.input.clone(),
                kind: JobKind::Header,
                moc: true,
                uic: false,
                output: None,
            };
            cache.scan(cx, &row, required_time(cx, &row.path)?, &info, &stamps)?;
        }
    }
    uic::generate(cx, &info, uic_changed, &jobs)?;
    if let Some(moc) = &info.moc {
        moc::generate(
            cx,
            &info,
            moc,
            moc_changed,
            &jobs,
            &mut cache,
            &mut dependencies,
        )?;
    }
    dependencies.retain(|path| {
        !jobs.outputs.contains_key(path)
            && info.moc.as_ref().is_none_or(|moc| !moc.skip.contains(path))
    });
    // Generated UI inputs remain dependencies even if a moc skip list names them.
    dependencies.extend(
        jobs.outputs
            .values()
            .filter_map(|job| (job.kind == JobKind::Uic).then_some(job.input.clone())),
    );
    depfile::write(cx, &info, dependencies)?;
    cache.write(cx, &info.parse_cache)?;
    cache::write_settings(
        cx,
        &info.common.settings,
        [("moc", moc_hash), ("uic", uic_hash)],
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JobKind {
    Header,
    Source,
    Module,
    Uic,
}

struct Job {
    kind: JobKind,
    input: PathBuf,
    aggregate: Option<BString>,
    includers: BTreeSet<PathBuf>,
}

#[derive(Default)]
struct Jobs {
    outputs: std::collections::BTreeMap<PathBuf, Job>,
}

impl Jobs {
    fn add(
        &mut self,
        output: PathBuf,
        kind: JobKind,
        input: &Path,
        aggregate: Option<BString>,
        includer: Option<&Path>,
    ) -> Result<()> {
        use std::collections::btree_map::Entry;

        if self.outputs.len() >= ITEM_LIMIT && !self.outputs.contains_key(&output) {
            return Err("Too many generated files.".into());
        }
        match self.outputs.entry(output) {
            Entry::Occupied(mut entry) => {
                let job = entry.get_mut();
                if job.input != input || job.kind != kind || job.aggregate != aggregate {
                    return Err(format!(
                        "The generated file \"{}\" has conflicting inputs \"{}\" and \"{}\". Includers: {:?}, {:?}.",
                        entry.key().display(),
                        entry.get().input.display(),
                        input.display(),
                        entry.get().includers,
                        includer
                    ));
                }
                job.includers.extend(includer.map(Path::to_path_buf));
            }
            Entry::Vacant(entry) => {
                entry.insert(Job {
                    kind,
                    input: input.to_path_buf(),
                    aggregate,
                    includers: includer.map(Path::to_path_buf).into_iter().collect(),
                });
            }
        }
        Ok(())
    }
}

fn normalized(path: &Path) -> PathBuf {
    to_native(&Lexical::new(&from_native(path)).normalize())
}

fn resolve(base: &Path, value: &[u8]) -> PathBuf {
    normalized(&base.join(to_native(value)))
}

fn safe_relative(value: &[u8]) -> bool {
    !value.is_empty()
        && !value.contains(&0)
        && !value.contains(&b'\n')
        && !value.contains(&b'\r')
        && !value.contains(&b'\\')
        && to_native(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        && to_native(value).file_name().is_some()
}

fn output_path(base: &Path, value: &[u8]) -> Result<PathBuf> {
    if !safe_relative(value) {
        return Err(format!(
            "Invalid generated file path: \"{}\".",
            String::from_utf8_lossy(value)
        ));
    }
    Ok(resolve(base, value))
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn timestamp(cx: &ToolCtx<'_>, path: &Path) -> Option<FileTime> {
    cx.host
        .metadata(path, Follow::Yes)
        .ok()
        .filter(|metadata| metadata.kind == FileKind::File)
        .map(|metadata| metadata.mtime)
}

fn required_time(cx: &ToolCtx<'_>, path: &Path) -> Result<FileTime> {
    timestamp(cx, path).ok_or_else(|| format!("The file \"{}\" does not exist.", path.display()))
}

fn ensure_dir(cx: &ToolCtx<'_>, path: &Path) -> Result<()> {
    cx.host
        .create_dir_all(path)
        .map_err(|error| format!("Cannot create \"{}\": {error}", path.display()))
}

fn ensure_parent(cx: &ToolCtx<'_>, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(cx, parent)?;
    }
    Ok(())
}

fn read(cx: &ToolCtx<'_>, path: &Path, limit: u64) -> Result<Vec<u8>> {
    required_time(cx, path)?;
    cx.host
        .read(path, ReadRange::All { limit })
        .map_err(|error| format!("Cannot read \"{}\": {error}", path.display()))
}

fn write_file(cx: &ToolCtx<'_>, path: &Path, contents: &[u8]) -> Result<()> {
    ensure_parent(cx, path)?;
    cx.host
        .write(path, contents, WriteMode::Truncate)
        .map_err(|error| format!("Cannot write \"{}\": {error}", path.display()))
}

fn write_changed(cx: &ToolCtx<'_>, path: &Path, contents: &[u8]) -> Result<bool> {
    if read(cx, path, DATA_LIMIT).is_ok_and(|old| old == contents) {
        return Ok(false);
    }
    write_file(cx, path, contents)?;
    Ok(true)
}

fn touch(cx: &ToolCtx<'_>, path: &Path) -> Result<()> {
    cx.host
        .set_times(path, None, Some(cx.host.now()))
        .map_err(|error| format!("Cannot set the time on \"{}\": {error}", path.display()))
}

fn remove(cx: &ToolCtx<'_>, path: &Path) -> Result<()> {
    match cx.host.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot remove \"{}\": {error}", path.display())),
    }
}

fn warn(cx: &ToolCtx<'_>, tool: &str, message: &str) {
    cx.host.write_out(
        Stream::Stderr,
        format!("Auto{tool} warning: {message}\n").as_bytes(),
    );
}

fn run_tool(
    cx: &ToolCtx<'_>,
    executable: &Path,
    args: Vec<BString>,
    cwd: &Path,
    verbose: bool,
) -> Result<RunOutput> {
    let program = from_native(executable);
    let windows = cx.host.system().sysname.eq_ignore_ascii_case(b"Windows");
    let command = || {
        let mut command = BString::from(if windows { "&" } else { "" });
        for arg in std::iter::once(&program).chain(&args) {
            if !command.is_empty() {
                command.push(b' ');
            }
            if windows {
                command.push(b'\'');
                for &byte in arg.as_slice() {
                    if byte == b'\'' {
                        command.push(byte);
                    }
                    command.push(byte);
                }
                command.push(b'\'');
            } else {
                command.extend_from_slice(&rcmake_base::shell::quote(arg));
            }
        }
        command
    };
    if verbose {
        let prefix = if windows {
            "Running PowerShell command: "
        } else {
            "Running "
        };
        let message = format!("{prefix}{}\n", command());
        cx.host.write_out(Stream::Stdout, message.as_bytes());
    }
    let failure = |status: &dyn std::fmt::Display| {
        let label = if windows {
            "PowerShell command"
        } else {
            "Command"
        };
        format!(
            "The tool failed.\n{label}: {}\nWorking directory: {}\nStatus: {status}\n",
            command(),
            cwd.display(),
        )
    };
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: args.clone(),
        }],
        cwd: cwd.to_path_buf(),
        env: cx.host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(std::time::Duration::from_secs(1800)),
        tee: false,
        default_error_mode: false,
    };
    let output = cx
        .host
        .run_bounded(&request, DATA_LIMIT as usize)
        .map_err(|error| failure(&error))?;
    if output.results.as_slice() != [ProcessResult::Exited(0)] {
        return Err(format!(
            "{}Output:\n{}{}",
            failure(&format_args!("{:?}", output.results)),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output)
}

fn with_includers(mut error: String, includers: &BTreeSet<PathBuf>) -> String {
    use std::fmt::Write;

    if !includers.is_empty() {
        error.push_str("\nIncluders:");
        for path in includers {
            let _ = write!(error, "\n  {}", path.display());
        }
    }
    error
}

fn run_generator(
    cx: &ToolCtx<'_>,
    common: &info::Common,
    executable: &Path,
    args: Vec<BString>,
    input: &Path,
    output: &Path,
    sidecar: Option<&Path>,
) -> Result<()> {
    ensure_parent(cx, output)?;
    remove(cx, output)?;
    if let Some(path) = sidecar {
        remove(cx, path)?;
    }
    let generated =
        run_tool(cx, executable, args, &common.build, common.verbose).and_then(|result| {
            required_time(cx, output)?;
            Ok(result)
        });
    match generated {
        Ok(result) => {
            cx.host.write_out(Stream::Stdout, &result.stdout);
            cx.host.write_out(Stream::Stderr, &result.stderr);
            Ok(())
        }
        Err(error) => {
            let _ = remove(cx, output);
            if let Some(path) = sidecar {
                let _ = remove(cx, path);
            }
            Err(format!(
                "Cannot generate \"{}\" from \"{}\".\n{error}",
                output.display(),
                input.display()
            ))
        }
    }
}
