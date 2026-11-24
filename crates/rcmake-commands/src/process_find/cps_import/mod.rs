//! Load Common Package Specification files and register their imported targets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, ByteSlice as _, version};
use rcmake_cps::{CpsError, CpsPackage, CpsRequirement, CpsUsage, reader};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, Frame, FrameKind, Location, MessageKind, RenderOptions, render,
};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, ReadRange, Stream, path};
use rcmake_json::{ReadOptions, Value};
use rcmake_syntax::ArgKind;

use super::find_package::{self, Bindings, Request};
use super::text;

pub(super) use targets::{import, target_kind};

mod targets;
#[cfg(test)]
mod tests;

const ACTIVE: &[u8] = b"_RCMAKE_ACTIVE_CPS_PACKAGES";

pub(super) struct PackageDiagnostic {
    pub(super) body: BString,
    files: Vec<BString>,
}

impl PackageDiagnostic {
    pub(super) fn new(body: BString) -> Self {
        Self {
            body,
            files: Vec::new(),
        }
    }

    pub(super) fn emit(&self, cx: &mut Cx<'_>) {
        self.emit_as(cx, MessageKind::Error);
    }

    fn emit_as(&self, cx: &mut Cx<'_>, kind: MessageKind) {
        let root = cx
            .project()
            .dir(rcmake_model::DirId::ROOT)
            .source_dir
            .clone();
        let backtrace = cx.backtrace();
        let commands: Vec<_> = backtrace
            .frames()
            .filter(|frame| frame.kind != FrameKind::ModuleInclude)
            .collect();
        let callers = commands
            .into_iter()
            .rev()
            .fold(Backtrace::default(), |stack, frame| {
                stack.push(frame.clone())
            });
        let backtrace = self.files.iter().rev().fold(callers, |stack, file| {
            stack.push(Frame {
                file: file.as_slice().into(),
                line: 0,
                name: None,
                kind: FrameKind::ModuleInclude,
            })
        });
        let diagnostic = Diagnostic {
            kind,
            effect: Effect::None,
            category: None,
            policy: None,
            location: Location::Backtrace(backtrace),
            body: self.body.clone(),
        };
        let options = RenderOptions {
            relative_base: Some(root),
            ..RenderOptions::default()
        };
        cx.host()
            .write_out(Stream::Stderr, &render(&diagnostic, &options));
    }
}

pub(super) struct Rejection {
    pub(super) reason: BString,
    pub(super) diagnostics: Vec<PackageDiagnostic>,
}

impl From<BString> for Rejection {
    fn from(reason: BString) -> Self {
        Self {
            reason,
            diagnostics: Vec::new(),
        }
    }
}

impl Rejection {
    fn dependency(
        package: &CpsPackage,
        dependency: BString,
        mut diagnostics: Vec<PackageDiagnostic>,
        files: &[&Path],
    ) -> Self {
        for diagnostic in &mut diagnostics {
            diagnostic
                .files
                .extend(files.iter().map(|file| path::from_native(file)));
        }
        Self {
            reason: dependency_error(package, &dependency),
            diagnostics,
        }
    }
}

fn read(cx: &Cx<'_>, file: &Path) -> Result<Vec<u8>, BString> {
    if !cx
        .host()
        .metadata(file, Follow::Yes)
        .is_ok_and(|m| m.kind == FileKind::File)
    {
        return Err(format!("{} is not a regular file.", file.display()).into());
    }
    cx.host()
        .read(
            file,
            ReadRange::All {
                limit: 16 * 1024 * 1024,
            },
        )
        .map_err(|error| error.to_string().into())
}

fn message(error: CpsError) -> BString {
    match error {
        CpsError::Invalid(text) => text,
        error => error.to_string().into(),
    }
}

pub(super) fn read_root(cx: &mut Cx<'_>, file: &Path) -> Result<CpsPackage, BString> {
    let bytes = read(cx, file)?;
    let file = path::from_native(file);
    match reader::read_package(&bytes, &file) {
        Ok(package) => Ok(package),
        Err(error) => recover_optional_attributes(cx, &bytes, &file).ok_or_else(|| message(error)),
    }
}

fn recover_optional_attributes(
    cx: &mut Cx<'_>,
    bytes: &[u8],
    file: &BString,
) -> Option<CpsPackage> {
    let mut value = rcmake_json::parse(bytes, &ReadOptions::default()).ok()?;
    let Value::Object(root) = &mut value else {
        return None;
    };
    let Value::String(package) = root.get(b"name".as_slice())?.clone() else {
        return None;
    };
    let mut warnings = Vec::new();
    for key in ["license", "default_license", "description", "homepage"] {
        if root
            .get(key.as_bytes())
            .is_some_and(|value| !matches!(value, Value::String(_)))
        {
            root.remove(key.as_bytes());
            warnings.push(format!("Package attribute {key:?} is not a string."));
        }
    }
    if let Some(Value::Object(components)) = root.get_mut(b"components".as_slice()) {
        components.retain(|name, value| {
            let name = format!("{package}::{name}");
            let Value::Object(component) = value else {
                warnings.push(format!("Component {name:?} is not an object."));
                return false;
            };
            let configurations = b"configurations".as_slice();
            if component.get(configurations).is_some_and(|value| !matches!(value, Value::Object(_))) {
                component.remove(configurations);
                warnings.push(format!("Component {name:?} attribute \"configurations\" is not an object."));
            }
            for (key, property) in [("location", "LOCATION"), ("link_location", "IMPLIB")] {
                if component.get(key.as_bytes()).is_some_and(|value| !matches!(value, Value::String(_))) {
                    component.remove(key.as_bytes());
                    warnings.push(format!("Failed to set property {property:?} on target {name:?}: attribute {key:?} is not a string."));
                }
            }
            true
        });
    }
    if warnings.is_empty() {
        return None;
    }
    let parsed = reader::read_package(&rcmake_json::write_compact(&value), file).ok()?;
    for body in warnings {
        PackageDiagnostic {
            body: body.into(),
            files: vec![file.clone()],
        }
        .emit_as(cx, MessageKind::Warning);
    }
    Some(parsed)
}

pub(super) fn check_version(package: &CpsPackage, request: &Request) -> Result<(), BString> {
    let Some(wanted) = &request.version else {
        return Ok(());
    };
    let Some(actual) = &package.version else {
        return Err("The package does not specify a version.".into());
    };
    let schema = package
        .version_schema
        .as_ref()
        .map_or(b"simple".as_slice(), |schema| schema.as_slice());
    let accepted = match schema {
        b"custom" => actual == &wanted.min,
        b"simple" | b"" => {
            let order = version::compare(actual, &wanted.min);
            if request.exact || package.compat_version.is_none() {
                order.is_eq()
            } else {
                !order.is_lt()
                    && package
                        .compat_version
                        .as_ref()
                        .is_some_and(|compat| !version::compare(compat, &wanted.min).is_gt())
                    && wanted.max.as_ref().is_none_or(|max| {
                        let order = version::compare(actual, max);
                        order.is_lt() || order.is_eq() && !wanted.exclusive
                    })
            }
        }
        _ => {
            return Err(
                format!("The package uses an unsupported version schema {schema:?}.").into(),
            );
        }
    };
    if accepted {
        Ok(())
    } else {
        let compat = package
            .compat_version
            .as_ref()
            .map_or_else(String::new, |compat| {
                format!(" (compatibility version {compat:?})")
            });
        Err(
            format!("Version {actual:?}{compat} is not compatible with the version requested.")
                .into(),
        )
    }
}

pub(super) fn version_bindings(package: &CpsPackage, prefix: &[u8], vars: &mut Bindings) {
    let Some(actual) = &package.version else {
        return;
    };
    if package
        .version_schema
        .as_ref()
        .is_none_or(|schema| schema.is_empty() || schema == b"simple")
    {
        find_package::version_bindings(prefix, actual, vars);
        let core = actual
            .split(|byte| matches!(byte, b'+' | b'-'))
            .next()
            .unwrap_or_default();
        let count = core.split(|byte| *byte == b'.').count();
        vars.add(text(&[prefix, b"_COUNT"]), count.to_string());
    } else {
        find_package::version_bindings(prefix, b"", vars);
        vars.add(prefix.into(), actual.clone());
    }
}

pub(super) fn prepare(
    cx: &mut Cx<'_>,
    file: &Path,
    package: &mut CpsPackage,
    request: &Request,
) -> Result<Result<(), Rejection>, CmdError> {
    let active = cx.var_normal_only(ACTIVE);
    let files = active
        .iter()
        .flat_map(|value| value.split(|byte| *byte == 0));
    let identity = path::from_native(&cx.host().canonicalize(file).unwrap_or_else(|_| file.into()));
    if files.clone().any(|file| file == identity)
        || files.count() >= cx.limits().max_recursion.min(64) as usize
    {
        return Ok(Err(BString::from(
            "Circular or excessively deep CPS package dependency.",
        )
        .into()));
    }
    let mut active_value = active.clone().unwrap_or_default();
    if !active_value.is_empty() {
        active_value.push(0);
    }
    active_value.extend_from_slice(&identity);
    cx.set_var(ACTIVE, active_value);
    let result = load_companions(cx, file, package, request);
    if let Some(active) = active {
        cx.set_var(ACTIVE, active);
    } else {
        cx.unset_var(ACTIVE);
    }
    result
}

struct Appendix {
    file: PathBuf,
    contents: Vec<u8>,
    package: CpsPackage,
}

fn load_companions(
    cx: &mut Cx<'_>,
    file: &Path,
    package: &mut CpsPackage,
    request: &Request,
) -> Result<Result<(), Rejection>, CmdError> {
    let (Some(dir), Some(stem)) = (
        file.parent(),
        file.file_stem()
            .map(|stem| path::from_native(Path::new(stem))),
    ) else {
        return Ok(Err(BString::from(
            "The CPS file has no package name or directory.",
        )
        .into()));
    };
    let mut files: Vec<_> = cx
        .host()
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let suffix = entry
                .name
                .strip_prefix(stem.as_slice())?
                .strip_suffix(b".cps")?;
            matches!(suffix.first(), Some(b'-' | b'@'))
                .then(|| (BString::from(suffix), entry.name.clone()))
        })
        .collect();
    files.sort();
    let mut appendices = Vec::new();
    let mut supplements: BTreeMap<BString, Vec<PathBuf>> = BTreeMap::new();
    for (suffix, name) in files {
        let companion = dir.join(path::to_native(&name));
        if suffix.contains(&b'@') {
            let owner = suffix
                .split(|byte| *byte == b'@')
                .next()
                .unwrap_or_default();
            supplements.entry(owner.into()).or_default().push(companion);
        } else if let Ok(contents) = read(cx, &companion) {
            let mut appendix = CpsPackage {
                name: package.name.clone(),
                cps_path: package.cps_path.clone(),
                prefix: package.prefix.clone(),
                ..Default::default()
            };
            if reader::merge_appendix(&mut appendix, &contents, &path::from_native(&companion))
                .is_ok()
            {
                appendices.push((
                    suffix,
                    Appendix {
                        file: companion,
                        contents,
                        package: appendix,
                    },
                ));
            }
        }
    }

    if let Err(reason) = load_supplements(
        cx,
        package,
        supplements.get(b"".as_slice()).map_or(&[], Vec::as_slice),
    ) {
        return Ok(Err(reason.into()));
    }
    let mut wanted: BTreeSet<_> = request
        .components
        .iter()
        .map(|(name, _)| name.clone())
        .chain(package.default_components.iter().cloned())
        .chain(internal_requirements(package))
        .collect();
    let mut failed = BTreeMap::new();
    if let Some((dependency, diagnostics)) = missing_dependency(cx, package, request)? {
        return Ok(Err(Rejection::dependency(
            package,
            dependency,
            diagnostics,
            &[file],
        )));
    }
    while let Some(index) = appendices.iter().position(|(_, appendix)| {
        request.components.is_empty()
            || appendix
                .package
                .components
                .keys()
                .any(|name| wanted.contains(name))
    }) {
        let (suffix, mut appendix) = appendices.remove(index);
        if let Err(reason) = load_supplements(
            cx,
            &mut appendix.package,
            supplements.get(&suffix).map_or(&[], Vec::as_slice),
        ) {
            return Ok(Err(reason.into()));
        }
        if let Some((dependency, diagnostics)) = missing_dependency(cx, &appendix.package, request)?
        {
            failed.insert(
                appendix
                    .package
                    .components
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                Rejection::dependency(package, dependency, diagnostics, &[&appendix.file, file]),
            );
            continue;
        }
        if let Err(error) = reader::merge_appendix(
            package,
            &appendix.contents,
            &path::from_native(&appendix.file),
        ) {
            return Ok(Err(message(error).into()));
        }
        wanted.extend(internal_requirements(&appendix.package));
        package.components.extend(appendix.package.components);
    }
    for (component, required) in &request.components {
        if *required
            && !package
                .components
                .get(component)
                .is_some_and(|entry| target_kind(&entry.ty).is_some())
        {
            let rejection = failed
                .into_iter()
                .find_map(|(components, rejection)| {
                    components.contains(component).then_some(rejection)
                })
                .unwrap_or_else(|| {
                    BString::from(format!("Required component was not found: {component}.")).into()
                });
            return Ok(Err(rejection));
        }
    }
    for name in package
        .default_components
        .iter()
        .cloned()
        .chain(internal_requirements(package))
    {
        if !package
            .components
            .get(&name)
            .is_some_and(|component| target_kind(&component.ty).is_some())
        {
            return Ok(Err(BString::from(format!(
                "Required component was not found: {name}."
            ))
            .into()));
        }
    }
    Ok(Ok(()))
}

fn load_supplements(
    cx: &Cx<'_>,
    package: &mut CpsPackage,
    files: &[PathBuf],
) -> Result<(), BString> {
    for file in files {
        let contents = read(cx, file)?;
        let path = path::from_native(file);
        let supplement = reader::read_supplement(&contents, &path).map_err(message)?;
        reader::merge_supplement(package, supplement, &path).map_err(message)?;
    }
    Ok(())
}

fn usages(package: &CpsPackage) -> impl Iterator<Item = &CpsUsage> {
    package.components.values().flat_map(|component| {
        std::iter::once(&component.usage).chain(component.configurations.values())
    })
}

fn references(usage: &CpsUsage) -> impl Iterator<Item = &[u8]> {
    usage
        .requires
        .iter()
        .chain(&usage.link_requires)
        .chain(&usage.compile_requires)
        .chain(&usage.dyld_requires)
        .map(|value| value.as_slice())
}

fn internal_requirements(package: &CpsPackage) -> impl Iterator<Item = BString> {
    usages(package).flat_map(references).filter_map(|entry| {
        let (dependency, component) = entry.split_once_str(b":")?;
        (dependency.is_empty() || dependency == package.name).then(|| component.into())
    })
}

fn missing_dependency(
    cx: &mut Cx<'_>,
    package: &CpsPackage,
    request: &Request,
) -> Result<Option<(BString, Vec<PackageDiagnostic>)>, CmdError> {
    let mut dependencies = package.requires.clone();
    for reference in usages(package).flat_map(references) {
        if let Some((dependency, component)) = reference.split_once_str(b":")
            && !dependency.is_empty()
            && dependency != package.name
            && !component.is_empty()
        {
            let requirement = dependencies.entry(dependency.into()).or_default();
            if !requirement.components.iter().any(|name| name == component) {
                requirement.components.push(component.into());
            }
        }
    }
    for (name, requirement) in dependencies {
        if let Some(diagnostics) = find_dependency(cx, &name, &requirement, request.global)? {
            return Ok(Some((name, diagnostics)));
        }
    }
    Ok(None)
}

fn find_dependency(
    cx: &mut Cx<'_>,
    name: &[u8],
    requirement: &CpsRequirement,
    global: bool,
) -> Result<Option<Vec<PackageDiagnostic>>, CmdError> {
    let found = text(&[name, b"_FOUND"]);
    cx.unset_var(&found);
    let mut args = vec![name.into()];
    args.extend(requirement.version.iter().cloned());
    args.extend(["CONFIG".into(), "QUIET".into(), "OPTIONAL".into()]);
    if global {
        args.push("GLOBAL".into());
    }
    if !requirement.components.is_empty() {
        args.push("COMPONENTS".into());
        args.extend(requirement.components.iter().cloned());
    }
    if !requirement.hints.is_empty() {
        args.push("HINTS".into());
        args.extend(requirement.hints.iter().cloned());
    }
    let line = cx.current_line();
    let args = args
        .into_iter()
        .map(|value| ExpandedArg {
            value,
            line,
            kind: ArgKind::Bracket,
        })
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    find_package::find_package_with_diagnostics(cx, &args, Some(&mut diagnostics))?;
    Ok((!cx.var_is_on(&found)).then_some(diagnostics))
}

fn dependency_error(package: &CpsPackage, dependency: &[u8]) -> BString {
    text(&[
        b"find_package could not find ",
        dependency,
        b", required by ",
        &package.name,
        b".",
    ])
}
