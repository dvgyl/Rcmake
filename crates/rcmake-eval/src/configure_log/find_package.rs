use rcmake_base::BString;

use super::{yaml_bool, yaml_value};
use rcmake_json::write_string;

use crate::{Cx, RunMode};

/// Fields for one package search in the configure log.
#[derive(Default)]
pub struct FindPackageEvent<'a> {
    /// Package name.
    pub name: &'a [u8],
    /// Component names, required flags, and results.
    pub components: Vec<(&'a [u8], bool, bool)>,
    /// Configuration filenames and their description types.
    pub configs: Vec<(&'a [u8], &'static [u8])>,
    /// Requested version and range fields.
    pub version: Vec<(&'static [u8], &'a [u8])>,
    /// Whether the request requires an exact version.
    pub exact: bool,
    /// The source of package requiredness.
    pub required: &'static [u8],
    /// Boolean request settings.
    pub flags: Vec<(&'static [u8], bool)>,
    /// List request settings.
    pub lists: Vec<(&'static [u8], &'a [BString])>,
    /// Requested registry view, if specified.
    pub registry_view: Option<&'a [u8]>,
    /// Effective search path switches.
    pub paths: Vec<(&'static [u8], bool)>,
    /// Effective root path mode.
    pub root_mode: &'static [u8],
    /// Paths that the search rejected.
    pub candidates: Vec<PackageCandidate<'a>>,
    /// Selected file, mode, and version.
    pub found: Option<(&'a [u8], &'static [u8], &'a [u8])>,
    /// Package root paths from innermost to outermost call.
    pub package_stack: Vec<&'a [BString]>,
    /// Search variables in configure log order.
    pub context: Vec<(BString, SearchValue)>,
}

/// A path that the package search did not select.
type PackageCandidate<'a> = (&'a [u8], &'static [u8], &'static [u8], &'a [u8]);

/// The scalar or list value of a search variable.
pub enum SearchValue {
    /// A variable read as a string.
    String(BString),
    /// A variable read as a list of paths.
    List(Vec<BString>),
}

impl FindPackageEvent<'_> {
    /// Add a search variable with a scalar value.
    pub fn context_string(&mut self, key: &[u8], value: BString) {
        self.context.push((key.into(), SearchValue::String(value)));
    }

    /// Add a search variable when its path list is nonempty.
    pub fn context_list(&mut self, key: &[u8], values: Vec<BString>) {
        if !values.is_empty() {
            self.context.push((key.into(), SearchValue::List(values)));
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        yaml_value(out, b"    name", self.name);
        if !self.components.is_empty() {
            out.extend_from_slice(b"    components:\n");
            for &(name, required, found) in &self.components {
                out.extend_from_slice(b"      -\n");
                yaml_value(out, b"        name", name);
                yaml_bool(out, b"        required", required);
                yaml_bool(out, b"        found", found);
            }
        }
        if !self.configs.is_empty() {
            out.extend_from_slice(b"    configs:\n");
            for &(filename, kind) in &self.configs {
                out.extend_from_slice(b"      -\n");
                yaml_value(out, b"        filename", filename);
                yaml_value(out, b"        kind", kind);
            }
        }
        out.extend_from_slice(b"    version_request:\n");
        for &(key, value) in &self.version {
            yaml_value(out, &[b"      ", key].concat(), value);
        }
        yaml_bool(out, b"      exact", self.exact);
        self.write_settings(out);
        if !self.candidates.is_empty() {
            out.extend_from_slice(b"    candidates:\n");
            for &(path, mode, reason, message) in &self.candidates {
                out.extend_from_slice(b"      -\n");
                yaml_value(out, b"        path", path);
                yaml_value(out, b"        mode", mode);
                yaml_value(out, b"        reason", reason);
                if !message.is_empty() {
                    yaml_value(out, b"        message", message);
                }
            }
        }
        if let Some((path, mode, version)) = self.found {
            out.extend_from_slice(b"    found:\n");
            yaml_value(out, b"      path", path);
            yaml_value(out, b"      mode", mode);
            yaml_value(out, b"      version", version);
        } else {
            out.extend_from_slice(b"    found: null\n");
        }
        self.write_context(out);
    }

    fn write_settings(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"    settings:\n");
        yaml_value(out, b"      required", self.required);
        for &(key, value) in &self.flags {
            yaml_bool(out, &[b"      ", key].concat(), value);
        }
        for &(key, values) in &self.lists {
            path_list(out, b"      ", key, values);
        }
        if let Some(view) = self.registry_view {
            yaml_value(out, b"      registry_view", view);
        }
        out.extend_from_slice(b"      paths:\n");
        for &(key, value) in &self.paths {
            yaml_bool(out, &[b"        CMAKE_FIND_USE_", key].concat(), value);
        }
        yaml_value(out, b"        CMAKE_FIND_ROOT_PATH_MODE", self.root_mode);
    }

    fn write_context(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"    search_context:\n");
        if !self.package_stack.is_empty() {
            out.extend_from_slice(b"      package_stack:\n");
            for paths in &self.package_stack {
                out.extend_from_slice(b"        -\n");
                path_list(out, b"          ", b"package_paths", paths);
            }
        }
        for (key, value) in &self.context {
            match value {
                SearchValue::String(value) => {
                    yaml_value(out, &[b"      ", key.as_slice()].concat(), value)
                }
                SearchValue::List(values) => path_list(out, b"      ", key, values),
            }
        }
    }
}

fn path_list(out: &mut Vec<u8>, indent: &[u8], key: &[u8], values: &[BString]) {
    if values.is_empty() {
        return;
    }
    out.extend_from_slice(indent);
    out.extend_from_slice(key);
    out.extend_from_slice(b":\n");
    for value in values {
        out.extend_from_slice(indent);
        out.extend_from_slice(b"  - ");
        write_string(out, value);
        out.push(b'\n');
    }
}

impl Cx<'_> {
    /// Add a `find_package-v1` event to the configure log.
    pub fn log_configure_find_package<'a>(
        &mut self,
        fill: impl FnOnce(&mut Self, &mut FindPackageEvent<'a>),
    ) {
        if self.mode() == RunMode::Configure {
            let mut event = FindPackageEvent::default();
            fill(self, &mut event);
            self.interp.start_configure_event(b"find_package-v1");
            self.interp.write_configure_checks();
            event.write(&mut self.interp.configure_log);
        }
    }
}
