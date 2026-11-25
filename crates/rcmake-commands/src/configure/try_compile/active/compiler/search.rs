// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use rcmake_store::TreePolicy;

impl CompilerEvidence {
    pub(super) fn search_directories(
        &self,
        request: &RunRequest,
        flags: &Flags,
    ) -> Option<BTreeSet<PathBuf>> {
        let compile = flags.source.is_some();
        if let Some(source) = &flags.source
            && self.toolchain.family == Family::AppleClang
        {
            let file = request.cwd.join(path::to_native(source));
            let bytes = self
                .host
                .read(&file, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            if !bytes
                .iter()
                .any(|byte| matches!(byte, b'#' | b'%' | b'?' | b'\\'))
                && !bytes
                    .windows(b"_Pragma".len())
                    .any(|word| word == b"_Pragma")
                && !bytes
                    .windows(b"__pragma".len())
                    .any(|word| word == b"__pragma")
                && (!self.cxx || !bytes.windows(b"import".len()).any(|word| word == b"import"))
            {
                return Some(BTreeSet::new());
            }
        }
        let mut directories: BTreeSet<_> = if compile {
            flags.include_directories.iter()
        } else {
            flags.library_directories.iter()
        }
        .cloned()
        .collect();
        if compile && let Some((file, _)) = &self.external_source {
            directories.insert(file.parent()?.to_owned());
        }
        let env = &request.env;
        for (name, value) in env {
            let name = name.to_ascii_uppercase();
            if if compile {
                matches!(
                    name.as_slice(),
                    b"CPATH"
                        | b"C_INCLUDE_PATH"
                        | b"CPLUS_INCLUDE_PATH"
                        | b"OBJC_INCLUDE_PATH"
                        | b"INCLUDE"
                )
            } else {
                matches!(name.as_slice(), b"LIBRARY_PATH" | b"LIB" | b"LIBPATH")
            } {
                for directory in std::env::split_paths(&path::to_native(value)) {
                    directories.insert(request.cwd.join(directory));
                }
            }
        }
        if self.toolchain.family == Family::Msvc {
            for variable in &flags.search {
                let name = variable.strip_prefix(b"/external:env:")?;
                let value = env
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))?
                    .1
                    .clone();
                directories.extend(
                    std::env::split_paths(&path::to_native(&value))
                        .map(|file| request.cwd.join(file)),
                );
            }
        } else if compile {
            let mut search = flags.search.clone();
            for pair in search.windows(2) {
                if let [key, value] = pair
                    && matches!(
                        key.as_slice(),
                        b"-I" | b"-isystem" | b"-iquote" | b"-idirafter" | b"-F" | b"-iframework"
                    )
                    && !path::to_native(value).is_absolute()
                {
                    return None;
                }
            }
            search.extend([
                "-x".into(),
                if self.cxx { "c++".into() } else { "c".into() },
                "-E".into(),
                "-v".into(),
                "-".into(),
            ]);
            let key = (
                self.toolchain.driver.clone(),
                search.clone(),
                process_environment_fingerprint(env),
            );
            let mut memo = self.searches.lock().ok()?;
            let found = if let Some((paths, present)) = memo.get(&key)
                && availability(self.host.as_ref(), paths)? == *present
            {
                paths.clone()
            } else {
                let output = query_in(
                    self.host.as_ref(),
                    &self.toolchain.driver,
                    &search,
                    &request.cwd,
                    env.clone(),
                )?;
                let paths = include_searches(&output.stderr)?;
                if memo.len() < 128 {
                    memo.insert(
                        key,
                        (paths.clone(), availability(self.host.as_ref(), &paths)?),
                    );
                }
                paths
            };
            directories.extend(found);
        } else if self.toolchain.family == Family::AppleClang {
            directories.insert(PathBuf::from("/usr/local/lib"));
            if flags.frameworks {
                directories.insert(PathBuf::from("/Library/Frameworks"));
            }
        } else {
            let output = query_in(
                self.host.as_ref(),
                &self.toolchain.driver,
                &["-print-search-dirs".into()],
                &request.cwd,
                env.clone(),
            )?;
            let locations = output
                .stdout
                .split(|&byte| byte == b'\n')
                .find_map(|line| line.strip_prefix(b"libraries: ="))?;
            directories.extend(std::env::split_paths(&path::to_native(locations)));
        }
        Some(directories)
    }

    pub(super) fn coverage(
        &self,
        directory: &Path,
        scratch: &Path,
        compile: bool,
    ) -> Option<Option<InputStamp>> {
        if directory == scratch || private_path(scratch, directory) {
            return Some(None);
        }
        let stamp = match self.host.canonicalize(directory) {
            Ok(canonical)
                if self
                    .toolchain
                    .sealed
                    .iter()
                    .any(|root| canonical.starts_with(root)) =>
            {
                if !compile {
                    return Some(None);
                }
                InputStamp::TreeListing {
                    path: directory.to_owned(),
                    policy: TreePolicy::Sdk,
                    digest: self.memo.sdk_digest(self.host.as_ref(), directory).ok()?,
                }
            }
            Ok(_) => InputStamp::TreeListing {
                path: directory.to_owned(),
                policy: TreePolicy::Ordinary,
                digest: self.memo.tree_digest(self.host.as_ref(), directory).ok()?,
            },
            Err(error)
                if error.kind == ErrorKind::NotFound
                    && self
                        .host
                        .metadata(directory, Follow::No)
                        .is_err_and(|error| error.kind == ErrorKind::NotFound) =>
            {
                InputStamp::Missing {
                    path: directory.to_owned(),
                }
            }
            Err(_) => return None,
        };
        Some(Some(stamp))
    }

    pub(super) fn header_is_covered(
        &self,
        file: &Path,
        scratch: &Path,
        trace: &[InputStamp],
    ) -> bool {
        if private_path(scratch, file)
            || trace
                .iter()
                .any(|stamp| matches!(stamp, InputStamp::FileHash { path, .. } if path == file))
        {
            return true;
        }
        if let Some(sdk) = &self.toolchain.sdk
            && file
                .file_name()
                .is_some_and(|name| name == "SDKSettings.json")
            && self.host.canonicalize(file).as_ref() == Ok(&sdk.join("SDKSettings.json"))
        {
            // AppleClang reports the SDK metadata with its driver-selected spelling.
            // Toolchain discovery already stamped the canonical SDK metadata.
            return true;
        }
        let Some(parent) = file.parent() else {
            return false;
        };
        let roots = trace.iter().filter_map(|stamp| match stamp {
            InputStamp::TreeListing { path, .. } => Some(path),
            _ => None,
        });
        let Ok(resolved) = self.host.canonicalize(file) else {
            return false;
        };
        let Some(resolved_parent) = resolved.parent() else {
            return false;
        };
        let quoted_parent = self.host.canonicalize(parent).ok();
        roots.into_iter().any(|root| {
            let resolved_root = self.host.canonicalize(root).ok();
            let canonical_covered = resolved_root
                .as_ref()
                .is_some_and(|root| resolved_parent.starts_with(root));
            // An SDK graph follows directory links. A file link can change the
            // parent used for a quoted include, which needs its own coverage.
            if parent.starts_with(root) {
                return canonical_covered || quoted_parent.as_deref() == Some(resolved_parent);
            }
            canonical_covered && quoted_parent.as_deref() == Some(resolved_parent)
        })
    }
}

fn availability(host: &dyn Host, paths: &[PathBuf]) -> Option<Vec<bool>> {
    paths
        .iter()
        .map(|path| match host.metadata(path, Follow::Yes) {
            Ok(stat) if stat.kind == FileKind::Dir => Some(true),
            Err(error) if error.kind == ErrorKind::NotFound => Some(false),
            _ => None,
        })
        .collect()
}

fn include_searches(output: &[u8]) -> Option<Vec<PathBuf>> {
    let mut searching = false;
    let mut found = false;
    let mut paths = BTreeSet::new();
    for line in output.split(|&byte| byte == b'\n').map(<[u8]>::trim_ascii) {
        if line.starts_with(b"#include ") && line.ends_with(b" search starts here:") {
            searching = true;
        } else if line == b"End of search list." {
            searching = false;
            found = true;
        } else if searching || line.starts_with(b"ignoring nonexistent directory ") {
            let value = if searching {
                line.strip_suffix(b" (framework directory)").unwrap_or(line)
            } else {
                line.strip_prefix(b"ignoring nonexistent directory \"")?
                    .strip_suffix(b"\"")?
            };
            let directory = path::to_native(value);
            if !directory.is_absolute() {
                return None;
            }
            paths.insert(directory);
        }
    }
    found.then(|| paths.into_iter().collect())
}
