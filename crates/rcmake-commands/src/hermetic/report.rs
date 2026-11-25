//! Write a stable report with no environment values or process arguments.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rcmake_eval::HermeticMode;
use rcmake_eval::hermetic::{ViolationKind, display_remote, windows_path_spelling};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, WriteMode};
use rcmake_json::{Value, write_compact};

use super::AuditState;

const MAX_INPUTS: usize = 4096;
const MAX_PATH_CHARS: usize = 1024;
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum InputKind {
    Env,
    Network,
    Path,
    Program,
}

impl InputKind {
    fn name(self) -> &'static str {
        match self {
            Self::Env => "environment",
            Self::Network => "network",
            Self::Path => "path",
            Self::Program => "program",
        }
    }
}

pub(super) struct Observations {
    roots: Vec<(&'static str, PathBuf)>,
    inputs: BTreeSet<(InputKind, String)>,
    truncated: bool,
}

impl Observations {
    pub(super) fn new(
        host: &dyn Host,
        source: &Path,
        build: &Path,
        modules: Option<&Path>,
    ) -> Self {
        let mut roots = Vec::new();
        for (label, root) in [
            ("{source}", Some(source)),
            ("{build}", Some(build)),
            ("{modules}", modules),
        ] {
            if let Some(root) = root {
                roots.push((label, root.to_owned()));
                if let Ok(canonical) = host.canonicalize(root)
                    && canonical != root
                {
                    roots.push((label, canonical));
                }
            }
        }
        roots.sort_by_key(|(_, root)| std::cmp::Reverse(root.components().count()));
        Self {
            roots,
            inputs: BTreeSet::new(),
            truncated: false,
        }
    }

    pub(super) fn env(&mut self, key: &[u8]) {
        self.insert(
            InputKind::Env,
            String::from_utf8_lossy(key)
                .chars()
                .take(MAX_PATH_CHARS)
                .collect(),
        );
    }

    pub(super) fn path(&mut self, kind: InputKind, path: &Path) {
        self.insert(kind, self.path_label(path));
    }

    pub(super) fn network(&mut self, url: &str) {
        self.insert(InputKind::Network, display_remote(url));
    }

    pub(super) fn project_file(&self, file: &Path) -> bool {
        self.roots
            .iter()
            .any(|(label, root)| *label == "{source}" && relative_path(file, root).is_some())
    }

    fn path_label(&self, path: &Path) -> String {
        self.roots
            .iter()
            .find_map(|(label, root)| {
                let relative = relative_path(path, root)?;
                if relative.as_os_str().is_empty() {
                    return Some((*label).into());
                }
                if !relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                {
                    return None;
                }
                let text = relative.to_string_lossy().replace('\\', "/");
                Some(
                    format!("{label}/{text}")
                        .chars()
                        .take(MAX_PATH_CHARS)
                        .collect(),
                )
            })
            .unwrap_or_else(|| "<external path>".into())
    }

    fn insert(&mut self, kind: InputKind, value: String) {
        self.inputs.insert((kind, value));
        if self.inputs.len() > MAX_INPUTS {
            self.inputs.pop_last();
            self.truncated = true;
        }
    }
}

fn relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    let path = windows_path_spelling(path);
    let root = windows_path_spelling(root);
    if let Ok(relative) = path.strip_prefix(root.as_ref()) {
        return Some(relative.to_owned());
    }
    if !cfg!(windows) {
        return None;
    }
    let mut components = path.components();
    for prefix in root.components() {
        if !components
            .next()?
            .as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(prefix.as_os_str().as_encoded_bytes())
        {
            return None;
        }
    }
    Some(components.as_path().to_owned())
}

pub(super) fn render(state: &AuditState, mode: HermeticMode, denied: bool) -> Vec<u8> {
    let observed = state
        .inputs
        .inputs
        .iter()
        .map(|(kind, value)| {
            object([
                ("kind", Value::string(kind.name())),
                ("input", Value::string(value.as_str())),
            ])
        })
        .collect();
    let mut violations = state
        .auditor
        .records()
        .map(|record| {
            let file = state.inputs.path_label(record.site.file());
            let value = match record.kind {
                ViolationKind::UndeclaredEnvRead | ViolationKind::NetworkAccess => &record.value,
                ViolationKind::OutOfTreePathRead => "<external path>",
                ViolationKind::UnapprovedProcess => "<unapproved executable>",
            };
            (file, record.site.line(), record.kind.code(), value)
        })
        .collect::<Vec<_>>();
    violations.sort_unstable();
    let violations = violations
        .into_iter()
        .map(|(file, line, code, value)| {
            object([
                ("code", Value::string(code)),
                ("file", Value::string(file)),
                ("line", Value::UInt(u64::from(line))),
                ("input", Value::string(value)),
            ])
        })
        .collect();
    let mode = match mode {
        HermeticMode::Off => "off",
        HermeticMode::Warn => "warn",
        HermeticMode::Strict => "strict",
    };
    let mut contents = write_compact(&object([
        ("version", Value::UInt(1)),
        ("mode", Value::string(mode)),
        ("denied", Value::Bool(denied)),
        ("inputs", Value::Array(observed)),
        ("inputs_truncated", Value::Bool(state.inputs.truncated)),
        ("violations", Value::Array(violations)),
        (
            "violations_omitted",
            Value::UInt(state.auditor.summary().omitted as u64),
        ),
    ]));
    contents.push(b'\n');
    contents.into()
}

pub(super) fn publish(host: &dyn Host, build: &Path, bytes: &[u8]) -> HostResult<()> {
    host.create_dir_all(build)?;
    let mut directory = host.canonicalize(build)?;
    for part in ["CMakeFiles", "rcmake"] {
        directory.push(part);
        match host.metadata(&directory, Follow::No) {
            Ok(metadata) if metadata.kind == FileKind::Dir => {}
            Ok(_) => {
                return Err(HostError::unsupported(
                    "The report path must be a directory.",
                ));
            }
            Err(error) if error.kind == ErrorKind::NotFound => host.create_dir_all(&directory)?,
            Err(error) => return Err(error),
        }
    }
    let path = directory.join("inputs.v1.json");
    let pid = host.system().pid;
    for _ in 0..32 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = directory.join(format!(".inputs.v1.{pid}.{sequence}.tmp"));
        match host.write(&temp, bytes, WriteMode::CreateNew) {
            Ok(()) => {
                let result = host.rename(&temp, &path);
                if result.is_err() {
                    let _ = host.remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(HostError::unsupported(
        "Cannot reserve a temporary report file.",
    ))
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
