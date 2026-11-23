use rcmake_base::{BString, ByteSlice, path_lex::Lexical, truth};
use rcmake_diag::{
    Backtrace, DiagCategory, Diagnostic, Effect, Location, MessageKind, RenderOptions, render,
};
use rcmake_genex::PreprocessMode;
use rcmake_model::{DirId, ExportKind, InstallKind, ProjectModel, Target, TargetId};
use rcmake_resolve::GenerationResult;

use super::cps::export_name;
use crate::GenError;

pub(crate) struct TargetExport {
    pub(crate) set: Option<BString>,
    pub(crate) namespace: BString,
    pub(crate) file: BString,
}

pub(crate) fn exports(
    model: &ProjectModel,
    result: &GenerationResult,
    id: TargetId,
    mode: PreprocessMode,
) -> Result<Vec<TargetExport>, GenError> {
    let contains = |name: &[u8]| {
        model
            .exports
            .get(name)
            .is_some_and(|set| set.targets.contains(&id))
    };
    let mut exports = Vec::new();
    if mode == PreprocessMode::Install {
        for rule in &model.installs {
            let (set, namespace, destination, file) = match &rule.kind {
                InstallKind::Export {
                    set,
                    namespace,
                    file,
                    ..
                } if contains(set) => (
                    set,
                    namespace.clone(),
                    rule.options.destination.clone().unwrap_or_default(),
                    file.clone()
                        .unwrap_or_else(|| format!("{set}.cmake").into()),
                ),
                InstallKind::PackageInfo(spec) if contains(&spec.export) => (
                    &spec.export,
                    format!("{}::", spec.package).into(),
                    super::cps::install_destination(model, rule, spec)?,
                    cps_file(spec)?,
                ),
                _ => continue,
            };
            exports.push(TargetExport {
                set: Some(set.clone()),
                namespace,
                file: Lexical::new(&destination).append(&file),
            });
        }
    } else {
        let target_name = model.target(id).map(export_name);
        for plan in result.exports.iter().filter(|plan| !plan.install) {
            let member = if plan.set.is_empty() {
                target_name
                    .as_ref()
                    .is_some_and(|name| plan.targets.iter().any(|target| &target.name == name))
            } else {
                contains(&plan.set)
            };
            if member {
                exports.push(TargetExport {
                    set: Some(plan.set.clone()).filter(|set| !set.is_empty()),
                    namespace: plan.namespace.clone(),
                    file: plan.file.clone(),
                });
            }
        }
        for rule in &model.build_exports {
            let (set, namespace, file) = match &rule.kind {
                ExportKind::CMake
                    if rule.targets.contains(&id)
                        || rule.set.as_ref().is_some_and(|set| contains(set)) =>
                {
                    let Some(file) = &rule.file else { continue };
                    (rule.set.clone(), rule.namespace.clone(), file.clone())
                }
                ExportKind::PackageInfo(spec) if contains(&spec.export) => {
                    let directory = rcmake_cps::naming::build_directory(
                        &spec.package,
                        &model.dir(rule.dir).binary_dir,
                        spec.lower_case_file,
                    )
                    .map_err(super::cps::cps_error)?;
                    (
                        Some(spec.export.clone()),
                        format!("{}::", spec.package).into(),
                        Lexical::new(&directory).append(&cps_file(spec)?),
                    )
                }
                _ => continue,
            };
            if !exports.iter().any(|entry| entry.file == file) {
                exports.push(TargetExport {
                    set,
                    namespace,
                    file,
                });
            }
        }
        exports.sort_by(|a, b| a.file.cmp(&b.file));
    }
    Ok(exports)
}

fn cps_file(spec: &rcmake_model::PackageInfoSpec) -> Result<BString, GenError> {
    rcmake_cps::naming::package_file_name(
        &spec.package,
        spec.appendix.as_ref().map(|value| value.as_slice()),
        spec.lower_case_file,
    )
    .map_err(super::cps::cps_error)
}

pub(crate) fn imported_package<'a>(model: &ProjectModel, target: &'a Target) -> Option<&'a [u8]> {
    target
        .props
        .get(b"EXPORT_FIND_PACKAGE_NAME")
        .filter(|name| !name.is_empty())
        .or_else(|| {
            target
                .props
                .get(b"_RCMAKE_FIND_PACKAGE_NAME")
                .filter(|name| !name.is_empty())
        })
        .map(|name| name.as_slice())
        .or_else(|| {
            let (namespace, _) = target.name.split_once_str(b"::")?;
            package_variable(model, target.dir, namespace, b"_FOUND")
                .filter(|found| truth::is_on(found))
                .map(|_| namespace)
        })
}

pub(crate) fn unknown_import(consumer: &Target, target: &Target) -> BString {
    format!(
        r#"Target "{}" references imported target "{}" which does not come from any known package."#,
        consumer.name, target.name,
    ).into()
}

pub(crate) fn author_warnings_enabled(model: &ProjectModel) -> bool {
    !model
        .definition(DirId::ROOT, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
}

pub(crate) fn imported_version<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
    package: &[u8],
) -> Option<&'a BString> {
    target
        .props
        .get(b"_RCMAKE_FIND_PACKAGE_VERSION")
        .filter(|version| !version.is_empty())
        .or_else(|| package_variable(model, target.dir, package, b"_VERSION"))
}

pub(crate) fn package_variable<'a>(
    model: &'a ProjectModel,
    dir: DirId,
    package: &[u8],
    suffix: &[u8],
) -> Option<&'a BString> {
    model
        .definition(dir, &[package, suffix].concat())
        .filter(|value| !value.is_empty() && !value.ends_with(b"-NOTFOUND"))
}

pub(crate) fn component(target: &Target, namespace: &[u8]) -> BString {
    if target.is_imported() {
        return target
            .name
            .strip_prefix(namespace)
            .unwrap_or(&target.name)
            .into();
    }
    export_name(target)
}

pub(crate) fn generator_diagnostic(body: impl Into<BString>, author_warning: bool) -> BString {
    let diagnostic = Diagnostic {
        kind: if author_warning {
            MessageKind::AuthorWarning
        } else {
            MessageKind::Error
        },
        effect: if author_warning {
            Effect::None
        } else {
            Effect::Fatal
        },
        category: author_warning.then_some(DiagCategory::Author),
        policy: None,
        location: Location::FileLine {
            file: b"CMakeLists.txt".as_slice().into(),
            line: 0,
        },
        body: body.into(),
    };
    render(&diagnostic, &RenderOptions::default())
}

pub(crate) fn author_warning(
    model: &ProjectModel,
    backtrace: &Backtrace,
    body: impl Into<BString>,
) -> Option<BString> {
    if !author_warnings_enabled(model) {
        return None;
    }
    let frames = backtrace
        .frames()
        .filter(|frame| frame.line != 0 || frame.name.is_some());
    let backtrace = frames
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .fold(Backtrace::default(), |backtrace, frame| {
            backtrace.push(frame)
        });
    Some(render(
        &Diagnostic {
            kind: MessageKind::AuthorWarning,
            effect: Effect::None,
            category: Some(DiagCategory::Author),
            policy: None,
            location: Location::Backtrace(backtrace),
            body: body.into(),
        },
        &RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        },
    ))
}

pub(crate) fn generation_error(messages: impl IntoIterator<Item = BString>) -> GenError {
    let mut error = BString::from("\n");
    for message in messages {
        error.extend_from_slice(&generator_diagnostic(message, false));
    }
    GenError::Invalid(error)
}
