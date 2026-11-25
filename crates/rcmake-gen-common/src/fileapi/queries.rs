use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_host::{DirEntry, FileKind, Follow, Host, ReadRange, path::to_native};
use rcmake_json::{ReadOptions, Value};

use super::{Array, FileApi, Kind, Members, Object, ReplyWriter, UInt, array, object, read_dir};
use crate::{GenError, write_error};

const QUERY_LIMIT: u64 = 4 * 1024 * 1024;

/// Global property that lists the `cmake_file_api(QUERY)` object names.
const PROJECT_QUERIES: &[u8] = b"_RCMAKE_FILE_API_PROJECT_QUERIES";

impl FileApi<'_, '_> {
    pub(super) fn read_queries(
        &mut self,
        dir: &Path,
    ) -> Result<Option<(Members, TestReplies)>, GenError> {
        let host = self.writer.host;
        let user = user_query_dir(host);
        let mut query_exists = false;
        let mut reply = Members::new();
        let mut tests = BTreeMap::new();
        for root in [dir].into_iter().chain(user.as_deref()) {
            query_exists |= host
                .metadata(root, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir);
            for entry in read_dir(host, root)? {
                if reply.contains_key(&entry.name) {
                    continue;
                }
                let response = if entry.name.starts_with(b"client-") {
                    self.client_queries(&root.join(to_native(&entry.name)), &mut tests)?
                } else {
                    self.stateless(&entry.name, &mut tests)?
                };
                reply.insert(entry.name, response);
            }
        }
        // `cmake_file_api(QUERY)` project queries have no query files.
        if let Some(queries) = self.model.global.props.get(PROJECT_QUERIES) {
            for name in rcmake_base::list::split(queries, rcmake_base::list::Empty::Drop) {
                query_exists = true;
                if let std::collections::btree_map::Entry::Vacant(entry) = reply.entry(name) {
                    let response = self.stateless(entry.key(), &mut tests)?;
                    entry.insert(response);
                }
            }
        }
        Ok(query_exists.then_some((reply, tests)))
    }

    fn client_queries(&mut self, dir: &Path, tests: &mut TestReplies) -> Result<Value, GenError> {
        let mut reply = Members::new();
        for entry in read_dir(self.writer.host, dir)? {
            let value = if entry.name == b"query.json" {
                self.stateful(&dir.join("query.json"), tests)?
            } else {
                self.stateless(&entry.name, tests)?
            };
            reply.insert(entry.name, value);
        }
        Ok(Object(reply))
    }

    fn stateless(&mut self, name: &[u8], tests: &mut TestReplies) -> Result<Value, GenError> {
        match name {
            b"__test-v1" => return self.respond(QueryKind::Test(1), tests),
            b"__test-v2" => return self.respond(QueryKind::Test(2), tests),
            _ => {}
        }
        for kind in Kind::ALL {
            if name == format!("{}-v{}", kind.name(), kind.version().0).as_bytes() {
                return self.reference(kind);
            }
        }
        Ok(error("unknown query file"))
    }

    fn stateful(&mut self, path: &Path, tests: &mut TestReplies) -> Result<Value, GenError> {
        let host = self.writer.host;
        if !host
            .metadata(path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Ok(error("failed to read from file"));
        }
        let Ok(bytes) = host.read(path, ReadRange::All { limit: QUERY_LIMIT }) else {
            return Ok(error("failed to read from file"));
        };
        if bytes
            .strip_prefix(b"\xef\xbb\xbf")
            .unwrap_or(&bytes)
            .iter()
            .all(u8::is_ascii_whitespace)
        {
            return Ok(error("A JSON document cannot be empty."));
        }
        let mut query = match rcmake_json::parse(&bytes, &ReadOptions::default()) {
            Ok(Object(query)) => query,
            Ok(Array(_)) => return Ok(error("query root is not an object")),
            Ok(_) => {
                return Ok(error(
                    "A valid JSON document must be either an array or an object value.",
                ));
            }
            Err(err) => return Ok(error(format!("query.json:{err}").as_str())),
        };
        let mut reply = Members::new();
        if let Some(client) = query
            .remove(b"client".as_slice())
            .filter(|value| *value != Value::Null)
        {
            reply.insert("client".into(), client);
        }
        let responses = match query.remove(b"requests".as_slice()) {
            None | Some(Value::Null) => error("'requests' member missing"),
            Some(requests) => {
                let responses = match &requests {
                    Array(requests) => {
                        let mut responses = Vec::with_capacity(requests.len());
                        for request in requests {
                            responses.push(match request_kind(request) {
                                Ok(kind) => self.respond(kind, tests)?,
                                Err(message) => error(&message),
                            });
                        }
                        array(responses)
                    }
                    _ => error("'requests' member is not an array"),
                };
                reply.insert("requests".into(), requests);
                responses
            }
        };
        reply.insert("responses".into(), responses);
        Ok(Object(reply))
    }

    fn respond(&mut self, kind: QueryKind, tests: &mut TestReplies) -> Result<Value, GenError> {
        match kind {
            QueryKind::Public(kind) => self.reference(kind),
            QueryKind::Test(_) if self.failed => Ok(error("no buildsystem generated")),
            QueryKind::Test(major) => {
                if let Some(reference) = tests.get(&major) {
                    return Ok(reference.clone());
                }
                let version = object([
                    ("major", UInt(major.into())),
                    ("minor", UInt(test_minor(major).into())),
                ]);
                let body = object([
                    ("kind", Value::string("__test")),
                    ("version", version.clone()),
                ]);
                let file = self.writer.hashed(&format!("__test-v{major}"), &body)?;
                let reference = object([
                    ("kind", Value::string("__test")),
                    ("version", version),
                    ("jsonFile", file),
                ]);
                tests.insert(major, reference.clone());
                Ok(reference)
            }
        }
    }
}

fn user_query_dir(host: &dyn Host) -> Option<PathBuf> {
    let config_dir = if let Some(dir) = host.env_get(b"CMAKE_CONFIG_DIR") {
        to_native(&dir)
    } else if cfg!(windows) {
        to_native(&host.env_get(b"LOCALAPPDATA")?).join("CMake")
    } else {
        let (home_config, suffix) = if cfg!(target_os = "macos") {
            ("Library/Application Support", "CMake")
        } else {
            (".config", "cmake")
        };
        host.env_get(b"XDG_CONFIG_HOME")
            .map(|dir| to_native(&dir))
            .or_else(|| {
                host.env_get(b"HOME")
                    .map(|home| to_native(&home).join(home_config))
            })?
            .join(suffix)
    };
    Some(config_dir.join("api/v1/query"))
}

impl ReplyWriter<'_> {
    pub(super) fn retain_file_aliases(&mut self, old_entries: &[DirEntry]) -> Result<(), GenError> {
        if self.files.is_empty() {
            return Ok(());
        }
        let mut stale = old_entries
            .iter()
            .filter(|entry| entry.kind == FileKind::File && !self.files.contains(&entry.name))
            .peekable();
        if stale.peek().is_none() {
            return Ok(());
        }
        let mut identities = BTreeSet::new();
        for name in &self.files {
            let path = self.dir.join(to_native(name));
            let metadata = self
                .host
                .metadata(&path, Follow::No)
                .map_err(|error| write_error(&path, error))?;
            if metadata.ino != 0 {
                identities.insert((metadata.dev, metadata.ino));
            }
        }
        let mut aliases = Vec::new();
        for entry in stale {
            // The old spelling can still refer to a new reply on this filesystem.
            let path = self.dir.join(to_native(&entry.name));
            let metadata = match self.host.metadata(&path, Follow::No) {
                Ok(metadata) => metadata,
                Err(error) if error.kind == ErrorKind::NotFound => continue,
                Err(error) => return Err(write_error(&path, error)),
            };
            if identities.contains(&(metadata.dev, metadata.ino)) {
                aliases.push(entry.name.clone());
            }
        }
        self.files.extend(aliases);
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn preserves_unicode_case_aliases_during_reply_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_host::{PureHost, WriteMode};

    let host = PureHost::default();
    let dir = Path::new("/reply");
    host.create_dir_all(dir)?;
    let old_name = "target-app-Débug.json";
    let new_name = "target-app-DÉBUG.json";
    let stale = dir.join("target-stale-Débug.json");
    host.write(&dir.join(old_name), b"{}", WriteMode::CreateNew)?;
    host.write(&stale, b"{}", WriteMode::CreateNew)?;
    let old_entries = host.read_dir(dir)?;
    // Give both spellings the same identity on the test filesystem.
    host.hard_link(&dir.join(old_name), &dir.join(new_name))?;
    let mut writer = ReplyWriter {
        host: &host,
        dir,
        files: BTreeSet::from([new_name.into()]),
        existing: BTreeSet::new(),
        dir_ready: true,
    };
    writer.retain_file_aliases(&old_entries)?;
    writer.remove_stale(old_entries)?;
    let names = host
        .read_dir(dir)?
        .into_iter()
        .map(|entry| entry.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(names, BTreeSet::from([old_name.into(), new_name.into()]));
    Ok(())
}

pub(super) fn error(message: &str) -> Value {
    object([("error", Value::string(message))])
}

fn member<'a>(object: &'a Members, key: &[u8]) -> Option<&'a Value> {
    object.get(key).filter(|value| **value != Value::Null)
}

pub(super) type TestReplies = BTreeMap<u32, Value>;

enum QueryKind {
    Public(Kind),
    Test(u32),
}

fn test_minor(major: u32) -> u32 {
    if major == 1 { 3 } else { 0 }
}

fn request_kind(request: &Value) -> Result<QueryKind, String> {
    let Object(request) = request else {
        return Err("request is not an object".into());
    };
    let Some(name) = member(request, b"kind") else {
        return Err("'kind' member missing".into());
    };
    let Value::String(name) = name else {
        return Err("'kind' member is not a string".into());
    };
    let kind = Kind::ALL.into_iter().find(|kind| name == kind.name());
    if kind.is_none() && name != "__test" {
        return Err(format!("unknown request kind '{name}'"));
    }
    let value = member(request, b"version").ok_or("'version' member missing")?;
    let versions = match value {
        Array(versions) => versions
            .iter()
            .map(|version| {
                parse_version(
                    version,
                    "'version' array entry is not a non-negative integer or object",
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
        version => vec![parse_version(
            version,
            "'version' member is not a non-negative integer, object, or array",
        )?],
    };
    for &(major, minor) in &versions {
        if let Some(kind) = kind {
            let (supported_major, supported_minor) = kind.version();
            if major == supported_major && minor <= supported_minor {
                return Ok(QueryKind::Public(kind));
            }
        } else if matches!(major, 1 | 2) && minor <= test_minor(major) {
            return Ok(QueryKind::Test(major));
        }
    }
    let mut message = String::from("no supported version specified");
    if !versions.is_empty() {
        message.push_str(" among:");
        for (major, minor) in versions {
            use std::fmt::Write;
            let _ = write!(message, " {major}.{minor}");
        }
    }
    Err(message)
}

fn parse_version(value: &Value, error: &'static str) -> Result<(u32, u32), &'static str> {
    let Object(version) = value else {
        return component(value).map(|major| (major, 0)).ok_or(error);
    };
    let major = member(version, b"major").ok_or("'version' object 'major' member missing")?;
    let major =
        component(major).ok_or("'version' object 'major' member is not a non-negative integer")?;
    let minor = match member(version, b"minor") {
        None => 0,
        Some(minor) => component(minor)
            .ok_or("'version' object 'minor' member is not a non-negative integer")?,
    };
    Ok((major, minor))
}

fn component(value: &Value) -> Option<u32> {
    match *value {
        Value::Int(n) => u32::try_from(n).ok(),
        Value::UInt(n) => u32::try_from(n).ok(),
        Value::Real(n) if (0.0..=f64::from(u32::MAX)).contains(&n) && n.fract() == 0.0 => {
            Some(n as u32)
        }
        _ => None,
    }
}
