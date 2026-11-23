//! Collect pending snippets and invoke the callbacks registered by queries.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, Stream, WriteMode, path};
use rcmake_json::Value;

use crate::files::{absolute, api_dir, json_files, publish_at, read_json, timestamp};
use crate::launcher::execute;
use crate::query::read_queries;
use crate::snippet::index_value;
use crate::trace::trace_value;
use crate::{Hook, IndexFile, InstrumentError, InstrumentOption, invalid};

static NEXT_BATCH: AtomicU64 = AtomicU64::new(0);

struct IndexLock<'a> {
    host: &'a dyn Host,
    file: PathBuf,
}

impl<'a> IndexLock<'a> {
    fn acquire(host: &'a dyn Host, data: &Path) -> Result<Self, InstrumentError> {
        host.create_dir_all(data)?;
        let file = data.join(".index.lock");
        for _ in 0..200 {
            match host.write(&file, b"", WriteMode::CreateNew) {
                Ok(()) => return Ok(Self { host, file }),
                Err(error) if error.kind == ErrorKind::AlreadyExists => {
                    host.sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(invalid(format!(
            "Timed out waiting for the instrumentation index lock at {}.",
            file.display()
        )))
    }
}

impl Drop for IndexLock<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_file(&self.file);
    }
}

fn callback_arguments(command: &[u8]) -> Result<Vec<BString>, InstrumentError> {
    let mut command = command.iter().copied();
    let mut args = Vec::new();
    let mut word = BString::default();
    let mut quote = None;
    let mut started = false;
    while let Some(byte) = command.next() {
        match (quote, byte) {
            (Some(open), close) if open == close => quote = None,
            (None, b'\'' | b'"') => {
                started = true;
                quote = Some(byte);
            }
            (None | Some(b'"'), b'\\') => {
                started = true;
                word.push(
                    command
                        .next()
                        .ok_or_else(|| invalid("The callback ends with a backslash."))?,
                );
            }
            (None, byte) if byte.is_ascii_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                started = true;
                word.push(byte);
            }
        }
    }
    if quote.is_some() {
        return Err(invalid("The callback contains an unclosed quote."));
    }
    if started {
        args.push(word);
    }
    if args.first().is_none_or(|program| program.is_empty()) {
        return Err(invalid("The callback command is empty."));
    }
    Ok(args)
}

fn pending(host: &dyn Host, data: &Path) -> Result<Vec<BString>, InstrumentError> {
    let mut indexed = BTreeSet::new();
    let index_dir = data.join("index");
    for name in json_files(host, &index_dir)? {
        if !name.starts_with(b"index-") {
            continue;
        }
        let file = index_dir.join(path::to_native(&name));
        let value = match read_json(host, &file) {
            Ok(value) => value,
            Err(InstrumentError::Host(error)) if error.kind == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Value::Object(index) = value
            && let Some(Value::Array(snippets)) = index.get(b"snippets".as_slice())
        {
            indexed.extend(snippets.iter().filter_map(|snippet| match snippet {
                Value::String(name) => Some(name.clone()),
                _ => None,
            }));
        }
    }

    let mut result = Vec::new();
    for prefix in ["", "v1"] {
        for name in json_files(host, &data.join(prefix))? {
            let name = path::from_native(&Path::new(prefix).join(path::to_native(&name)));
            if !indexed.contains(&name) {
                result.push(name);
            }
        }
    }
    Ok(result)
}

fn write_batch(
    host: &dyn Host,
    build: &Path,
    data: &Path,
    hook: Hook,
    include_static: bool,
    include_trace: bool,
) -> Result<(PathBuf, Vec<BString>), InstrumentError> {
    let _lock = IndexLock::acquire(host, data)?;
    let mut index = IndexFile {
        build_dir: path::from_native(build),
        data_dir: path::from_native(data),
        hook: hook.as_str().into(),
        snippets: pending(host, data)?,
        trace: None,
    };
    let suffix = format!(
        "{}-{}-{}",
        timestamp(host),
        host.system().pid,
        NEXT_BATCH.fetch_add(1, Ordering::Relaxed)
    );
    if include_trace {
        let snippets = index
            .snippets
            .iter()
            .map(|name| read_json(host, &data.join(path::to_native(name))))
            .collect::<Result<Vec<_>, _>>()?;
        let trace = trace_value(snippets)?;
        let file = publish_at(
            host,
            &data.join("trace"),
            &format!("trace-{suffix}"),
            &trace,
        )?;
        index.trace = file.strip_prefix(data).ok().map(path::from_native);
    }
    let file = match publish_at(
        host,
        &data.join("index"),
        &format!("index-{suffix}"),
        &index_value(host, &index, include_static),
    ) {
        Ok(file) => file,
        Err(error) => {
            if let Some(trace) = index.trace {
                let _ = host.remove_file(&data.join(path::to_native(&trace)));
            }
            return Err(error);
        }
    };
    Ok((file, index.snippets))
}

/// Index pending snippets for a subscribed hook and pass the index to each callback.
///
/// `Manual` can also be used to request indexing directly. The index and its
/// snippets are available during callbacks and removed after callbacks return.
///
/// # Errors
///
/// Returns an error if a query, indexing operation, or cleanup operation fails.
pub fn run_hook(host: &dyn Host, build_dir: &Path, hook: Hook) -> Result<(), InstrumentError> {
    let build = absolute(host, build_dir);
    let queries = read_queries(host, &build)?;
    if hook != Hook::Manual && !queries.iter().any(|query| query.hooks.contains(&hook)) {
        return Ok(());
    }
    let enabled = |option| queries.iter().any(|query| query.options.contains(&option));
    let data = api_dir(&build).join("data");
    let (index, snippets) = write_batch(
        host,
        &build,
        &data,
        hook,
        enabled(InstrumentOption::StaticSystemInformation),
        enabled(InstrumentOption::Trace) || enabled(InstrumentOption::CompileTrace),
    )?;
    for callback in queries.iter().flat_map(|query| &query.callbacks) {
        let result = callback_arguments(callback).and_then(|mut command| {
            command.push(path::from_native(&index));
            execute(
                host,
                host.initial_cwd(),
                &command,
                false,
                Some(Duration::from_secs(60)),
            )
        });
        match result {
            Err(error) => host.write_out(Stream::Stderr, format!("{error}\n").as_bytes()),
            Ok(output) => {
                if let Some(rcmake_host::ProcessResult::SpawnFailed(error)) = output.results.first()
                {
                    host.write_out(Stream::Stderr, format!("{error}\n").as_bytes());
                }
            }
        }
    }
    for name in snippets {
        match host.remove_file(&data.join(path::to_native(&name))) {
            Ok(()) => {}
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    host.remove_file(&index)?;
    Ok(())
}
