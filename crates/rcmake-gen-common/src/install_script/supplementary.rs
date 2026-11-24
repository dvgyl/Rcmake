use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use rcmake_base::{BString, hash::HashAlgo, list, path_lex::Lexical};
use rcmake_cps::{naming, writer};
use rcmake_genex::{EvalContext, PreprocessMode};
use rcmake_host::{
    Host, Stream,
    path::{from_native, to_native},
};
use rcmake_model::{
    DirId, ExportKind, InstallKind, InstallOptions, InstallRule, ProjectMetadata, ProjectModel,
    SbomSpec,
};
use rcmake_resolve::{
    BuildStep, GenerationResult, InstallAction, InstallActionKind, InstallManifest,
    usage::UsageResolver,
};

use super::{
    file_sets::{options_for_set, set_files},
    install_command,
};
use crate::{
    GenError, cpack_properties,
    export::{
        cps, output_error,
        provenance::{author_warning, generation_error},
        write_output,
    },
    genex, quote, sbom,
};

/// Write package information, SPDX documents, and CPack properties.
///
/// Return the supplemental install actions for the caller's install manifest.
/// These actions also install source file sets and C++ module files.
///
/// # Errors
///
/// Return an error for invalid export data, conflicting output paths, or a write failure.
pub fn write_supplementary_outputs(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
    bin: &Path,
) -> Result<InstallManifest, GenError> {
    let mut outputs = Outputs::default();
    if let Some(properties) = cpack_properties::render_resolved(model, &result.configs)? {
        outputs.add(bin.join("CPackProperties.cmake"), properties)?;
    }
    let now = host
        .env_get(b"SOURCE_DATE_EPOCH")
        .and_then(|value| std::str::from_utf8(&value).ok()?.parse().ok())
        .unwrap_or_else(|| host.now().secs);
    for export in &model.build_exports {
        let directory = &model.dir(export.dir).binary_dir;
        match &export.kind {
            ExportKind::PackageInfo(spec) => {
                outputs.cps_schema_warning(model, spec, &export.backtrace);
                let files = cps::render_for_tree(model, result, spec, None, &mut outputs.warnings)?;
                let directory =
                    naming::build_directory(&spec.package, directory, spec.lower_case_file)
                        .map_err(cps::cps_error)?;
                outputs.add(
                    to_native(&directory).join(to_native(&files.file_name)),
                    writer::write_build_package(&files.package)
                        .map_err(|error| generation_error([error.to_string().into()]))?,
                )?;
            }
            ExportKind::Sbom(spec) => {
                validate_fragment(&spec.name)?;
                let directory = to_native(directory)
                    .join("sbom")
                    .join(to_native(&spec.name));
                outputs.sbom(model, result, spec, export.dir, &directory, None, now)?;
            }
            ExportKind::CMake => {}
        }
    }
    for (index, rule) in model.installs.iter().enumerate() {
        if dir_excluded_from_all(model, rule.dir) {
            continue;
        }
        let directory = to_native(&model.dir(rule.dir).binary_dir);
        match &rule.kind {
            InstallKind::PackageInfo(spec) => {
                outputs.cps_schema_warning(model, spec, &rule.backtrace);
                let files =
                    cps::render_for_tree(model, result, spec, Some(rule), &mut outputs.warnings)?;
                let destination = cps::install_destination(model, rule, spec)?;
                let directory = stage(&directory, "Export", &destination);
                let path = directory.join(to_native(&files.file_name));
                outputs.add(
                    path.clone(),
                    writer::write_package(&files.package)
                        .map_err(|error| generation_error([error.to_string().into()]))?,
                )?;
                let config = rule.options.configurations.clone();
                outputs.install(
                    model,
                    rule.dir,
                    &rule.options,
                    &destination,
                    config,
                    vec![from_native(&path)],
                );
                for (name, supplement) in files.supplements {
                    let path = directory.join(to_native(&name));
                    outputs.add(
                        path.clone(),
                        writer::write_supplement(&supplement).map_err(cps::cps_error)?,
                    )?;
                    let config = original_config(result, &supplement.configuration);
                    outputs.install(
                        model,
                        rule.dir,
                        &rule.options,
                        &destination,
                        vec![config],
                        vec![from_native(&path)],
                    );
                }
            }
            InstallKind::Sbom(spec) => {
                let destination = sbom_destination(model, rule.dir, spec, &rule.options)?;
                let directory = stage(&directory, "sbom", &destination);
                outputs.sbom(
                    model,
                    result,
                    spec,
                    rule.dir,
                    &directory,
                    Some((&destination, &rule.options)),
                    now,
                )?;
            }
            InstallKind::Targets { .. } => outputs.file_sets(model, result, index, rule)?,
            _ => {}
        }
    }
    for spec in automatic_sboms(model)? {
        let options = InstallOptions::default();
        let destination = sbom_destination(model, DirId::ROOT, &spec, &options)?;
        let directory = stage(bin, "sbom", &destination);
        outputs.sbom(
            model,
            result,
            &spec,
            DirId::ROOT,
            &directory,
            Some((&destination, &options)),
            now,
        )?;
    }
    outputs.bmis(result, bin);
    for (path, content) in outputs.files {
        write_output(host, &path, &content)?;
    }
    for warning in outputs.warnings {
        host.write_out(Stream::Stderr, &warning);
    }
    Ok(outputs.manifest)
}

fn dir_excluded_from_all(model: &ProjectModel, mut dir: DirId) -> bool {
    loop {
        let info = model.dir(dir);
        if info.exclude_from_all {
            return true;
        }
        let Some(parent) = info.parent else {
            return false;
        };
        dir = parent;
    }
}

fn original_config(result: &GenerationResult, name: &[u8]) -> BString {
    cps::configurations(result)
        .find(|config| cps::config_name(&config.0) == name)
        .map_or_else(|| name.into(), |config| config.0)
}

fn stage(bin: &Path, kind: &str, destination: &[u8]) -> PathBuf {
    bin.join("CMakeFiles")
        .join(kind)
        .join(HashAlgo::Md5.hex_digest(destination))
}

fn sbom_destination(
    model: &ProjectModel,
    dir: DirId,
    spec: &SbomSpec,
    options: &InstallOptions,
) -> Result<BString, GenError> {
    validate_fragment(&spec.name)?;
    Ok(options.destination.clone().unwrap_or_else(|| {
        let lib = model
            .definition(dir, b"CMAKE_INSTALL_LIBDIR")
            .filter(|value| !value.is_empty())
            .map_or(b"lib".as_slice(), |value| value.as_slice());
        Lexical::new(lib).append(&[b"sbom/", spec.name.as_slice()].concat())
    }))
}

fn validate_fragment(name: &[u8]) -> Result<(), GenError> {
    if name.is_empty()
        || matches!(name, b"." | b"..")
        || name.iter().any(|byte| b"/\\:\0".contains(byte))
    {
        return Err(GenError::Invalid(
            format!("Invalid output name \"{}\".", BString::from(name)).into(),
        ));
    }
    Ok(())
}

fn automatic_sboms(model: &ProjectModel) -> Result<Vec<SbomSpec>, GenError> {
    let Some(formats) = model.definition(DirId::ROOT, b"CMAKE_INSTALL_SBOM_FORMATS") else {
        return Ok(Vec::new());
    };
    let formats = list::split(formats, list::Empty::Drop);
    for format in &formats {
        sbom::validate_format(format)?;
    }
    let Some(format) = formats.into_iter().next() else {
        return Ok(Vec::new());
    };
    let project = model
        .definition(DirId::ROOT, b"PROJECT_NAME")
        .cloned()
        .map_or(ProjectMetadata::None, ProjectMetadata::Named);
    Ok(model
        .exports
        .keys()
        .filter(|name| {
            !model.installs.iter().any(
                |rule| matches!(&rule.kind, InstallKind::Sbom(spec) if spec.exports.contains(name)),
            )
        })
        .map(|name| SbomSpec {
            name: name.clone(),
            exports: vec![name.clone()],
            project: project.clone(),
            format: Some(format.clone()),
            ..SbomSpec::default()
        })
        .collect())
}

#[derive(Default)]
struct Outputs {
    files: BTreeMap<PathBuf, BString>,
    manifest: InstallManifest,
    warnings: BTreeSet<BString>,
}

impl Outputs {
    fn cps_schema_warning(
        &mut self,
        model: &ProjectModel,
        spec: &rcmake_model::PackageInfoSpec,
        backtrace: &rcmake_diag::Backtrace,
    ) {
        if let Some(schema) = &spec.version_schema
            && !matches!(
                schema.as_slice(),
                b"" | b"simple" | b"custom" | b"pep440" | b"rpm" | b"dpkg"
            )
        {
            self.warnings.extend(author_warning(
                model,
                backtrace,
                format!(
                    r#"Package "{}" uses unrecognized version schema "{schema}"."#,
                    spec.package,
                ),
            ));
        }
    }

    fn add(&mut self, path: PathBuf, content: BString) -> Result<(), GenError> {
        use std::collections::btree_map::Entry;
        match self.files.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert(content);
            }
            Entry::Occupied(entry) => {
                return Err(output_error(
                    &from_native(entry.key()),
                    "Multiple exports write this file.",
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn sbom(
        &mut self,
        model: &ProjectModel,
        result: &GenerationResult,
        spec: &SbomSpec,
        dir: DirId,
        directory: &Path,
        install: Option<(&[u8], &InstallOptions)>,
        now: i64,
    ) -> Result<(), GenError> {
        let mode = if install.is_some() {
            PreprocessMode::Install
        } else {
            PreprocessMode::Build
        };
        for config in cps::configurations(result) {
            if install.is_some_and(|(_, options)| {
                !cps::matches_configuration(&config, &options.configurations)
            }) {
                continue;
            }
            let suffix = cps::config_name(&config.0);
            validate_fragment(&suffix)?;
            let name = [spec.name.as_slice(), b"-", &suffix, b".spdx.json"].concat();
            let path = directory.join(to_native(&name));
            let document = sbom::render_for_tree(model, result, spec, dir, &config.0, mode, now)?;
            self.warnings.extend(document.warnings);
            self.add(path.clone(), document.contents)?;
            if let Some((destination, options)) = install {
                self.install(
                    model,
                    dir,
                    options,
                    destination,
                    vec![config.0],
                    vec![from_native(&path)],
                );
            }
        }
        Ok(())
    }

    fn install(
        &mut self,
        model: &ProjectModel,
        dir: DirId,
        options: &InstallOptions,
        destination: &[u8],
        configurations: Vec<BString>,
        files: Vec<BString>,
    ) {
        let component = options
            .component
            .clone()
            .or_else(|| {
                model
                    .definition(dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .cloned()
            })
            .unwrap_or_else(|| "Unspecified".into());
        let action = InstallAction {
            component,
            destination: destination.into(),
            configurations,
            kind: InstallActionKind::Files {
                file_type: "FILE".into(),
                files,
                permissions: options.permissions.clone(),
                rename: options.rename.clone(),
            },
            optional: options.optional,
        };
        self.push(action, options.exclude_from_all);
    }

    fn push(&mut self, mut action: InstallAction, exclude: bool) {
        if exclude {
            let mut code = BString::from("if(CMAKE_INSTALL_COMPONENT STREQUAL ");
            quote(&mut code, &action.component);
            code.extend_from_slice(b")\n");
            let _ = super::render_payload(&mut code, &action, &Default::default(), b"  ");
            code.extend_from_slice(b"endif()");
            action.kind = InstallActionKind::Code(code);
        }
        self.manifest.actions.push(action);
    }

    fn file_sets(
        &mut self,
        model: &ProjectModel,
        result: &GenerationResult,
        index: usize,
        rule: &InstallRule,
    ) -> Result<(), GenError> {
        let InstallKind::Targets { targets, .. } = &rule.kind else {
            return Ok(());
        };
        let mut usage = UsageResolver::new(model);
        let default = model
            .definition(rule.dir, b"CMAKE_INSTALL_INCLUDEDIR")
            .map_or(b"include".as_slice(), |value| value.as_slice());
        for target in targets.iter().filter_map(|&id| model.target(id)) {
            for set in target.file_sets.iter().filter_map(|&id| model.file_set(id)) {
                let Some(options) = options_for_set(model, index, &set.name) else {
                    continue;
                };
                let mut configurations = Vec::new();
                for config in cps::configurations(result)
                    .filter(|config| cps::matches_configuration(config, &options.configurations))
                {
                    let ctx = EvalContext {
                        config: config.clone(),
                        ..EvalContext::default()
                    };
                    let destination = genex(
                        &mut usage,
                        rule.dir,
                        &ctx,
                        options
                            .destination
                            .as_deref()
                            .map_or(default, Vec::as_slice),
                        None,
                    )?;
                    let mut destinations = BTreeMap::<BString, Vec<BString>>::new();
                    for (path, relative) in set_files(model, &mut usage, target, set, &config)?
                        .into_values()
                        .flatten()
                    {
                        let parent = Lexical::new(&relative).parent();
                        let destination = if parent.is_empty() {
                            destination.clone()
                        } else {
                            Lexical::new(&destination).append(parent)
                        };
                        destinations.entry(destination).or_default().push(path);
                    }
                    configurations.push((config, destinations));
                }
                let same_for_all = options.configurations.is_empty()
                    && configurations
                        .iter()
                        .map(|(_, files)| files)
                        .all(|files| Some(files) == configurations.first().map(|(_, files)| files));
                for (config, destinations) in configurations {
                    for (destination, paths) in destinations {
                        let configs = if same_for_all {
                            Vec::new()
                        } else {
                            vec![config.0.clone()]
                        };
                        self.install(model, rule.dir, &options, &destination, configs, paths);
                    }
                    if same_for_all {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn bmis(&mut self, result: &GenerationResult, bin: &Path) {
        for graph in &result.graphs {
            for step in &graph.steps {
                let BuildStep::Collate {
                    target, dd, ddis, ..
                } = step
                else {
                    continue;
                };
                let Some(rule) = result
                    .targets
                    .iter()
                    .find(|facts| facts.config == graph.config && facts.metadata.name == *target)
                    .and_then(|facts| facts.cxx_modules.as_ref())
                    .and_then(|modules| modules.bmi_install.as_ref())
                    .filter(|rule| !rule.destination.is_empty())
                else {
                    continue;
                };
                let table = bin.join(to_native(dd)).with_file_name("CXXModules.json");
                let action = InstallAction {
                    component: rule.component.clone(),
                    destination: rule.destination.clone(),
                    configurations: vec![graph.config.0.clone()],
                    kind: InstallActionKind::Code(BString::default()),
                    optional: rule.optional,
                };
                let mut script = BString::from("if(EXISTS ");
                quote(&mut script, &from_native(&table));
                script.extend_from_slice(b")\n  file(READ ");
                quote(&mut script, &from_native(&table));
                script.extend_from_slice(b" _rcmake_modules)\n  foreach(_rcmake_ddi IN ITEMS");
                for ddi in ddis {
                    script.push(b' ');
                    quote(&mut script, &from_native(&bin.join(to_native(ddi))));
                }
                script.extend_from_slice(br#")
    file(READ "${_rcmake_ddi}" _rcmake_scan)
    string(JSON _rcmake_providers ERROR_VARIABLE _rcmake_error GET "${_rcmake_scan}" rules 0 provides)
    if(NOT _rcmake_error)
      string(JSON _rcmake_count LENGTH "${_rcmake_providers}")
      if(_rcmake_count GREATER 0)
        math(EXPR _rcmake_last "${_rcmake_count} - 1")
        foreach(_rcmake_index RANGE "${_rcmake_last}")
          string(JSON _rcmake_name GET "${_rcmake_providers}" "${_rcmake_index}" logical-name)
          string(JSON _rcmake_bmi GET "${_rcmake_modules}" modules "${_rcmake_name}" bmi)
          if(NOT IS_ABSOLUTE "${_rcmake_bmi}")
            set(_rcmake_bmi ""#);
                crate::quoted_content(&mut script, &from_native(bin));
                script.extend_from_slice(
                    br#"/${_rcmake_bmi}")
          endif()
          "#,
                );
                install_command(&mut script, &action, b"FILE");
                if !rule.permissions.is_empty() {
                    script.extend_from_slice(b" PERMISSIONS");
                    super::arguments(&mut script, &rule.permissions);
                }
                script.extend_from_slice(
                    br#" FILES "${_rcmake_bmi}")
        endforeach()
      endif()
    endif()
  endforeach()
"#,
                );
                if !rule.optional {
                    script.extend_from_slice(b"else()\n  message(FATAL_ERROR ");
                    quote(
                        &mut script,
                        &format!("Cannot find C++ module metadata at {}.", table.display())
                            .into_bytes(),
                    );
                    script.extend_from_slice(b")\n");
                }
                script.extend_from_slice(b"endif()");
                self.push(
                    InstallAction {
                        kind: InstallActionKind::Code(script),
                        ..action
                    },
                    rule.exclude_from_all,
                );
            }
        }
    }
}
