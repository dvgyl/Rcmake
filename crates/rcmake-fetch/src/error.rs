use std::{fmt, path::PathBuf};

use crate::FetchError;

/// Declaration and cache information for an archive error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveContext {
    /// FetchContent dependency name, when supplied.
    pub dependency: Option<String>,
    /// Ordered URLs with credentials, queries, and fragments removed.
    pub urls: Vec<String>,
    /// Expected content identities in `ALGORITHM=hex` form.
    pub expected: Vec<String>,
    /// Lockfile that supplied the pin, when applicable.
    pub lockfile: Option<PathBuf>,
    /// Cache directory.
    pub cache_root: PathBuf,
    /// Action that can resolve the error.
    pub recovery: &'static str,
}

impl ArchiveContext {
    pub(crate) fn error(self, cause: FetchError) -> FetchError {
        FetchError::Archive {
            context: Box::new(self),
            cause: Box::new(cause),
        }
    }
}

impl fmt::Display for ArchiveContext {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(dependency) = &self.dependency {
            write!(out, "Archive for {dependency:?}. ")?;
        }
        write!(out, "URLs {:?}. Cache {:?}. ", self.urls, self.cache_root)?;
        if !self.expected.is_empty() {
            write!(out, "Expected {}. ", self.expected.join(" and "))?;
        }
        if let Some(path) = &self.lockfile {
            write!(out, "Lockfile {path:?}. ")?;
        }
        out.write_str(self.recovery)
    }
}

// Process output can contain headers or credentials that are not part of the URL.
// Keep it out of public errors. Display only the declared source with secrets removed.
pub(crate) fn sanitized_url(url: &str) -> String {
    let value = if has_url_suffix(url) {
        url.split(['?', '#']).next().unwrap_or_default()
    } else {
        url
    };
    let redacted = if let Some((scheme, location)) = value.split_once("://") {
        let end = location.find(['/', '\\']).unwrap_or(location.len());
        let (authority, path) = location.split_at(end);
        match authority.rsplit_once('@') {
            Some((_, host)) => format!("{scheme}://{host}{path}"),
            None => value.to_owned(),
        }
    } else {
        // Git also accepts [user@]host:path.
        let end = value.find(['/', '\\']).unwrap_or(value.len());
        let (address, path) = value.split_at(end);
        address
            .rsplit_once('@')
            .filter(|(_, host)| host.contains(':'))
            .map_or_else(|| value.to_owned(), |(_, host)| format!("{host}{path}"))
    };
    redacted.chars().filter(|ch| !ch.is_control()).collect()
}

pub(crate) fn has_url_suffix(url: &str) -> bool {
    url.contains(['?', '#'])
        && (url.contains("://") || url.split('/').next().is_some_and(|s| s.contains(':')))
}
