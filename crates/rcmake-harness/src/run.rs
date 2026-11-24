use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};

use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::case::Case;

const TIMEOUT: Duration = Duration::from_secs(15);
const SIZE_LIMIT: u64 = 16 * 1024 * 1024;
const TREE_DEPTH_LIMIT: usize = 64;
const TREE_ENTRY_LIMIT: usize = 10_000;

#[derive(Deserialize, PartialEq, Serialize)]
pub(crate) struct Outcome {
    exit_code: i32,
    stdout: Content,
    stderr: Content,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    files: Option<FileTree>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Binary { hex: String },
}

impl Content {
    fn from_bytes(bytes: Vec<u8>, normalize: impl FnOnce(&str) -> String) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text(normalize(&text)),
            Err(error) => {
                let mut hex = String::with_capacity(error.as_bytes().len() * 2);
                for byte in error.as_bytes() {
                    use std::fmt::Write as _;
                    let _ = write!(hex, "{byte:02x}");
                }
                Self::Binary { hex }
            }
        }
    }

    fn for_diff(&self) -> Cow<'_, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(text),
            Self::Binary { hex } => Cow::Owned(format!("hex: {hex}\n")),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum Entry {
    Directory,
    File { content: Content },
    Symlink { target: String },
}

type FileTree = BTreeMap<String, Entry>;

pub(crate) fn execute(exe: &Path, case: &Case) -> Result<Outcome, String> {
    let run = || -> io::Result<Outcome> {
        let sandbox = tempfile::tempdir()?;
        let root = sandbox.path();
        for (path, content) in &case.files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, content)?;
        }

        let mut stdin = tempfile::tempfile()?;
        stdin.write_all(case.stdin.as_bytes())?;
        stdin.rewind()?;
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let sandbox_path = root.to_string_lossy();
        let mut command = Command::new(exe);
        command
            .args(
                case.args
                    .iter()
                    .map(|arg| arg.replace("<TMP>", &sandbox_path)),
            )
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("LANG", "C")
            .env("TZ", "UTC")
            .envs(&case.env)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let status = wait(&mut child, &stdout, &stderr)?;

        let exit_code = status_exit_code(status);
        let normalizer = Normalizer::new(root, exe);
        stdout.rewind()?;
        stderr.rewind()?;
        Ok(Outcome {
            exit_code,
            stdout: Content::from_bytes(read_limited(stdout, SIZE_LIMIT)?, |text| {
                normalizer.normalize(text)
            }),
            stderr: Content::from_bytes(read_limited(stderr, SIZE_LIMIT)?, |text| {
                normalizer.normalize(text)
            }),
            files: case.check_files.then(|| snapshot(root)).transpose()?,
        })
    };
    run().map_err(|error| format!("case {} with {}: {error}", case.name, exe.display()))
}

fn status_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return -signal;
    }
    -1
}

fn wait(child: &mut Child, stdout: &File, stderr: &File) -> io::Result<ExitStatus> {
    let start = Instant::now();
    let poll = || -> io::Result<()> {
        if start.elapsed() >= TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process exceeded 15 seconds",
            ));
        }
        if stdout.metadata()?.len() > SIZE_LIMIT || stderr.metadata()?.len() > SIZE_LIMIT {
            return Err(io::Error::other("process output exceeds the size limit"));
        }
        Ok(())
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if let Err(error) = poll() {
                    stop(child);
                    return Err(error);
                }
            }
            Err(error) => {
                stop(child);
                return Err(error);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop(child: &mut Child) {
    #[cfg(unix)]
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn check_size(size: u64) -> io::Result<()> {
    if size > SIZE_LIMIT {
        return Err(io::Error::other("file exceeds the size limit"));
    }
    Ok(())
}

pub(crate) fn read_text(path: &Path) -> io::Result<String> {
    String::from_utf8(read_limited(File::open(path)?, SIZE_LIMIT)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_limited(file: File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("file exceeds the size limit"));
    }
    Ok(bytes)
}

fn snapshot(root: &Path) -> io::Result<FileTree> {
    let mut entries = BTreeMap::new();
    let mut remaining = SIZE_LIMIT;
    let mut dirs = vec![(PathBuf::new(), 0)];
    while let Some((relative, depth)) = dirs.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let name = entry.file_name().into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "file name is not valid UTF-8")
            })?;
            if name.starts_with("._") {
                continue;
            }
            if entries.len() >= TREE_ENTRY_LIMIT || depth >= TREE_DEPTH_LIMIT {
                return Err(io::Error::other("file tree exceeds the snapshot limit"));
            }
            let kind = entry.file_type()?;
            let path = relative.join(&name);
            let data = if kind.is_file() {
                let bytes = read_limited(File::open(entry.path())?, remaining)?;
                remaining -= bytes.len() as u64;
                Entry::File {
                    content: Content::from_bytes(bytes, str::to_owned),
                }
            } else if kind.is_dir() {
                dirs.push((path.clone(), depth + 1));
                Entry::Directory
            } else if kind.is_symlink() {
                let target = fs::read_link(entry.path())?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "symlink target is not valid UTF-8",
                        )
                    })?;
                Entry::Symlink { target }
            } else {
                return Err(io::Error::other(format!(
                    "unsupported file type at {}",
                    entry.path().display()
                )));
            };
            let key = path.to_string_lossy();
            let key = if cfg!(windows) {
                key.replace('\\', "/")
            } else {
                key.into_owned()
            };
            entries.insert(key, data);
        }
    }
    Ok(entries)
}

struct Normalizer(Vec<(String, &'static str)>);

impl Normalizer {
    fn new(sandbox: &Path, exe: &Path) -> Self {
        let mut paths = Vec::new();
        add_paths(&mut paths, sandbox, "<TMP>");
        add_paths(&mut paths, exe, "<CMAKE_EXE>");
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            add_paths(&mut paths, &prefix.join("share/cmake-4.5"), "<CMAKE_ROOT>");
        }
        paths.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        Self(paths)
    }

    fn normalize(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            if line.starts_with("CMake suite maintained and supported by Kitware")
                || line.starts_with("Rcmake native Rust drop-in replacement for CMake")
            {
                output.push_str("<VERSION_ATTRIBUTION>");
                if line.ends_with("\r\n") {
                    output.push_str("\r\n");
                } else if line.ends_with('\n') {
                    output.push('\n');
                }
            } else {
                let mut normalized = line.to_owned();
                for (path, replacement) in &self.0 {
                    normalized = normalized.replace(path, replacement);
                }
                output.push_str(&normalized);
            }
        }
        output
    }
}

fn add_paths(paths: &mut Vec<(String, &'static str)>, path: &Path, token: &'static str) {
    let canonical = path.canonicalize().ok();
    for path in [Some(path), canonical.as_deref()].into_iter().flatten() {
        add_spelling(paths, path, token);
        if (path.starts_with("/private/tmp") || path.starts_with("/private/var"))
            && let Ok(short) = path.strip_prefix("/private")
        {
            add_spelling(paths, &Path::new("/").join(short), token);
        } else if (path.starts_with("/tmp") || path.starts_with("/var"))
            && let Ok(short) = path.strip_prefix("/")
        {
            add_spelling(paths, &Path::new("/private").join(short), token);
        }
    }
}

fn add_spelling(paths: &mut Vec<(String, &'static str)>, path: &Path, token: &'static str) {
    let native = path.to_string_lossy();
    if cfg!(windows) {
        let cmake = native.replace('\\', "/");
        if cmake != native {
            paths.push((cmake, token));
        }
    }
    paths.push((native.into_owned(), token));
}

impl Outcome {
    pub(crate) fn diff(&self, actual: &Self, expected_label: &str) -> serde_json::Result<String> {
        let mut diff = String::new();
        let mut append = |field: &str, expected: &str, actual: &str| {
            diff.push_str(
                &TextDiff::configure()
                    .timeout(Duration::from_secs(1))
                    .diff_lines(expected, actual)
                    .unified_diff()
                    .header(
                        &format!("{expected_label}/{field}"),
                        &format!("rcmake/{field}"),
                    )
                    .to_string(),
            );
        };
        if self.exit_code != actual.exit_code {
            append(
                "exit_code",
                &format!("{}\n", self.exit_code),
                &format!("{}\n", actual.exit_code),
            );
        }
        for (stream, expected, actual) in [
            ("stdout", &self.stdout, &actual.stdout),
            ("stderr", &self.stderr, &actual.stderr),
        ] {
            if expected != actual {
                if std::mem::discriminant(expected) == std::mem::discriminant(actual) {
                    append(stream, &expected.for_diff(), &actual.for_diff());
                } else {
                    append(
                        stream,
                        &(serde_json::to_string(expected)? + "\n"),
                        &(serde_json::to_string(actual)? + "\n"),
                    );
                }
            }
        }
        if self.files != actual.files {
            append(
                "files",
                &(serde_json::to_string_pretty(&self.files)? + "\n"),
                &(serde_json::to_string_pretty(&actual.files)? + "\n"),
            );
        }
        Ok(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_and_preserves_line_endings() {
        let exe = Path::new("/tmp/install/bin/cmake");
        let normalizer = Normalizer::new(Path::new("/private/tmp/sandbox"), exe);
        let raw = concat!(
            "/tmp/sandbox /private/tmp/sandbox\n",
            "/tmp/install/bin/cmake /private/tmp/install/bin/cmake\n",
            "/tmp/install/share/cmake-4.5/Modules /private/tmp/install/share/cmake-4.5\n",
            "CMake suite maintained and supported by Kitware (kitware.com/cmake).\r\n",
            "Rcmake native Rust drop-in replacement for CMake 4.5.1.\n",
            "last line",
        );
        assert_eq!(
            normalizer.normalize(raw),
            "<TMP> <TMP>\n<CMAKE_EXE> <CMAKE_EXE>\n<CMAKE_ROOT>/Modules <CMAKE_ROOT>\n<VERSION_ATTRIBUTION>\r\n<VERSION_ATTRIBUTION>\nlast line"
        );
        let normalizer = Normalizer::new(Path::new("/var/folders/test"), exe);
        assert_eq!(
            normalizer.normalize("/private/var/folders/test\n"),
            "<TMP>\n"
        );
    }

    #[test]
    fn snapshots_nested_files_and_symlinks() -> io::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let root = sandbox.path();
        fs::create_dir_all(root.join("nested/empty"))?;
        fs::write(root.join("nested/text"), "first\nsecond")?;
        fs::write(root.join("nested/binary"), [0, 255])?;
        fs::write(root.join("nested/._ignored"), "metadata")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("..", root.join("nested/loop"))?;
        let prior = snapshot(root)?;
        assert_eq!(prior.get("nested/empty"), Some(&Entry::Directory));
        assert_eq!(
            prior.get("nested/text"),
            Some(&Entry::File {
                content: Content::Text("first\nsecond".to_owned())
            })
        );
        assert_eq!(
            prior.get("nested/binary"),
            Some(&Entry::File {
                content: Content::Binary {
                    hex: "00ff".to_owned()
                }
            })
        );
        assert!(!prior.contains_key("nested/._ignored"));
        #[cfg(unix)]
        assert_eq!(
            prior.get("nested/loop"),
            Some(&Entry::Symlink {
                target: "..".to_owned()
            })
        );
        fs::remove_file(root.join("nested/text"))?;
        assert!(!snapshot(root)?.contains_key("nested/text"));
        Ok(())
    }
}
