//! Configure-time requests for package information and SBOM files.

use std::collections::BTreeMap;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::Cx;
use rcmake_host::path::{from_native, to_native};
use rcmake_model::{
    BuildExport, ExportKind, InstallKind, InstallOptions, PackageInfoSpec, ProjectMetadata,
    SbomSpec,
};

use super::{Result, destination, fail, type_destination};

const PROJECT_OPTIONS: &[&[u8]] = &[
    b"PROJECT",
    b"VERSION",
    b"DEFAULT_LICENSE",
    b"DESCRIPTION",
    b"HOMEPAGE_URL",
];
const PACKAGE_OPTIONS: &[&[u8]] = &[
    b"EXPORT",
    b"APPENDIX",
    b"COMPAT_VERSION",
    b"VERSION_SCHEMA",
    b"LICENSE",
    b"CXX_MODULES_DIRECTORY",
];
const SBOM_OPTIONS: &[&[u8]] = &[b"DATA_LICENSE", b"PACKAGE_URL", b"FORMAT"];
const SPDX_JSON_FORMAT: &[u8] = b"spdx-3.0.1+json";

#[derive(Clone, Copy)]
enum Arity {
    One,
    List,
    Flag,
}

fn arity(signature: &[u8], key: &[u8], install: bool) -> Option<Arity> {
    if PROJECT_OPTIONS.contains(&key) {
        return Some(Arity::One);
    }
    match key {
        b"NO_PROJECT_METADATA" => Some(Arity::Flag),
        b"DESTINATION" | b"COMPONENT" if install => Some(Arity::One),
        b"CONFIGURATIONS" | b"PERMISSIONS" if install => Some(Arity::List),
        b"EXCLUDE_FROM_ALL" if install => Some(Arity::Flag),
        _ if signature == b"PACKAGE_INFO" => match key {
            b"LOWER_CASE_FILE" => Some(Arity::Flag),
            b"DEFAULT_TARGETS" | b"DEFAULT_CONFIGURATIONS" => Some(Arity::List),
            _ if PACKAGE_OPTIONS.contains(&key) => Some(Arity::One),
            _ => None,
        },
        b"EXPORTS" => Some(Arity::List),
        _ if SBOM_OPTIONS.contains(&key) => Some(Arity::One),
        _ => None,
    }
}

#[derive(Default)]
struct Arguments<'a>(BTreeMap<&'a [u8], Vec<&'a [u8]>>);

impl<'a> Arguments<'a> {
    fn parse(signature: &'a [u8], input: &[&'a [u8]], install: bool) -> Result<Self> {
        let mut result = Self::default();
        let mut errors = BTreeMap::new();
        let mut unknown = Vec::new();
        let mut tokens = input.iter().copied().peekable();
        let is_option = |key: &[u8]| arity(signature, key, install).is_some();
        let mut key = signature;
        let mut kind = Some(Arity::One);
        loop {
            let values = match kind {
                Some(Arity::One) => tokens
                    .next_if(|next| !is_option(next))
                    .into_iter()
                    .collect(),
                Some(Arity::List) => {
                    std::iter::from_fn(|| tokens.next_if(|next| !is_option(next))).collect()
                }
                Some(Arity::Flag) => Vec::new(),
                None => Vec::new(),
            };
            match kind {
                Some(Arity::One | Arity::List)
                    if values.is_empty()
                        && !(install && matches!(key, b"CONFIGURATIONS" | b"PERMISSIONS")) =>
                {
                    errors.insert(key, "missing required value");
                }
                Some(Arity::One | Arity::List)
                    if values.contains(&b"".as_slice())
                        && !(install && matches!(key, b"DESTINATION" | b"COMPONENT")) =>
                {
                    errors.insert(key, "empty string not allowed");
                }
                Some(Arity::List) => result.0.entry(key).or_default().extend(values),
                Some(_) => {
                    result.0.insert(key, values);
                }
                None => unknown.push(key),
            }
            let Some(next) = tokens.next() else {
                break;
            };
            key = next;
            kind = arity(signature, key, install);
        }
        if !errors.is_empty() {
            let label = if errors.len() == 1 {
                "argument"
            } else {
                "arguments"
            };
            let mut message = format!(
                "{} given invalid {label}:\n",
                String::from_utf8_lossy(signature)
            );
            for (key, reason) in errors {
                use std::fmt::Write as _;
                let _ = write!(message, "\n  {}: {reason}", String::from_utf8_lossy(key));
            }
            return Err(fail(message));
        }
        if let Some(key) = unknown.first() {
            return Err(fail(
                [signature, b" given unknown argument: \"", key, b"\"."].concat(),
            ));
        }
        valid_name(signature, result.one(signature).unwrap_or_default())?;
        Ok(result)
    }

    fn has(&self, key: &[u8]) -> bool {
        self.0.contains_key(key)
    }

    fn one(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.0.get(key)?.first().copied()
    }

    fn field(&self, key: &[u8]) -> Option<BString> {
        self.one(key).map(BString::from)
    }

    fn list(&self, key: &[u8]) -> Vec<BString> {
        self.0
            .get(key)
            .into_iter()
            .flatten()
            .copied()
            .map(BString::from)
            .collect()
    }

    fn project(&self) -> Result<ProjectMetadata> {
        match (self.one(b"PROJECT"), self.has(b"NO_PROJECT_METADATA")) {
            (Some(_), true) => Err(conflict(b"PROJECT", b"NO_PROJECT_METADATA")),
            (Some(name), false) => Ok(ProjectMetadata::Named(name.into())),
            (None, true) => Ok(ProjectMetadata::None),
            (None, false) => Ok(ProjectMetadata::Current),
        }
    }

    fn install_options(&self, cx: &Cx<'_>, default_path: BString) -> Result<InstallOptions> {
        let path = self
            .one(b"DESTINATION")
            .filter(|value| !value.is_empty())
            .unwrap_or(&default_path);
        let permissions = self.list(b"PERMISSIONS");
        if permissions
            .iter()
            .any(|value| crate::file_path::permission(value).is_none())
        {
            return Err(fail("unknown error."));
        }
        Ok(InstallOptions {
            destination: Some(destination(cx, path)),
            component: self.field(b"COMPONENT"),
            permissions,
            configurations: self.list(b"CONFIGURATIONS"),
            exclude_from_all: self.has(b"EXCLUDE_FROM_ALL"),
            ..InstallOptions::default()
        })
    }
}

fn conflict(first: &[u8], second: &[u8]) -> rcmake_eval::CmdError {
    fail([first, b" and ", second, b" are mutually exclusive."].concat())
}

fn valid_name(signature: &[u8], name: &[u8]) -> Result<()> {
    if name
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
    {
        Ok(())
    } else {
        Err(fail(
            [signature, b" given invalid package name \"", name, b"\"."].concat(),
        ))
    }
}

fn known_project(cx: &mut Cx<'_>, name: &[u8]) -> bool {
    cx.cache()
        .get(&[name, b"_SOURCE_DIR"].concat())
        .is_some_and(|entry| entry.ty == rcmake_model::CacheType::Static)
}

fn metadata_project(
    cx: &mut Cx<'_>,
    project: &ProjectMetadata,
    name: &[u8],
) -> Result<Option<BString>> {
    match project {
        ProjectMetadata::Named(project) if !known_project(cx, project) => Err(fail(
            [
                b"PROJECT given unknown project name \"",
                project.as_slice(),
                b"\".",
            ]
            .concat(),
        )),
        ProjectMetadata::Named(project) => Ok(Some(project.clone())),
        ProjectMetadata::Current
            if cx.var_or_empty(b"PROJECT_NAME") == name && known_project(cx, name) =>
        {
            Ok(Some(name.into()))
        }
        _ => Ok(None),
    }
}

fn project_field(cx: &Cx<'_>, project: &[u8], suffix: &[u8]) -> Option<BString> {
    let value = cx.var_or_empty(&[project, b"_", suffix].concat());
    (!value.is_empty()).then_some(value)
}

fn project_license(cx: &Cx<'_>, project: &[u8]) -> Option<BString> {
    project_field(cx, project, b"SPDX_LICENSE").or_else(|| project_field(cx, project, b"LICENSE"))
}

fn package_info(cx: &mut Cx<'_>, args: &Arguments<'_>) -> Result<PackageInfoSpec> {
    let mut spec = PackageInfoSpec {
        package: args.field(b"PACKAGE_INFO").unwrap_or_default(),
        export: args
            .field(b"EXPORT")
            .ok_or_else(|| fail("PACKAGE_INFO missing EXPORT."))?,
        project: args.project()?,
        appendix: args.field(b"APPENDIX"),
        lower_case_file: args.has(b"LOWER_CASE_FILE"),
        version: args.field(b"VERSION"),
        compat_version: args.field(b"COMPAT_VERSION"),
        version_schema: args.field(b"VERSION_SCHEMA"),
        default_targets: args.list(b"DEFAULT_TARGETS"),
        default_configurations: args.list(b"DEFAULT_CONFIGURATIONS"),
        license: args.field(b"LICENSE"),
        default_license: args.field(b"DEFAULT_LICENSE"),
        description: args.field(b"DESCRIPTION"),
        homepage_url: args.field(b"HOMEPAGE_URL"),
        cxx_modules_directory: args.field(b"CXX_MODULES_DIRECTORY"),
    };
    if spec.version.is_none() {
        if spec.compat_version.is_some() {
            return Err(fail("COMPAT_VERSION requires VERSION."));
        }
        if spec.version_schema.is_some() {
            return Err(fail("VERSION_SCHEMA requires VERSION."));
        }
    }
    if spec.appendix.is_some() {
        for key in [
            b"VERSION".as_slice(),
            b"LICENSE",
            b"DESCRIPTION",
            b"HOMEPAGE_URL",
            b"DEFAULT_TARGETS",
            b"DEFAULT_CONFIGURATIONS",
            b"PROJECT",
        ] {
            if args.has(key) {
                return Err(conflict(b"APPENDIX", key));
            }
        }
    } else if let Some(project) = metadata_project(cx, &spec.project, &spec.package)? {
        if spec.version.is_none() {
            spec.version = project_field(cx, &project, b"VERSION");
            if spec.version.is_some() {
                spec.compat_version = project_field(cx, &project, b"COMPAT_VERSION");
            }
        }
        spec.license = spec.license.or_else(|| project_license(cx, &project));
        spec.description = spec
            .description
            .or_else(|| project_field(cx, &project, b"DESCRIPTION"));
        spec.homepage_url = spec
            .homepage_url
            .or_else(|| project_field(cx, &project, b"HOMEPAGE_URL"));
    }
    Ok(spec)
}

fn sbom(cx: &mut Cx<'_>, args: &Arguments<'_>) -> Result<SbomSpec> {
    let mut spec = SbomSpec {
        name: args.field(b"SBOM").unwrap_or_default(),
        exports: args.list(b"EXPORTS"),
        project: args.project()?,
        version: args.field(b"VERSION"),
        data_license: args.field(b"DATA_LICENSE"),
        default_license: args.field(b"DEFAULT_LICENSE"),
        description: args.field(b"DESCRIPTION"),
        homepage_url: args.field(b"HOMEPAGE_URL"),
        package_url: args.field(b"PACKAGE_URL"),
        format: args
            .one(b"FORMAT")
            .map(|format| sbom_format(b"SBOM", format))
            .transpose()?,
    };
    if spec.exports.is_empty() {
        return Err(fail("SBOM missing EXPORTS."));
    }
    if let Some(project) = metadata_project(cx, &spec.project, &spec.name)? {
        spec.version = spec
            .version
            .or_else(|| project_field(cx, &project, b"VERSION"));
        spec.data_license = spec.data_license.or_else(|| project_license(cx, &project));
        spec.description = spec
            .description
            .or_else(|| project_field(cx, &project, b"DESCRIPTION"));
        spec.homepage_url = spec
            .homepage_url
            .or_else(|| project_field(cx, &project, b"HOMEPAGE_URL"));
    }
    Ok(spec)
}

fn sbom_format(label: &[u8], format: &[u8]) -> Result<BString> {
    match format {
        b"spdx" | b"spdx-3.0+json" | b"spdx-3.0.1+json" => Ok(SPDX_JSON_FORMAT.into()),
        _ => Err(fail(
            [label, b" given invalid format \"", format, b"\"."].concat(),
        )),
    }
}

pub(super) fn install(
    cx: &mut Cx<'_>,
    signature: &[u8],
    input: &[&[u8]],
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    let args = Arguments::parse(signature, input, true)?;
    let lib = type_destination(cx, b"LIB")?;
    if signature == b"PACKAGE_INFO" {
        let spec = package_info(cx, &args)?;
        let name = package_filename_stem(&spec);
        let system = cx.var_or_empty(b"CMAKE_SYSTEM_NAME");
        let windows = if system.is_empty() {
            cx.host().system().sysname == b"Windows"
        } else {
            system == b"Windows"
        };
        let path = if windows {
            [b"cps/", name.as_slice()].concat()
        } else {
            [lib.as_slice(), b"/cps/", &name].concat()
        };
        *options = args.install_options(cx, path.into())?;
        Ok(InstallKind::PackageInfo(Box::new(spec)))
    } else {
        let spec = sbom(cx, &args)?;
        let path = sbom_destination(&lib, &spec.name);
        *options = args.install_options(cx, path)?;
        let path = options.destination.as_ref();
        if cx.project().installs.iter().any(|rule| {
            matches!(&rule.kind, InstallKind::Sbom(previous) if previous.name == spec.name)
                && rule.options.destination.as_ref() == path
        }) {
            return Err(duplicate_sbom(&spec.name));
        }
        Ok(InstallKind::Sbom(Box::new(spec)))
    }
}

fn sbom_destination(lib: &[u8], name: &[u8]) -> BString {
    [lib, b"/sbom/", name].concat().into()
}

fn duplicate_sbom(name: &[u8]) -> rcmake_eval::CmdError {
    fail([b"SBOM command already specified for the file ", name, b"."].concat())
}

fn package_filename_stem(spec: &PackageInfoSpec) -> BString {
    if spec.lower_case_file {
        spec.package.to_ascii_lowercase().into()
    } else {
        spec.package.clone()
    }
}

pub(super) fn export(cx: &mut Cx<'_>, signature: &[u8], input: &[&[u8]]) -> Result<()> {
    let args = Arguments::parse(signature, input, false)?;
    let (kind, file, filename, namespace, modules) = if signature == b"PACKAGE_INFO" {
        let spec = package_info(cx, &args)?;
        require_export_set(cx, &spec.export)?;
        let name = package_filename_stem(&spec);
        let mut filename = name.clone();
        if let Some(appendix) = &spec.appendix {
            filename.push(b'-');
            filename.extend_from_slice(appendix);
        }
        filename.extend_from_slice(b".cps");
        let file = [b"cps/", name.as_slice(), b"/", &filename].concat();
        let namespace = [spec.package.as_slice(), b"::"].concat().into();
        let modules = spec.cxx_modules_directory.clone();
        (
            ExportKind::PackageInfo(spec),
            file,
            filename,
            namespace,
            modules,
        )
    } else {
        let spec = sbom(cx, &args)?;
        for name in &spec.exports {
            require_export_set(cx, name)?;
        }
        let file = [b"sbom/", spec.name.as_slice(), b"/", &spec.name].concat();
        let name = spec.name.clone();
        (ExportKind::Sbom(spec), file, name, BString::default(), None)
    };
    let file = from_native(&cx.current_binary_dir().join(to_native(&file)));
    let file = Lexical::new(&file).normalize();
    if cx
        .project()
        .build_exports
        .iter()
        .any(|export| export.file.as_ref() == Some(&file))
    {
        if signature == b"SBOM" {
            return Err(duplicate_sbom(&filename));
        }
        return Err(fail(
            [
                b"command already specified for the file ",
                filename.as_slice(),
                b".",
            ]
            .concat(),
        ));
    }
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    cx.project().build_exports.push(BuildExport {
        dir,
        backtrace,
        kind,
        set: None,
        targets: Vec::new(),
        namespace,
        file: Some(file),
        append: false,
        cxx_modules_directory: modules,
    });
    Ok(())
}

fn require_export_set(cx: &mut Cx<'_>, set: &[u8]) -> Result<()> {
    if cx.project().exports.contains_key(set) {
        Ok(())
    } else {
        Err(fail([b"Export set \"", set, b"\" not found."].concat()))
    }
}
