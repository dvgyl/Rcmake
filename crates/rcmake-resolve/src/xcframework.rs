use std::{collections::HashMap, sync::Mutex, time::Duration};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{Location, MessageKind};
use rcmake_host::{
    CommandSpec, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest, path,
};
use rcmake_json::Value;
use rcmake_model::{BT, ProjectModel, Target};

use crate::{
    ResolveError,
    lower::{
        apple::{self, Variant},
        join,
    },
    sources::SourcePaths,
};

#[derive(Default)]
pub(crate) struct Plists(Mutex<HashMap<BString, Vec<Entry>>>);

#[derive(Clone)]
struct Entry {
    system: &'static [u8],
    variant: Option<Variant>,
    identifier: BString,
    library: BString,
    headers: Option<BString>,
}

pub(crate) struct Slice {
    pub(crate) library: BString,
    pub(crate) headers: Option<BString>,
}

const MAX_PLIST: u64 = 4 * 1024 * 1024;

impl Plists {
    pub(crate) fn select(
        &self,
        model: &ProjectModel,
        paths: &SourcePaths<'_>,
        target: &Target,
        root: &BString,
        origin: &BT<BString>,
    ) -> Result<Slice, ResolveError> {
        let plist = join(root, b"Info.plist");
        let error = |body: String| {
            let body = BString::from(body);
            paths.emit(
                model,
                MessageKind::Error,
                Location::Backtrace(origin.backtrace.clone()),
                body.clone(),
            );
            ResolveError::Reported(body)
        };
        let cached = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned();
        let entries = match cached {
            Some(entries) => entries,
            None => {
                let value = read_plist(paths.host, &plist)
                    .ok_or_else(|| error(format!("Unable to parse plist file:\n  {plist}")))?;
                let invalid = || error(format!("Invalid xcframework .plist file:\n  {plist}"));
                let Value::Object(object) = value else {
                    return Err(invalid());
                };
                let Some((package, version)) =
                    string(object.get(b"CFBundlePackageType".as_slice()))
                        .zip(string(object.get(b"XCFrameworkFormatVersion".as_slice())))
                else {
                    return Err(invalid());
                };
                if package != "XFWK" || version != "1.0" {
                    return Err(error(format!(
                        "Expected:\n  {plist}\nto have CFBundlePackageType \"XFWK\" and XCFrameworkFormatVersion \"1.0\""
                    )));
                }
                let Some(Value::Array(libraries)) = object.get(b"AvailableLibraries".as_slice())
                else {
                    return Err(invalid());
                };
                let parsed = libraries
                    .iter()
                    .map(parse_entry)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(invalid)?;
                let mut guard = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.entry(root.clone()).or_insert(parsed).clone()
            }
        };
        let system = apple::system(model, target.dir);
        let variant = apple::variant(model, target.dir);
        let entry = entries
            .iter()
            .find(|entry| entry.system == system && entry.variant == variant)
            .ok_or_else(|| {
                error(format!(
                    "Unable to find suitable library in:\n  {plist}\nfor system name \"{}\"",
                    BString::from(system)
                ))
            })?;
        let directory = join(root, &entry.identifier);
        Ok(Slice {
            library: join(&directory, &entry.library),
            headers: entry
                .headers
                .as_ref()
                .map(|headers| join(&directory, headers)),
        })
    }
}

fn string(value: Option<&Value>) -> Option<&BString> {
    let Value::String(value) = value? else {
        return None;
    };
    Some(value)
}

fn parse_entry(value: &Value) -> Option<Entry> {
    let Value::Object(fields) = value else {
        return None;
    };
    let get = |key: &[u8]| fields.get(key);
    let system = apple::plist_system(string(get(b"SupportedPlatform"))?)?;
    let variant = match get(b"SupportedPlatformVariant") {
        None => None,
        Some(value) => match string(Some(value))?.as_slice() {
            b"simulator" => Some(Variant::Simulator),
            b"maccatalyst" => Some(Variant::Catalyst),
            _ => return None,
        },
    };
    let Value::Array(architectures) = get(b"SupportedArchitectures")? else {
        return None;
    };
    if architectures
        .iter()
        .any(|value| string(Some(value)).is_none())
    {
        return None;
    }
    let headers = match get(b"HeadersPath") {
        None => None,
        Some(value) => {
            let value = string(Some(value))?;
            if value.is_empty() {
                None
            } else {
                Some(relative(value)?)
            }
        }
    };
    Some(Entry {
        system,
        variant,
        identifier: relative(string(get(b"LibraryIdentifier"))?)?,
        library: relative(string(get(b"LibraryPath"))?)?,
        headers,
    })
}

fn relative(value: &BString) -> Option<BString> {
    let path = Lexical::new(value);
    (!value.is_empty()
        && !path.absolute()
        && path.root_name().is_empty()
        && !value.contains(&0)
        && !path.components().any(|part| part == b".."))
    .then(|| value.clone())
}

fn read_plist(host: &dyn Host, plist: &[u8]) -> Option<Value> {
    let file = path::to_native(plist);
    if !host.metadata(&file, Follow::Yes).is_ok_and(|metadata| {
        metadata.kind == rcmake_host::FileKind::File && metadata.len <= MAX_PLIST
    }) {
        return None;
    }
    let bytes = host.read(&file, ReadRange::All { limit: MAX_PLIST }).ok()?;
    if bytes.len() as u64 > MAX_PLIST {
        return None;
    }
    let result = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "/usr/bin/plutil".into(),
                    args: ["-convert", "json", "-o", "-", "--", "-"]
                        .map(BString::from)
                        .to_vec(),
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Bytes(bytes),
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(5)),
                tee: false,
                default_error_mode: false,
            },
            &mut |_, _| {},
        )
        .ok()?;
    if result.results != [ProcessResult::Exited(0)] || result.stdout.len() as u64 > MAX_PLIST {
        return None;
    }
    rcmake_json::parse(&result.stdout, &rcmake_json::ReadOptions::default()).ok()
}
