use std::path::Path;

use rcmake_host::{FileKind, Follow, ReadRange, path::to_native};
use rcmake_json::{ReadOptions, Value};

use super::{Array, FileApi, Kind, Members, Object, array, object, read_dir};
use crate::GenError;

const QUERY_LIMIT: u64 = 4 * 1024 * 1024;

impl FileApi<'_, '_> {
    pub(super) fn read_queries(&mut self, dir: &Path) -> Result<Members, GenError> {
        let mut reply = Members::new();
        for entry in read_dir(self.writer.host, dir)? {
            let value = if entry.name.starts_with(b"client-") {
                self.client_queries(&dir.join(to_native(&entry.name)))?
            } else {
                self.stateless(&entry.name)?
            };
            reply.insert(entry.name, value);
        }
        Ok(reply)
    }

    fn client_queries(&mut self, dir: &Path) -> Result<Value, GenError> {
        let mut reply = Members::new();
        for entry in read_dir(self.writer.host, dir)? {
            let value = if entry.name == b"query.json" {
                self.stateful(&dir.join("query.json"))?
            } else {
                self.stateless(&entry.name)?
            };
            reply.insert(entry.name, value);
        }
        Ok(Object(reply))
    }

    fn stateless(&mut self, name: &[u8]) -> Result<Value, GenError> {
        for kind in Kind::ALL {
            if name == format!("{}-v{}", kind.name(), kind.version().0).as_bytes() {
                return self.reference(kind);
            }
        }
        Ok(error("unknown query file"))
    }

    fn stateful(&mut self, path: &Path) -> Result<Value, GenError> {
        let host = self.writer.host;
        if !host
            .metadata(path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Ok(error("failed to read query.json"));
        }
        let Ok(bytes) = host.read(path, ReadRange::All { limit: QUERY_LIMIT }) else {
            return Ok(error("failed to read query.json"));
        };
        let mut query = match rcmake_json::parse(&bytes, &ReadOptions::default()) {
            Ok(Object(query)) => query,
            Ok(_) => return Ok(error("query root is not an object")),
            Err(err) => return Ok(error(format!("query.json:{err}").as_str())),
        };
        let mut reply = Members::new();
        if let Some(client) = query.remove(b"client".as_slice()) {
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
                                Ok(kind) => self.reference(kind)?,
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
}

fn error(message: &str) -> Value {
    object([("error", Value::string(message))])
}

fn member<'a>(object: &'a Members, key: &[u8]) -> Option<&'a Value> {
    object.get(key).filter(|value| **value != Value::Null)
}

fn request_kind(request: &Value) -> Result<Kind, String> {
    let Object(request) = request else {
        return Err("request is not an object".into());
    };
    let Some(name) = member(request, b"kind") else {
        return Err("'kind' member missing".into());
    };
    let Value::String(name) = name else {
        return Err("'kind' member is not a string".into());
    };
    let kind = Kind::ALL
        .into_iter()
        .find(|kind| name == kind.name())
        .ok_or_else(|| format!("unknown request kind '{name}'"))?;
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
    let (major, minor) = kind.version();
    if versions.iter().any(|&(requested_major, requested_minor)| {
        requested_major == major && requested_minor <= minor
    }) {
        return Ok(kind);
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
