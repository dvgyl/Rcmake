//! Load Common Package Specification files and register their imported targets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, ByteSlice as _, list, version};
use rcmake_cps::{CpsComponentType, CpsError, CpsPackage, CpsRequirement, CpsUsage, reader};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, Frame, FrameKind, Location, MessageKind, RenderOptions, render,
};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, ReadRange, Stream, path};
use rcmake_json::{ReadOptions, Value};
use rcmake_model::{
    BT, ImportedVisibility, LinkItem, Target, TargetKind, TargetOrigin, UsageField,
};
use rcmake_syntax::ArgKind;

use super::find_package::{self, Bindings, Request};
use super::text;

const ACTIVE: &[u8] = b"_RCMAKE_ACTIVE_CPS_PACKAGES";
const SOURCE: &[u8] = b"_RCMAKE_CPS_SOURCE";

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

pub(super) fn target_kind(ty: &CpsComponentType) -> Option<TargetKind> {
    Some(match ty {
        CpsComponentType::Archive => TargetKind::StaticLibrary,
        CpsComponentType::Dylib => TargetKind::SharedLibrary,
        CpsComponentType::Module => TargetKind::ModuleLibrary,
        CpsComponentType::Executable => TargetKind::Executable,
        CpsComponentType::Interface | CpsComponentType::Symbolic => TargetKind::InterfaceLibrary,
        CpsComponentType::Jar | CpsComponentType::Unknown(_) => return None,
    })
}

pub(super) fn import(
    cx: &mut Cx<'_>,
    package: &CpsPackage,
    source: &BString,
) -> Result<(), CmdError> {
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    let policies = *cx.policies_mut();
    let mut targets = Vec::new();
    let new_target = |name, kind| {
        let mut target = Target::new(
            name,
            kind,
            TargetOrigin::Imported(ImportedVisibility::Local),
            dir,
            backtrace.clone(),
            policies,
        );
        target.props.set("SYSTEM".into(), "ON".into());
        target.props.set(SOURCE.into(), source.clone());
        target
    };
    for (component_name, component) in &package.components {
        let Some(kind) = target_kind(&component.ty) else {
            continue;
        };
        let mut target = new_target(text(&[&package.name, b"::", component_name]), kind);
        if let Some(license) = component
            .license
            .as_ref()
            .or(package.default_license.as_ref())
            .or(package.license.as_ref())
            .filter(|license| !license.is_empty())
        {
            target.props.set("SPDX_LICENSE".into(), license.clone());
        }
        if component.ty == CpsComponentType::Symbolic {
            target.props.set("SYMBOLIC".into(), "TRUE".into());
        }
        apply_usage(&mut target, &package.name, &component.usage, b"");
        let mut seen = BTreeSet::new();
        let configurations: Vec<_> = package
            .configurations
            .iter()
            .filter_map(|default| {
                component
                    .configurations
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(default))
            })
            .chain(&component.configurations)
            .filter(|(name, _)| seen.insert(name.to_ascii_uppercase()))
            .collect();
        if !configurations.is_empty() {
            target.props.set(
                "IMPORTED_CONFIGURATIONS".into(),
                list::join(
                    configurations
                        .iter()
                        .map(|(name, _)| name.to_ascii_uppercase()),
                ),
            );
            for (name, usage) in configurations {
                apply_usage(&mut target, &package.name, usage, name);
            }
        }
        targets.push(target);
    }
    if !package.default_components.is_empty() {
        let mut target = new_target(package.name.clone(), TargetKind::InterfaceLibrary);
        if let Some(license) = &package.license {
            target.props.set("SPDX_LICENSE".into(), license.clone());
        }
        for component in &package.default_components {
            add_link(&mut target, text(&[&package.name, b"::", component]));
        }
        targets.push(target);
    }

    let project = cx.project();
    let mut new_targets = Vec::new();
    for target in targets {
        if let Some(existing) = project
            .find_target(dir, &target.name)
            .and_then(|id| project.target(id))
        {
            if existing.props.get(SOURCE) != Some(source) {
                return Err(CmdError::Status(format!(
                    "cannot create target {:?} because a target with the same name already exists.",
                    target.name
                ).into()));
            }
        } else {
            new_targets.push(target);
        }
    }
    for target in new_targets {
        project
            .add_target(target)
            .map_err(|error| CmdError::Status(error.to_string().into()))?;
    }
    Ok(())
}

fn apply_usage(target: &mut Target, package: &[u8], usage: &CpsUsage, config: &[u8]) {
    let suffix = if config.is_empty() {
        BString::default()
    } else {
        text(&[b"_", &config.to_ascii_uppercase()])
    };
    for (prop, value) in [
        (b"IMPORTED_LOCATION".as_slice(), &usage.location),
        (b"IMPORTED_IMPLIB", &usage.link_location),
        (b"IMPORTED_SONAME", &usage.link_name),
    ] {
        if let Some(value) = value {
            target.props.set(text(&[prop, &suffix]), value.clone());
        }
    }
    if !usage.link_languages.is_empty() {
        target.props.set(
            text(&[b"IMPORTED_LINK_INTERFACE_LANGUAGES", &suffix]),
            list::join(
                usage
                    .link_languages
                    .iter()
                    .filter_map(|name| language(name)),
            ),
        );
    }
    if !usage.dyld_requires.is_empty() {
        target.props.set(
            text(&[b"IMPORTED_LINK_DEPENDENT_LIBRARIES", &suffix]),
            list::join(
                usage
                    .dyld_requires
                    .iter()
                    .map(|name| reference(package, name)),
            ),
        );
    }

    for (field, entries) in [
        (UsageField::InterfaceLinkOptions, &usage.link_flags),
        (
            UsageField::InterfaceLinkDirectories,
            &usage.link_directories,
        ),
    ] {
        for entry in entries {
            add_usage(target, field, entry.clone(), config, b"*");
        }
    }
    for (field, by_language) in [
        (UsageField::InterfaceIncludeDirectories, &usage.includes),
        (
            UsageField::InterfaceSystemIncludeDirectories,
            &usage.includes,
        ),
        (UsageField::InterfaceCompileOptions, &usage.compile_flags),
    ] {
        for (lang, entries) in by_language {
            for entry in entries {
                add_usage(target, field, entry.clone(), config, lang);
            }
        }
    }
    for (lang, definitions) in &usage.definitions {
        for (key, value) in definitions {
            let definition = value
                .as_ref()
                .map_or_else(|| key.clone(), |value| text(&[key, b"=", value]));
            add_usage(
                target,
                UsageField::InterfaceCompileDefinitions,
                definition,
                config,
                lang,
            );
        }
    }
    for entry in &usage.link_libraries {
        add_link(target, condition(entry.clone(), b"CONFIG", config));
    }
    for (entries, scope) in [
        (&usage.requires, b"".as_slice()),
        (&usage.link_requires, b"LINK_ONLY"),
        (&usage.compile_requires, b"COMPILE_ONLY"),
    ] {
        for entry in entries {
            let name = reference(package, entry);
            let link = if scope.is_empty() {
                name
            } else {
                text(&[b"$<", scope, b":", &name, b">"])
            };
            add_link(target, condition(link, b"CONFIG", config));
        }
    }
}

fn reference(package: &[u8], entry: &[u8]) -> BString {
    if let Some((dependency, component)) = entry.split_once_str(b":") {
        let dependency = if dependency.is_empty() {
            package
        } else {
            dependency
        };
        text(&[dependency, b"::", component])
    } else {
        entry.into()
    }
}

fn language(name: &[u8]) -> Option<&'static [u8]> {
    Some(match name {
        b"c" => b"C",
        b"cpp" => b"CXX",
        b"fortran" => b"Fortran",
        b"objc" => b"OBJC",
        b"objcxx" => b"OBJCXX",
        b"cuda" => b"CUDA",
        b"hip" => b"HIP",
        b"swift" => b"Swift",
        b"asm" => b"ASM",
        _ => return None,
    })
}

fn condition(entry: BString, key: &[u8], value: &[u8]) -> BString {
    if value.is_empty() {
        entry
    } else {
        text(&[b"$<$<", key, b":", value, b">:", &entry, b">"])
    }
}

fn add_usage(target: &mut Target, field: UsageField, entry: BString, config: &[u8], lang: &[u8]) {
    let value = if lang == b"*" {
        entry
    } else if let Some(lang) = language(lang) {
        condition(entry, b"COMPILE_LANGUAGE", lang)
    } else {
        return;
    };
    let value = condition(value, b"CONFIG", config);
    target
        .usage
        .push(field, BT::new(value, target.backtrace.clone()));
}

fn add_link(target: &mut Target, value: BString) {
    target
        .usage
        .interface_link_libraries
        .get_or_insert_with(Vec::new)
        .push(LinkItem {
            text: BT::new(value, target.backtrace.clone()),
            lookup_dir: target.dir,
        });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{
        CommandSpec, Host, Input, NativeHost, Output, ProcessResult, PureHost, RunRequest, Stream,
        WriteMode,
    };
    use rcmake_model::DirId;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    const ROOT: &str = "/work/prefix/lib/cps/Foo";
    const FOO: &str = r#"
{
  "cps_version": "0.15.0", "name": "Foo", "version": "2.4.3+suffix",
  "compat_version": "2.0", "cps_path": "@prefix@/lib/cps/Foo",
  "default_components": ["bar"],
  "components": {"bar": {
    "type": "archive", "includes": ["@prefix@/include"],
    "compile_flags": {"*":["-Wall"], "cpp":["-Wextra"]},
    "definitions": {"*":{"HAVE_FOO":null}},
    "link_flags": ["-flag"], "requires": ["Dep:api"]
  }}
}"#;

    fn interpreter(host: Arc<dyn Host>, cwd: &Path) -> Interp {
        Interp::new(
            InterpConfig {
                host,
                mode: RunMode::Configure,
                cmake_root: "/cmake".into(),
                cmake_command: "/cmake/bin/cmake".into(),
                cwd: cwd.into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        )
    }

    fn fixture(
        code: &str,
        expected: i32,
    ) -> Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::default());
        for (name, contents) in [
            ("Foo.cps", FOO),
            (
                "Foo@noconfig.cps",
                r#"{
              "name":"Foo", "configuration":"noconfig", "components":{"bar":{
                "location":"@prefix@/lib/libbar.a",
                "link_location":"@prefix@/lib/bar.imp",
                "link_languages":["cpp"]
              }}
            }"#,
            ),
            (
                "Foo-extra.cps",
                r#"{
              "name":"Foo", "components":{"extra":{"type":"interface", "requires":[":bar"]}}
            }"#,
            ),
            (
                "Foo-skipped.cps",
                r#"{
              "name":"Foo", "requires":{"Absent":null}, "components":{"skipped":{"type":"interface"}}
            }"#,
            ),
        ] {
            host.seed_file(&Path::new(ROOT).join(name), contents.as_bytes())?;
        }
        host.seed_file(
            Path::new("/work/prefix/share/cps/Dep/dep.cps"),
            br#"{"name":"Dep","cps_version":"0.15.0","prefix":"/work/prefix","components":{"api":{"type":"interface"}}}"#,
        )?;
        host.seed_file(
            Path::new("/work/prefix/cps/custom/custom.cps"),
            br#"{"name":"Custom","cps_version":"0.15.0","prefix":"/work/prefix","version_schema":"custom","version":"42","components":{"api":{"type":"interface"}}}"#,
        )?;
        let src = Path::new("/work/src");
        let code = format!(
            "cmake_minimum_required(VERSION 4.5)\nset(CMAKE_PREFIX_PATH /work/prefix)\n{code}"
        );
        host.seed_file(&src.join("CMakeLists.txt"), code.as_bytes())?;
        let mut interp = interpreter(host.clone(), Path::new("/work"));
        let outcome = interp.run_configure(src, Path::new("/work/build"), &[]);
        assert_eq!(
            outcome.exit_code,
            expected,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        Ok((host, interp))
    }

    #[test]
    fn imports_configurations_usage_requirements_and_default_target() -> TestResult {
        fixture(
            r#"
find_package(Foo CONFIG REQUIRED GLOBAL)
if(NOT Foo_FOUND OR NOT Dep_FOUND OR NOT Foo_bar_FOUND OR NOT Foo_extra_FOUND
   OR NOT TARGET Foo OR NOT TARGET Foo::extra OR NOT TARGET Dep::api OR TARGET Foo::skipped)
  message(FATAL_ERROR "Expected imported CPS targets")
endif()
if(NOT Foo_VERSION STREQUAL "2.4.3+suffix" OR NOT Foo_VERSION_MAJOR EQUAL 2
   OR NOT Foo_VERSION_MINOR EQUAL 4 OR NOT Foo_VERSION_PATCH EQUAL 3
   OR NOT Foo_VERSION_TWEAK EQUAL 0 OR NOT Foo_DIR STREQUAL "/work/prefix/lib/cps/Foo"
   OR NOT Foo_CONFIG STREQUAL "/work/prefix/lib/cps/Foo/Foo.cps")
  message(FATAL_ERROR "Unexpected CPS package variables")
endif()
get_target_property(location Foo::bar IMPORTED_LOCATION_NOCONFIG)
get_target_property(implib Foo::bar IMPORTED_IMPLIB_NOCONFIG)
get_target_property(langs Foo::bar IMPORTED_LINK_INTERFACE_LANGUAGES_NOCONFIG)
get_target_property(configs Foo::bar IMPORTED_CONFIGURATIONS)
get_target_property(includes Foo::bar INTERFACE_INCLUDE_DIRECTORIES)
get_target_property(system_includes Foo::bar INTERFACE_SYSTEM_INCLUDE_DIRECTORIES)
get_target_property(defs Foo::bar INTERFACE_COMPILE_DEFINITIONS)
get_target_property(options Foo::bar INTERFACE_COMPILE_OPTIONS)
get_target_property(link_options Foo::bar INTERFACE_LINK_OPTIONS)
get_target_property(default_link Foo INTERFACE_LINK_LIBRARIES)
get_target_property(internal_link Foo::extra INTERFACE_LINK_LIBRARIES)
get_target_property(external_link Foo::bar INTERFACE_LINK_LIBRARIES)
get_target_property(global Foo::bar IMPORTED_GLOBAL)
if(NOT global OR NOT includes STREQUAL "/work/prefix/include"
   OR NOT system_includes STREQUAL "/work/prefix/include"
   OR NOT location STREQUAL "/work/prefix/lib/libbar.a"
   OR NOT implib STREQUAL "/work/prefix/lib/bar.imp" OR NOT langs STREQUAL "CXX"
   OR NOT configs STREQUAL "NOCONFIG" OR NOT defs STREQUAL "HAVE_FOO"
   OR NOT options STREQUAL "-Wall;$<$<COMPILE_LANGUAGE:CXX>:-Wextra>"
   OR NOT link_options STREQUAL "-flag" OR NOT default_link STREQUAL "Foo::bar"
   OR NOT external_link STREQUAL "Dep::api" OR NOT internal_link STREQUAL "Foo::bar")
  message(FATAL_ERROR "Unexpected imported properties")
endif()
"#,
            0,
        )?;
        Ok(())
    }

    #[test]
    fn checks_versions_and_selects_appendices_on_each_call() -> TestResult {
        fixture(
            r#"
find_package(Custom 42.0 QUIET CONFIG)
if(Custom_FOUND)
  message(FATAL_ERROR "Custom versions require an exact string match")
endif()
find_package(Custom 42 CONFIG REQUIRED)
if(NOT Custom_VERSION STREQUAL "42" OR NOT Custom_VERSION_COUNT EQUAL 0)
  message(FATAL_ERROR "Unexpected custom version")
endif()
find_package(Foo 2.1...<2.4.3 QUIET CONFIG)
if(Foo_FOUND OR TARGET Foo::bar)
  message(FATAL_ERROR "CPS version exceeds the exclusive limit")
endif()
find_package(Foo 2.4.3 EXACT CONFIG REQUIRED COMPONENTS bar OPTIONAL_COMPONENTS skipped)
if(NOT Foo_FOUND OR Foo_skipped_FOUND OR TARGET Foo::extra)
  message(FATAL_ERROR "Unexpected CPS appendix")
endif()
find_package(Foo 2.1...2.4.3 CONFIG REQUIRED COMPONENTS extra)
if(NOT Foo_extra_FOUND OR NOT TARGET Foo::extra)
  message(FATAL_ERROR "Expected requested CPS appendix")
endif()
find_package(Foo QUIET CONFIG CONFIGS FooConfig.cmake)
if(Foo_FOUND)
  message(FATAL_ERROR "CONFIGS must suppress CPS discovery")
endif()
"#,
            0,
        )?;
        let (host, mut interp) = fixture("find_package(Foo CONFIG REQUIRED COMPONENTS absent)", 1)?;
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(stderr.contains_str("Required component was not found: absent."));
        assert!(
            interp
                .cx()
                .project()
                .find_target(DirId::ROOT, b"Foo::bar")
                .is_none()
        );
        Ok(())
    }

    struct Scratch {
        host: Arc<NativeHost>,
        path: PathBuf,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }

    #[test]
    fn imports_package_info_installed_by_cmake_4_5_1() -> TestResult {
        let host = Arc::new(NativeHost::new()?);
        let Some(cmake) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
            return Ok(());
        };
        let now = host.now();
        let path = std::env::temp_dir().join(format!(
            "rcmake-cps-{}-{}-{}",
            host.system().pid,
            now.secs,
            now.nanos
        ));
        host.create_dir_all(&path)?;
        let scratch = Scratch {
            host: host.clone(),
            path,
        };
        let root = host.canonicalize(&scratch.path)?;
        let run = |args| {
            let output = host.run(
                &RunRequest {
                    pipeline: vec![CommandSpec {
                        program: cmake.clone(),
                        args,
                    }],
                    cwd: root.clone(),
                    env: host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: Some(Duration::from_secs(30)),
                    tee: false,
                },
                &mut |_, _| {},
            )?;
            assert_eq!(
                output.results,
                [ProcessResult::Exited(0)],
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok::<_, rcmake_host::HostError>(output.stdout)
        };
        assert!(run(vec!["--version".into()])?.starts_with(b"cmake version 4.5.1\n"));
        let producer = root.join("producer");
        host.create_dir_all(&producer)?;
        host.write(
            &producer.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Producer NONE)
add_library(api INTERFACE)
target_include_directories(api INTERFACE "$<INSTALL_INTERFACE:include>")
install(TARGETS api EXPORT DemoTargets)
install(PACKAGE_INFO Demo EXPORT DemoTargets DESTINATION lib/cps/Demo
        VERSION 2.3 COMPAT_VERSION 2.0 DEFAULT_TARGETS api LOWER_CASE_FILE)
"#,
            WriteMode::CreateNew,
        )?;
        run(vec![
            "-S".into(),
            "producer".into(),
            "-B".into(),
            "build".into(),
        ])?;
        let prefix = root.join("prefix");
        run(vec![
            "--install".into(),
            "build".into(),
            "--prefix".into(),
            path::from_native(&prefix),
        ])?;
        let consumer = root.join("consumer");
        host.create_dir_all(&consumer)?;
        host.write(
            &consumer.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
find_package(Demo 2.1 CONFIG REQUIRED PATHS "${CMAKE_CURRENT_SOURCE_DIR}/../prefix" NO_DEFAULT_PATH)
if(NOT TARGET Demo::api OR NOT TARGET Demo OR NOT Demo_VERSION STREQUAL "2.3")
  message(FATAL_ERROR "Expected installed CPS targets")
endif()
"#,
            WriteMode::CreateNew,
        )?;
        let mut interp = interpreter(host, &root);
        assert_eq!(
            interp
                .run_configure(&consumer, &root.join("consumer-build"), &[])
                .exit_code,
            0
        );
        let mut cx = interp.cx();
        let project = cx.project();
        let api = project
            .find_target(DirId::ROOT, b"Demo::api")
            .ok_or("missing API target")?;
        let target = project.target(api).ok_or("missing imported target")?;
        assert_eq!(
            target.usage.joined(UsageField::InterfaceIncludeDirectories),
            Some(path::from_native(&prefix.join("include")))
        );
        Ok(())
    }
}
