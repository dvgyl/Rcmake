//! Read pkg-config package files and expose their usage requirements.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rcmake_base::{BString, ByteSlice as _, list};
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, ReadRange, path};
use rcmake_model::{
    BT, ImportedVisibility, LinkItem, Target, TargetKind, TargetOrigin, UsageField,
};

use crate::vars_list::separate::Arguments;

use self::options::{Mode, Request, Settings};
use self::pc::{InvalidFile, Package, matches_version, requirements, version_part};
use super::text;

mod options;
mod pc;

const PACKAGE_PROPERTY: &str = "_RCMAKE_PKG_CONFIG_PACKAGE";
const VERSION_PROPERTY: &str = "_RCMAKE_PKG_CONFIG_VERSION";

pub(super) fn run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let req = Request::parse(args)?;
    let settings = Settings::load(cx, &req)?;
    if req.mode == Mode::Extract {
        match load(cx, &settings, req.package, b"").and_then(|package| {
            check_version(req.package, &package, req.version, req.flag(b"EXACT"), b"")?;
            Ok(package)
        }) {
            Ok(package) => extract(cx, &package, &settings),
            Err(message) => report(cx, &req, &message)?,
        }
    } else {
        let prefix = req.value(b"PREFIX").unwrap_or_default();
        let bindings = parse_bindings(&req)?;
        let mut graph = Graph {
            settings: &settings,
            prefix,
            bindings,
            targets: IndexMap::new(),
        };
        let found_var = text(&[b"PKGCONFIG_", req.package, b"_FOUND"]);
        let root = match graph.resolve(cx, req.package, req.version, req.flag(b"EXACT"), b"", 0) {
            Ok(root) => root,
            Err(message) => {
                cx.set_var(&found_var, "FALSE".into());
                report(cx, &req, &message)?;
                return Ok(Flow::Normal);
            }
        };
        if req.mode == Mode::Import {
            let name = req.value(b"NAME").unwrap_or(req.package);
            graph.add_import(cx, name, &root)?;
        }
        for target in graph.targets.into_values() {
            cx.project()
                .add_target(target)
                .map_err(|error| CmdError::status(error.to_string()))?;
        }
        cx.set_var(&found_var, "TRUE".into());
    }
    Ok(Flow::Normal)
}

fn report(cx: &mut Cx<'_>, req: &Request<'_>, message: &[u8]) -> Result<(), CmdError> {
    if req.flag(b"REQUIRED") {
        return Err(CmdError::status(message));
    }
    if !req.flag(b"QUIET") {
        cx.issue(MessageKind::Warning, Effect::None, None, message.into());
    }
    Ok(())
}

fn parse_bindings<'a>(req: &Request<'a>) -> Result<BTreeMap<&'a [u8], &'a [u8]>, CmdError> {
    req.values(b"BIND_PC_REQUIRES")
        .unwrap_or_default()
        .iter()
        .map(|binding| {
            binding.split_once_str(b"=").ok_or_else(|| {
                CmdError::status(format!(
                    "given invalid binding {:?}.",
                    BString::from(*binding)
                ))
            })
        })
        .collect()
}

fn load(
    cx: &Cx<'_>,
    settings: &Settings,
    name: &[u8],
    required_by: &[u8],
) -> Result<Package, BString> {
    let show = BString::from(name);
    let path = find_file(cx, settings, name).ok_or_else(|| {
        let mut message = BString::from(format!("Could not find pkg-config: '{show}'"));
        if !required_by.is_empty() {
            message.extend_from_slice(&text(&[b" required by: '", required_by, b"'"]));
        }
        message
    })?;
    let file = path::from_native(&path);
    let fail = |stage| BString::from(format!("{stage} failed for file: '{file}'"));
    let content = cx
        .host()
        .read(
            &path,
            ReadRange::All {
                limit: cx.limits().max_file_read,
            },
        )
        .map_err(|_| fail("Parsing"))?;
    let mut variables = BTreeMap::from([
        (
            "pcfiledir".into(),
            path.parent()
                .map(|dir| quote_path(&path::from_native(dir)))
                .unwrap_or_default(),
        ),
        ("pc_path".into(), list::join(&settings.search)),
        (
            "pc_sysrootdir".into(),
            if settings.sysroot.is_empty() {
                "/".into()
            } else {
                quote_path(&settings.sysroot)
            },
        ),
    ]);
    if let Some(top_build) = &settings.top_build {
        variables.insert("pc_top_builddir".into(), quote_path(top_build));
    }
    Package::parse(
        &content,
        variables,
        settings.strictness,
        cx.limits().max_value_bytes,
    )
    .map_err(|error| {
        fail(match error {
            InvalidFile::Syntax => "Parsing",
            InvalidFile::Resolution => "Resolution",
        })
    })
}

fn quote_path(path: &[u8]) -> BString {
    if !path.iter().any(u8::is_ascii_whitespace) {
        return path.into();
    }
    text(&[b"\"", path, b"\""])
}

fn find_file(cx: &Cx<'_>, settings: &Settings, name: &[u8]) -> Option<PathBuf> {
    let requested = path::to_native(name);
    let is_file = |path: &Path| {
        cx.host()
            .metadata(path, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    };
    if requested.is_absolute() {
        return is_file(&requested).then_some(requested);
    }
    if name.is_empty() || name.iter().any(|byte| matches!(byte, b'/' | b'\\')) {
        return None;
    }
    let suffixes: &[&[u8]] = if settings.disable_uninstalled || name.ends_with(b"-uninstalled") {
        &[b".pc"]
    } else {
        &[b"-uninstalled.pc", b".pc"]
    };
    for suffix in suffixes {
        let filename = path::to_native(&text(&[name, suffix]));
        for dir in &settings.search {
            let candidate = cx.resolve_path(dir).join(&filename);
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn check_version(
    name: &[u8],
    package: &Package,
    requested: &[u8],
    exact: bool,
    required_by: &[u8],
) -> Result<(), BString> {
    check_version_value(
        name,
        package.field(b"Version"),
        requested,
        exact,
        required_by,
    )
}

fn check_version_value(
    name: &[u8],
    actual: &[u8],
    requested: &[u8],
    exact: bool,
    required_by: &[u8],
) -> Result<(), BString> {
    if matches_version(actual, requested, exact) {
        return Ok(());
    }
    let qualifier = if exact { b"exact ".as_slice() } else { b"" };
    let version = if exact {
        version_part(requested).1
    } else {
        requested
    };
    let mut error = text(&[
        b"Package '",
        name,
        b"' version '",
        actual,
        b"' does not meet ",
        qualifier,
        b"version requirement '",
        version,
        b"'",
    ]);
    if !required_by.is_empty() {
        error.extend_from_slice(&text(&[b" of '", required_by, b"'"]));
    }
    Err(error)
}

fn extract(cx: &mut Cx<'_>, package: &Package, settings: &Settings) {
    let mut set = |key: &[u8], value| cx.set_var(&text(&[b"CMAKE_PKG_CONFIG_", key]), value);
    for (key, field) in [
        (b"NAME".as_slice(), b"Name".as_slice()),
        (b"DESCRIPTION", b"Description"),
        (b"VERSION", b"Version"),
    ] {
        set(key, package.field(field).into());
    }
    for (key, field) in [
        (b"REQUIRES".as_slice(), b"Requires".as_slice()),
        (b"REQUIRES_PRIVATE", b"Requires.private"),
        (b"PROVIDES", b"Provides"),
        (b"CONFLICTS", b"Conflicts"),
    ] {
        set(
            key,
            list::join(requirements(package.field(field)).map(|(name, _)| name)),
        );
    }
    for (suffix, field_suffix) in [(b"".as_slice(), b"".as_slice()), (b"_PRIVATE", b".private")] {
        let cflags = Flags::parse(
            package.field(&text(&[b"Cflags", field_suffix])),
            settings,
            FlagKind::Compile,
        );
        let libs = Flags::parse(
            package.field(&text(&[b"Libs", field_suffix])),
            settings,
            FlagKind::Link,
        );
        for (key, value) in [
            (b"CFLAGS".as_slice(), join_words(&cflags.all)),
            (b"INCLUDES", list::join(cflags.directories)),
            (b"COMPILE_OPTIONS", list::join(cflags.options)),
            (b"LIBS", join_words(&libs.all)),
            (b"LIBDIRS", list::join(libs.directories)),
            (b"LIBNAMES", list::join(libs.libraries)),
            (b"LINK_OPTIONS", list::join(libs.options)),
        ] {
            set(&text(&[key, suffix]), value);
        }
    }
}

fn join_words(words: &[BString]) -> BString {
    let mut result = BString::default();
    for (index, word) in words.iter().enumerate() {
        if index != 0 {
            result.push(b' ');
        }
        result.extend_from_slice(word);
    }
    result
}

#[derive(Clone, Copy)]
enum FlagKind {
    Compile,
    Link,
}

#[derive(Default)]
struct Flags {
    all: Vec<BString>,
    directories: Vec<BString>,
    libraries: Vec<BString>,
    options: Vec<BString>,
}

impl Flags {
    fn parse(value: &[u8], settings: &Settings, kind: FlagKind) -> Self {
        let mut flags = Self::default();
        let (prefix, filtered) = match kind {
            FlagKind::Compile => (b"-I", &settings.filter_includes),
            FlagKind::Link => (b"-L", &settings.filter_libs),
        };
        for mut flag in Arguments::new(value, false) {
            if let Some(dir) = flag.strip_prefix(prefix) {
                if filtered.iter().any(|value| value == dir) {
                    continue;
                }
                if !settings.sysroot.is_empty() {
                    flag = text(&[
                        prefix,
                        settings
                            .sysroot
                            .strip_suffix(b"/")
                            .unwrap_or(&settings.sysroot),
                        dir,
                    ]);
                }
                flags.directories.push(flag.clone());
            } else if matches!(kind, FlagKind::Link) && flag.starts_with(b"-l") {
                flags.libraries.push(flag.clone());
            } else {
                flags.options.push(flag.clone());
            }
            flags.all.push(flag);
        }
        flags
    }
}

struct Graph<'a, 'b> {
    settings: &'a Settings,
    prefix: &'b [u8],
    bindings: BTreeMap<&'b [u8], &'b [u8]>,
    targets: IndexMap<BString, Target>,
}

impl Graph<'_, '_> {
    fn resolve(
        &mut self,
        cx: &mut Cx<'_>,
        package: &[u8],
        constraint: &[u8],
        exact: bool,
        required_by: &[u8],
        depth: u32,
    ) -> Result<BString, BString> {
        if depth >= cx.limits().max_recursion.min(128) {
            return Err("pkg-config dependency recursion limit exceeded.".into());
        }
        let target_name = if self.prefix.is_empty() {
            text(&[b"@foreign_pkgcfg::", package])
        } else {
            text(&[b"@foreign_pkgcfg::", self.prefix, b"_", package])
        };
        let dir = cx.current_dir_id();
        let project = cx.project();
        let existing = self.targets.get(&target_name).or_else(|| {
            project
                .find_target(dir, &target_name)
                .and_then(|id| project.target(id))
        });
        if let Some(existing) = existing {
            if !required_by.is_empty() {
                let version = existing
                    .props
                    .get(VERSION_PROPERTY.as_bytes())
                    .map_or(b"".as_slice(), |value| value.as_slice());
                check_version_value(package, version, constraint, exact, required_by)?;
            }
            return Ok(target_name);
        }
        let data = load(cx, self.settings, package, required_by)?;
        check_version(package, &data, constraint, exact, required_by)?;

        let mut target = new_target(cx, target_name.clone(), ImportedVisibility::Global);
        target.props.set(PACKAGE_PROPERTY.into(), package.into());
        target
            .props
            .set(VERSION_PROPERTY.into(), data.field(b"Version").into());
        add_flags(&mut target, &data, self.settings);
        self.targets.insert(target_name.clone(), target);
        let mut links = Vec::new();
        for (dependency, constraint) in requirements(data.field(b"Requires")) {
            let link = if let Some(&binding) = self.bindings.get(dependency) {
                binding.into()
            } else {
                self.resolve(cx, dependency, &constraint, false, package, depth + 1)?
            };
            links.push(link);
        }
        if let Some(target) = self.targets.get_mut(&target_name) {
            add_links(target, links);
        }
        Ok(target_name)
    }

    fn add_import(&mut self, cx: &mut Cx<'_>, name: &[u8], root: &[u8]) -> Result<(), CmdError> {
        let name = text(&[b"PkgConfig::", name]);
        let dir = cx.current_dir_id();
        let project = cx.project();
        if project.find_target(dir, &name).is_some() {
            return Ok(());
        }
        let package = self
            .targets
            .get(root)
            .or_else(|| {
                project
                    .find_target(dir, root)
                    .and_then(|id| project.target(id))
            })
            .and_then(|target| target.props.get(PACKAGE_PROPERTY.as_bytes()))
            .cloned()
            .ok_or_else(|| CmdError::status("could not resolve the pkg-config target."))?;
        let mut target = new_target(cx, name.clone(), ImportedVisibility::Local);
        target.props.set(PACKAGE_PROPERTY.into(), package);
        add_links(&mut target, [root.into()]);
        self.targets.insert(name, target);
        Ok(())
    }
}

fn new_target(cx: &mut Cx<'_>, name: BString, visibility: ImportedVisibility) -> Target {
    let mut target = Target::new(
        name,
        TargetKind::InterfaceLibrary,
        TargetOrigin::Imported(visibility),
        cx.current_dir_id(),
        cx.backtrace(),
        *cx.policies_mut(),
    );
    target.props.set("SYSTEM".into(), "ON".into());
    target
}

fn add_flags(target: &mut Target, package: &Package, settings: &Settings) {
    let compile = Flags::parse(package.field(b"Cflags"), settings, FlagKind::Compile);
    let link = Flags::parse(package.field(b"Libs"), settings, FlagKind::Link);
    for (field, flags, strip) in [
        (
            UsageField::InterfaceIncludeDirectories,
            compile.directories,
            b"-I".as_slice(),
        ),
        (UsageField::InterfaceCompileOptions, compile.options, b""),
        (
            UsageField::InterfaceLinkDirectories,
            link.directories,
            b"-L",
        ),
        (UsageField::InterfaceLinkOptions, link.options, b""),
    ] {
        for flag in flags {
            let value = flag.strip_prefix(strip).unwrap_or(&flag);
            target
                .usage
                .push(field, BT::new(value.into(), target.backtrace.clone()));
        }
    }
    add_links(
        target,
        link.libraries
            .into_iter()
            .map(|lib| lib.strip_prefix(b"-l").unwrap_or(&lib).into()),
    );
}

fn add_links(target: &mut Target, links: impl IntoIterator<Item = BString>) {
    for value in links {
        target
            .usage
            .interface_link_libraries
            .get_or_insert_with(Vec::new)
            .push(LinkItem {
                text: BT::new(value, target.backtrace.clone()),
                lookup_dir: target.dir,
            });
    }
}
