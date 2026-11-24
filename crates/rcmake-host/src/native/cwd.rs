use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub(super) fn logical_cwd(physical: &Path, pwd: Option<&Path>) -> PathBuf {
    let Some(pwd) = pwd.filter(|path| path.is_absolute() && *path != physical) else {
        return physical.to_path_buf();
    };
    let Ok(actual) = fs::metadata(physical) else {
        return physical.to_path_buf();
    };
    let is_current_dir = |path: &Path| {
        fs::metadata(path)
            .is_ok_and(|meta| meta.dev() == actual.dev() && meta.ino() == actual.ino())
    };
    if !is_current_dir(pwd) {
        return physical.to_path_buf();
    }

    let mut logical = PathBuf::from("/");
    for part in pwd.components() {
        match part {
            Component::Normal(name) => logical.push(name),
            Component::ParentDir => {
                logical.pop();
            }
            _ => {}
        }
    }
    if logical != pwd && !is_current_dir(&logical) {
        return physical.to_path_buf();
    }
    directory_spelling(physical, &logical)
}

fn directory_spelling(physical: &Path, logical: &Path) -> PathBuf {
    use std::os::unix::fs::DirEntryExt;

    let mut result = PathBuf::from("/");
    for name in logical.iter().skip(1) {
        let candidate = result.join(name);
        if physical.starts_with(&candidate) {
            result = candidate;
            continue;
        }
        // Query the entry itself to preserve the name of a symbolic link in PWD.
        let spelling = fs::symlink_metadata(&candidate).ok().and_then(|meta| {
            let entries = fs::read_dir(&result).ok()?;
            let mut matching = None;
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                if entry_name == name {
                    return Some(entry_name);
                }
                if entry.ino() == meta.ino() {
                    matching = Some(entry_name);
                }
            }
            matching
        });
        result.push(spelling.as_deref().unwrap_or(name));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn logical_cwd_uses_directory_spelling_and_preserves_symbolic_links() -> io::Result<()> {
        let dir = std::env::temp_dir()
            .canonicalize()?
            .join(format!("rcmake-logical-cwd-{}", std::process::id()));
        fs::create_dir(&dir)?;
        let result = (|| -> io::Result<()> {
            let physical = dir.join("Source/Leaf");
            fs::create_dir_all(&physical)?;
            symlink("Source", dir.join("Alias"))?;

            let pwd = dir.join("Alias/../Alias/./Leaf/");
            assert_eq!(logical_cwd(&physical, Some(&pwd)), dir.join("Alias/Leaf"));
            assert_eq!(
                logical_cwd(&physical, Some(Path::new("Source/Leaf"))),
                physical
            );
            let missing = dir.join("Missing/../Alias/Leaf");
            assert_eq!(logical_cwd(&physical, Some(&missing)), physical);
            assert_eq!(logical_cwd(&physical, Some(&dir)), physical);

            let lower = dir.join("alias/leaf");
            if lower.is_dir() {
                assert_eq!(logical_cwd(&physical, Some(&lower)), dir.join("Alias/Leaf"));
                assert_eq!(
                    logical_cwd(&physical, Some(&dir.join("source/leaf"))),
                    physical
                );
            }
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&dir);
        result.and(cleanup)
    }
}
