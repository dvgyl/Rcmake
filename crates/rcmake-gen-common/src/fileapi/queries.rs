use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{DirEntry, FileKind, Follow, Host, ReadRange, path::to_native};
use rcmake_json::{ReadOptions, Value};

use super::{
    Array, FileApi, Kind, Members, Object, ReplyWriter, UInt, array, index_name, json_bytes,
    object, read_dir,
};
use crate::{GenError, write_error};

const QUERY_LIMIT: u64 = 4 * 1024 * 1024;

impl FileApi<'_, '_> {
    pub(super) fn read_queries(&mut self, dir: &Path) -> Result<Members, GenError> {
        let host = self.writer.host;
        let old_entries = read_dir(host, self.writer.dir)?;
        let user = user_query_dir(host);
        let mut query_exists = false;
        let mut reply = Members::new();
        let mut tests = BTreeMap::new();
        for root in user.as_deref().into_iter().chain([dir]) {
            query_exists |= host
                .metadata(root, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir);
            for entry in read_dir(host, root)? {
                if reply.contains_key(&entry.name) {
                    continue;
                }
                let response = if entry.name.starts_with(b"client-") {
                    self.client_queries(&dir.join(to_native(&entry.name)), &mut tests)?
                } else {
                    self.stateless(&entry.name, &mut tests)?
                };
                reply.insert(entry.name, response);
            }
        }
        self.writer.retain_file_aliases(&old_entries)?;
        // The caller writes an index for a nonempty reply and lists public objects.
        if (query_exists && reply.is_empty()) || !tests.is_empty() {
            let index = object([
                ("cmake", self.cmake()),
                (
                    "objects",
                    array(self.objects.values().cloned().chain(tests.into_values())),
                ),
                ("reply", Object(reply)),
            ]);
            self.writer
                .write(&index_name(host, &old_entries), &json_bytes(&index))?;
            return Ok(Members::new());
        }
        Ok(reply)
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
    fn retain_file_aliases(&mut self, old_entries: &[DirEntry]) -> Result<(), GenError> {
        if self.files.is_empty() {
            return Ok(());
        }
        let mut by_name = BTreeMap::<_, Vec<&BString>>::new();
        for name in &self.files {
            by_name
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(name);
        }
        let mut aliases = Vec::new();
        for entry in old_entries {
            if entry.kind != FileKind::File || self.files.contains(&entry.name) {
                continue;
            }
            let Some(candidates) = by_name.get(&entry.name.to_ascii_lowercase()) else {
                continue;
            };
            // The old spelling can still refer to a new reply on this filesystem.
            let path = self.dir.join(to_native(&entry.name));
            let metadata = match self.host.metadata(&path, Follow::No) {
                Ok(metadata) => metadata,
                Err(error) if error.kind == ErrorKind::NotFound => continue,
                Err(error) => return Err(write_error(&path, error)),
            };
            for name in candidates {
                let path = self.dir.join(to_native(name));
                let current = self
                    .host
                    .metadata(&path, Follow::No)
                    .map_err(|error| write_error(&path, error))?;
                if current.ino != 0 && (current.dev, current.ino) == (metadata.dev, metadata.ino) {
                    aliases.push(entry.name.clone());
                    break;
                }
            }
        }
        self.files.extend(aliases);
        Ok(())
    }
}

fn error(message: &str) -> Value {
    object([("error", Value::string(message))])
}

fn member<'a>(object: &'a Members, key: &[u8]) -> Option<&'a Value> {
    object.get(key).filter(|value| **value != Value::Null)
}

type TestReplies = BTreeMap<u32, Value>;

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
