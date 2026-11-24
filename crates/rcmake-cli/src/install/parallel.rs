use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rcmake_base::{BString, Limits, hash::HashAlgo};
use rcmake_host::{
    CommandSpec, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest, WriteMode,
    discover_cmake_root, path,
};
use rcmake_json::{ReadOptions, Value, parse};

use super::{Options, job_count};

pub(super) struct Index {
    scripts: Vec<PathBuf>,
    configs: Vec<BString>,
}

impl Index {
    pub(super) fn load(host: &dyn Host, dir: &Path) -> Result<Option<Self>, String> {
        let file = dir.join("CMakeFiles/InstallScripts.json");
        let metadata = match host.metadata(&file, Follow::Yes) {
            Ok(metadata) => metadata,
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if host
            .metadata(&dir.join("CMakeFiles/cmake.check_cache"), Follow::Yes)
            .is_ok_and(|cache| cache.mtime.secs > metadata.mtime.secs)
        {
            return Ok(None);
        }
        let content = host
            .read(
                &file,
                ReadRange::All {
                    limit: Limits::default().max_file_read,
                },
            )
            .map_err(|error| error.to_string())?;
        let Value::Object(value) =
            parse(&content, &ReadOptions::default()).map_err(|error| error.to_string())?
        else {
            return Err("The install script index must contain a JSON object.".into());
        };
        if value.get(b"Parallel".as_slice()) != Some(&Value::Bool(true)) {
            return Ok(None);
        }
        let scripts = strings(value.get(b"InstallScripts".as_slice()), "InstallScripts")?
            .into_iter()
            .map(|script| dir.join(path::to_native(&script)))
            .collect();
        let configs = strings(value.get(b"Configs".as_slice()), "Configs")?;
        Ok(Some(Self { scripts, configs }))
    }
}

fn strings(value: Option<&Value>, field: &str) -> Result<Vec<BString>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(text) => Ok(text.clone()),
                _ => Err(format!("{field} must contain strings.")),
            })
            .collect(),
        _ => Err(format!("{field} must contain a list.")),
    }
}

pub(super) fn run(
    argv: &[BString],
    opts: &Options,
    host: &dyn Host,
    index: Index,
) -> Result<i32, String> {
    let executable = path::from_native(&discover_cmake_root(host, argv.first()).0);
    let enabled = !rcmake_instrument::query::read_queries(host, &opts.dir)
        .map_err(|error| error.to_string())?
        .is_empty();
    let jobs = opts
        .jobs
        .or_else(|| {
            host.env_get(b"CMAKE_INSTALL_PARALLEL_LEVEL")
                .and_then(|jobs| job_count(&jobs).ok())
        })
        .unwrap_or(1) as usize;
    let configs = match opts.vars.get(b"CMAKE_INSTALL_CONFIG_NAME".as_slice()) {
        Some(config) => vec![Some(config)],
        None if !index.configs.is_empty() => index.configs.iter().map(Some).collect(),
        None => vec![None],
    };
    let components = if opts.components.is_empty() {
        vec![None]
    } else {
        opts.components.iter().map(Some).collect()
    };
    for component in components {
        let manifest = manifest_name(component);
        let mut paths = Vec::new();
        for config in &configs {
            let tasks = Tasks {
                host,
                opts,
                executable: &executable,
                scripts: &index.scripts,
                config: *config,
                component,
                enabled,
            };
            if !tasks.run(jobs)? {
                return Ok(1);
            }
            let part = local_manifests(host, &index.scripts)?;
            if paths.len().saturating_add(part.len()) as u64 > Limits::default().max_file_read {
                return Err("The install manifest exceeds the size limit.".into());
            }
            paths.extend(part);
        }
        publish_manifest(host, &opts.dir, &manifest, &paths)?;
    }
    Ok(0)
}

struct Tasks<'a> {
    host: &'a dyn Host,
    opts: &'a Options,
    executable: &'a BString,
    scripts: &'a [PathBuf],
    config: Option<&'a BString>,
    component: Option<&'a BString>,
    enabled: bool,
}

impl Tasks<'_> {
    fn run(&self, jobs: usize) -> Result<bool, String> {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..jobs.clamp(1, 64).min(self.scripts.len()) {
                let worker = || -> Result<bool, String> {
                    let mut passed = true;
                    while let Some(script) = self.scripts.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        passed &= self.script(script)?;
                    }
                    Ok(passed)
                };
                workers.push(
                    std::thread::Builder::new()
                        .spawn_scoped(scope, worker)
                        .map_err(|error| error.to_string())?,
                );
            }
            let mut passed = true;
            for worker in workers {
                passed &= worker
                    .join()
                    .map_err(|_| "An install worker terminated unexpectedly.")??;
            }
            Ok(passed)
        })
    }

    fn script(&self, script: &Path) -> Result<bool, String> {
        let mut command = vec![
            self.executable.clone(),
            "-DCMAKE_INSTALL_LOCAL_ONLY=1".into(),
        ];
        for (key, value) in &self.opts.vars {
            if *key != b"CMAKE_INSTALL_CONFIG_NAME" {
                command.push(define(key, value));
            }
        }
        if let Some(config) = self.config {
            command.push(define(b"CMAKE_INSTALL_CONFIG_NAME", config));
        }
        if let Some(component) = self.component {
            command.push(define(b"CMAKE_INSTALL_COMPONENT", component));
        }
        command.extend(["-P".into(), path::from_native(script)]);
        if self.enabled {
            return rcmake_instrument::launcher::run_install(
                self.host,
                &self.opts.dir,
                &command,
                self.config.cloned(),
            )
            .map(|code| code == 0)
            .map_err(|error| error.to_string());
        }

        let [program, args @ ..] = command.as_slice() else {
            return Ok(false);
        };
        let result = self
            .host
            .run(
                &RunRequest {
                    pipeline: vec![CommandSpec {
                        program: program.clone(),
                        args: args.to_vec(),
                    }],
                    cwd: self.host.initial_cwd(),
                    env: self.host.env_all(),
                    stdin: Input::Inherit,
                    stdout: Output::Inherit,
                    stderr: Output::Inherit,
                    timeout: Some(Duration::from_secs(600)),
                    tee: false,
                },
                &mut |_, _| {},
            )
            .map_err(|error| error.to_string())?;
        Ok(matches!(
            result.results.as_slice(),
            [ProcessResult::Exited(0)]
        ))
    }
}

fn define(key: &[u8], value: &[u8]) -> BString {
    BString::from([b"-D", key, b"=", value].concat())
}

fn manifest_name(component: Option<&BString>) -> String {
    let Some(component) = component.filter(|name| !name.is_empty()) else {
        return "install_manifest.txt".into();
    };
    let name = if component
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
    {
        component.to_string()
    } else {
        HashAlgo::Md5.hex_digest(component)
    };
    format!("install_manifest_{name}.txt")
}

fn publish_manifest(host: &dyn Host, dir: &Path, name: &str, content: &[u8]) -> Result<(), String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let now = host.now();
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(
        ".{name}.{}-{}-{}-{sequence}.tmp",
        host.system().pid,
        now.secs,
        now.nanos
    ));
    host.write(&temporary, content, WriteMode::CreateNew)
        .map_err(|error| error.to_string())?;
    let result = host.rename(&temporary, &dir.join(name));
    if result.is_err() {
        let _ = host.remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}

fn local_manifests(host: &dyn Host, scripts: &[PathBuf]) -> Result<Vec<u8>, String> {
    let mut paths = Vec::new();
    let limit = Limits::default().max_file_read;
    for script in scripts {
        let file = script.with_file_name("install_local_manifest.txt");
        let remaining = limit.saturating_sub(paths.len() as u64);
        match host.read(&file, ReadRange::All { limit: remaining }) {
            Ok(data) if !data.is_empty() => {
                paths.extend_from_slice(&data);
                if !data.ends_with(b"\n") {
                    paths.push(b'\n');
                }
            }
            Ok(_) => {}
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(paths)
}
