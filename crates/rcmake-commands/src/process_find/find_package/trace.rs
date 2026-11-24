use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::Cx;
use rcmake_host::path;

pub(super) struct SearchTrace {
    body: Option<BString>,
    candidates: Vec<BString>,
}

impl SearchTrace {
    pub(super) fn new(cx: &Cx<'_>, name: &[u8]) -> Self {
        let enabled = cx.find_package_debug(name);
        Self {
            body: enabled.then(BString::default),
            candidates: Vec::new(),
        }
    }

    pub(super) fn section<'a>(
        &mut self,
        heading: &[u8],
        paths: impl IntoIterator<Item = &'a PathBuf>,
    ) {
        if let Some(body) = &mut self.body {
            body.extend_from_slice(heading);
            body.push(b'\n');
            let mut paths = paths.into_iter().peekable();
            if paths.peek().is_none() {
                body.extend_from_slice(b"  none\n");
            } else {
                for path in paths {
                    append_path(body, &path::from_native(path));
                }
            }
        }
    }

    pub(super) fn heading(&mut self, heading: &[u8]) {
        if let Some(body) = &mut self.body {
            body.extend_from_slice(heading);
            body.push(b'\n');
        }
    }

    pub(super) fn candidate(&mut self, dir: &Path, filename: &Path) {
        if self.body.is_some() {
            self.candidates.push(path::from_native(&dir.join(filename)));
        }
    }

    pub(super) fn module(&mut self, name: &[u8], paths: &[PathBuf], found: Option<&Path>) {
        let heading = super::text(&[
            b"find_package considered the following paths for ",
            name,
            b":",
        ]);
        if paths.is_empty() {
            self.heading(&heading);
        } else {
            self.section(&heading, paths);
        }
        let found = found.map(path::from_native);
        self.result(found.as_ref().map(|value| value.as_slice()));
    }

    pub(super) fn module_not_found(&mut self, found_var: &[u8]) {
        if let Some(body) = &mut self.body {
            body.extend_from_slice(&super::text(&[
                b"\nThe module is considered not found due to ",
                found_var,
                b" being FALSE.",
            ]));
        }
    }

    pub(super) fn config(&mut self, name: &[u8], found: Option<&[u8]>, direct: bool) {
        if let Some(body) = &mut self.body {
            if !direct {
                body.extend_from_slice(&super::text(&[
                    b"find_package considered the following locations for ",
                    name,
                    b"'s Config module:\n",
                ]));
            }
            for path in self.candidates.drain(..) {
                append_path(body, &path);
            }
        }
        if !direct {
            self.result(found);
        }
    }

    fn result(&mut self, found: Option<&[u8]>) {
        if let Some(body) = &mut self.body {
            if let Some(file) = found {
                body.extend_from_slice(b"The file was found at\n");
                append_path(body, file);
            } else {
                body.extend_from_slice(b"The file was not found.\n");
            }
        }
    }

    pub(super) fn emit(self, cx: &mut Cx<'_>) {
        if let Some(body) = self.body.filter(|body| !body.is_empty()) {
            cx.issue(MessageKind::Log, Effect::None, None, body);
        }
    }
}

fn append_path(body: &mut BString, path: &[u8]) {
    body.extend_from_slice(b"  ");
    body.extend_from_slice(path);
    body.push(b'\n');
}
