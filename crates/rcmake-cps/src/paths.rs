use rcmake_base::BString;

use crate::{CpsError, CpsPackage};

const PREFIX: &[u8] = b"@prefix@";

pub(crate) struct Paths {
    directory: Vec<u8>,
    prefix: Vec<u8>,
}

impl Paths {
    pub(crate) fn new(package: &CpsPackage, path: &[u8]) -> Result<Self, CpsError> {
        let path = slashes(path);
        let directory = match path.iter().rposition(|byte| *byte == b'/') {
            Some(0) => b"/".to_vec(),
            Some(2) if path.get(1..2) == Some(b":") => path.iter().take(3).copied().collect(),
            Some(end) => path.iter().take(end).copied().collect(),
            None => b".".to_vec(),
        };
        let prefix = if let Some(prefix) = &package.prefix {
            let prefix = slashes(prefix);
            is_absolute(&prefix).then_some(prefix)
        } else {
            package
                .cps_path
                .as_ref()
                .and_then(|cps_path| find_prefix(&directory, &slashes(cps_path)))
        };
        let Some(prefix) = prefix else {
            return Err(CpsError::Invalid(
                "The CPS file location does not match an absolute prefix or cps_path.".into(),
            ));
        };
        Ok(Self { directory, prefix })
    }

    pub(crate) fn resolve(&self, path: &mut BString) {
        let normalized = slashes(path);
        if let Some(expanded) = self.expand(&normalized) {
            *path = expanded;
        } else if is_absolute(&normalized) || normalized.is_empty() {
            *path = normalized.into();
        } else {
            *path = join(&self.directory, &normalized);
        }
    }

    pub(crate) fn substitute(&self, path: &mut BString) {
        if path.starts_with(PREFIX)
            && let Some(expanded) = self.expand(&slashes(path))
        {
            *path = expanded;
        }
    }

    fn expand(&self, path: &[u8]) -> Option<BString> {
        let suffix = prefix_suffix(path)?;
        Some(join(
            &self.prefix,
            suffix.strip_prefix(b"/").unwrap_or(suffix),
        ))
    }
}

fn slashes(path: &[u8]) -> Vec<u8> {
    path.iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect()
}

fn is_absolute(path: &[u8]) -> bool {
    path.starts_with(b"/")
        || path.get(1..3) == Some(b":/") && path.first().is_some_and(u8::is_ascii_alphabetic)
}

fn prefix_suffix(path: &[u8]) -> Option<&[u8]> {
    path.strip_prefix(PREFIX)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with(b"/"))
}

fn find_prefix(directory: &[u8], cps_path: &[u8]) -> Option<Vec<u8>> {
    let suffix = prefix_suffix(cps_path)?;
    let suffix = suffix.strip_suffix(b"/").unwrap_or(suffix);
    if suffix.is_empty() {
        return Some(directory.to_vec());
    }
    let base = if directory.get(1..2) == Some(b":") || directory.starts_with(b"//") {
        let start = directory.len().checked_sub(suffix.len())?;
        let (base, tail) = directory.split_at_checked(start)?;
        tail.eq_ignore_ascii_case(suffix).then_some(base)?
    } else {
        directory.strip_suffix(suffix)?
    };
    if base.is_empty() {
        return Some(b"/".to_vec());
    }
    if base.get(1..) == Some(b":") {
        return Some([base, b"/"].concat());
    }
    Some(base.to_vec())
}

fn join(base: &[u8], name: &[u8]) -> BString {
    let mut path = base.to_vec();
    if !name.is_empty() {
        if !base.ends_with(b"/") {
            path.push(b'/');
        }
        path.extend_from_slice(name);
    }
    path.into()
}
