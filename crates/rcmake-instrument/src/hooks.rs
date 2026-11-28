//! Collect pending snippets and invoke the callbacks registered by queries.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{FileTime, Follow, Host, Stream, path};
use rcmake_json::Value;

use crate::files::{absolute, api_dir, file_stem, json_files, publish_at, read_json, timestamp};
use crate::launcher::execute;
use crate::query::read_queries;
use crate::snippet::index_value;
use crate::trace::trace_value;
use crate::{Hook, IndexFile, InstrumentError, InstrumentOption, invalid};

static NEXT_BATCH: AtomicU64 = AtomicU64::new(0);

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

fn pending(host: &dyn Host, data: &Path) -> Result<(Vec<BString>, bool), InstrumentError> {
    let mut indexed = BTreeSet::new();
    let mut watermark: Option<FileTime> = None;
    let index_dir = data.join("index");
    for name in json_files(host, &index_dir)? {
        if !name.starts_with(b"index-") {
            continue;
        }
        let file = index_dir.join(path::to_native(&name));
        let metadata = match host.metadata(&file, Follow::No) {
            Ok(metadata) => metadata,
            Err(error) if error.kind == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        watermark = Some(watermark.map_or(metadata.mtime, |old| old.max(metadata.mtime)));
        if metadata.len == 0 {
            continue;
        }
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
            let name = path::from_native_cmake(&Path::new(prefix).join(path::to_native(&name)));
            if indexed.contains(&name) {
                continue;
            }
            if let Some(watermark) = watermark {
                match host.metadata(&data.join(path::to_native(&name)), Follow::No) {
                    Ok(metadata) if metadata.mtime > watermark => {}
                    Ok(_) => continue,
                    Err(error) if error.kind == ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            result.push(name);
        }
    }
    Ok((result, watermark.is_some()))
}

fn write_batch(
    host: &dyn Host,
    build: &Path,
    data: &Path,
    hook: Hook,
    include_static: bool,
    include_trace: bool,
) -> Result<(PathBuf, Vec<BString>), InstrumentError> {
    let (snippets, _) = pending(host, data)?;
    let mut index = IndexFile {
        build_dir: path::from_native_cmake(build),
        data_dir: path::from_native_cmake(data),
        hook: hook.as_str().into(),
        snippets,
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
        index.trace = file.strip_prefix(data).ok().map(path::from_native_cmake);
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

fn remove_file(host: &dyn Host, file: &Path) -> Result<(), InstrumentError> {
    match host.remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn prune_artifacts(host: &dyn Host, dir: &Path, prefix: &[u8]) -> Result<(), InstrumentError> {
    let mut files = json_files(host, dir)?;
    files.retain(|name| name.starts_with(prefix));
    files.sort_by(|left, right| file_stem(left).cmp(file_stem(right)));
    files.pop();
    for name in files {
        remove_file(host, &dir.join(path::to_native(&name)))?;
    }
    Ok(())
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
    let api = api_dir(&build);
    let data = api.join("data");
    let lock_path = api.join(".index.lock");
    host.create_dir_all(&api)?;
    let (index, snippets) = {
        let _lock = host.lock_file(&lock_path, None)?;
        write_batch(
            host,
            &build,
            &data,
            hook,
            enabled(InstrumentOption::StaticSystemInformation),
            enabled(InstrumentOption::Trace),
        )?
    };
    for callback in queries.iter().flat_map(|query| &query.callbacks) {
        let result = callback_arguments(callback).and_then(|mut command| {
            command.push(path::from_native_cmake(&index));
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
    let _lock = host.lock_file(&lock_path, None)?;
    for name in snippets {
        remove_file(host, &data.join(path::to_native(&name)))?;
    }
    remove_file(host, &index)?;
    let (pending, other_index) = pending(host, &data)?;
    if !other_index {
        prune_artifacts(host, &data.join("trace"), b"trace-")?;
        if pending.is_empty() {
            prune_artifacts(host, &data.join("content"), b"cmake-")?;
        }
    }
    Ok(())
}
