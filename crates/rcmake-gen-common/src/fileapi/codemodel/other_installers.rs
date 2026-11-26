use std::collections::BTreeSet;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext, EvalSite, TargetHandle, evaluate, parse};
use rcmake_model::{DirId, FileSet, InstallOptions, InstallRule, Target};
use rcmake_resolve::{
    lower::{install_target_group_options, installs_unnamed_runtime_dependencies},
    usage::UsageResolver,
};

use crate::{GenError, install_script::file_sets};

use super::super::{Bool, FileApi, Members, Object, Value, array, members, object, relative};
use super::{backtraces::Backtraces, directories::applies, installers, number, targets::TargetRef};

pub(super) struct Rows<'a, 'm, 'w> {
    pub(super) api: &'a FileApi<'m, 'w>,
    pub(super) config: ConfigId,
}

impl Rows<'_, '_, '_> {
    pub(super) fn file_sets(
        &self,
        index: usize,
        rule: &InstallRule,
        reference: &TargetRef<'_>,
        raw: &Target,
    ) -> Result<Vec<Members>, GenError> {
        raw.file_sets
            .iter()
            .filter_map(|&id| {
                let set = self.api.model.file_set(id)?;
                let options = file_sets::options_for_set(self.api.model, index, &set.name)?;
                Some(self.file_set(rule, reference, raw, set, &options))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| rows.into_iter().flatten().collect())
    }

    fn file_set(
        &self,
        rule: &InstallRule,
        reference: &TargetRef<'_>,
        raw: &Target,
        set: &FileSet,
        options: &InstallOptions,
    ) -> Result<Option<Members>, GenError> {
        let Some(mut row) = self.base(
            rule.dir,
            &rule.backtrace,
            options,
            "fileSet",
            (b"CMAKE_INSTALL_INCLUDEDIR", b"include"),
        )?
        else {
            return Ok(None);
        };
        let mut usage = UsageResolver::new(self.api.model);
        let bases = file_sets::set_files(self.api.model, &mut usage, raw, set, &self.config)?;
        let source_dir = &self.api.model.dir(raw.dir).source_dir;
        let context = EvalContext {
            config: self.config.clone(),
            head: u32::try_from(set.target.index()).ok().map(TargetHandle),
            ..EvalContext::default()
        };
        let ordered_bases = if set.base_dirs.is_empty() {
            vec![Lexical::new(source_dir).normalize()]
        } else {
            let raw_bases = usage
                .file_set_paths(set.target, b"BASE_DIRS", &set.base_dirs, &context)
                .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            let dir_lex = Lexical::new(source_dir);
            let mut seen = BTreeSet::new();
            raw_bases
                .into_iter()
                .map(|path| Lexical::new(&dir_lex.append(&path.value)).normalize())
                .filter(|path| seen.insert(path.clone()))
                .collect()
        };
        row.insert(
            "paths".into(),
            array(
                bases
                    .values()
                    .flatten()
                    .map(|(from, to)| installers::path(self.api.source, from, to)),
            ),
        );
        row.insert(
            "fileSetDirectories".into(),
            array(
                ordered_bases
                    .iter()
                    .map(|base| Value::string(relative(base, self.api.source))),
            ),
        );
        row.insert("fileSetName".into(), Value::string(set.name.clone()));
        row.insert("fileSetType".into(), Value::string(set.kind.as_str()));
        row.insert("fileSetTarget".into(), target_ref(reference));
        Ok(Some(row))
    }

    pub(super) fn bmi(
        &self,
        index: usize,
        rule: &InstallRule,
        reference: &TargetRef<'_>,
    ) -> Result<Option<Members>, GenError> {
        let Some(options) = install_target_group_options(self.api.model, index, b"CXX_MODULES_BMI")
            .filter(|options| {
                options
                    .destination
                    .as_ref()
                    .is_some_and(|path| !path.is_empty())
            })
        else {
            return Ok(None);
        };
        let mut row = self.base(
            rule.dir,
            &rule.backtrace,
            &options,
            "cxxModuleBmi",
            (b"", b""),
        )?;
        if let Some(row) = &mut row {
            row.insert("cxxModuleBmiTarget".into(), target_ref(reference));
        }
        Ok(row)
    }

    pub(super) fn unnamed_dependencies(
        &self,
        index: usize,
        rule: &InstallRule,
    ) -> Result<Vec<Members>, GenError> {
        let model = self.api.model;
        if !installs_unnamed_runtime_dependencies(model, index) {
            return Ok(Vec::new());
        }
        let platform = self.platform(rule.dir);
        let library = if platform == b"Windows" {
            b"RUNTIME".as_slice()
        } else {
            b"LIBRARY"
        };
        let Some(library) = install_target_group_options(model, index, library) else {
            return Ok(Vec::new());
        };
        let framework = (platform == b"Darwin")
            .then(|| install_target_group_options(model, index, b"FRAMEWORK"))
            .flatten();
        self.dependencies(rule.dir, &rule.backtrace, b"", &library, framework.as_ref())
    }

    pub(super) fn dependencies(
        &self,
        dir: DirId,
        backtrace: &Backtrace,
        name: &[u8],
        library: &InstallOptions,
        framework: Option<&InstallOptions>,
    ) -> Result<Vec<Members>, GenError> {
        let default = if self.platform(dir) == b"Windows" {
            (b"CMAKE_INSTALL_BINDIR".as_slice(), b"bin".as_slice())
        } else {
            (b"CMAKE_INSTALL_LIBDIR".as_slice(), b"lib".as_slice())
        };
        let library = self.dependency(dir, backtrace, name, library, "library", default)?;
        let framework = framework
            .map(|options| self.dependency(dir, backtrace, name, options, "framework", (b"", b"")))
            .transpose()?
            .flatten();
        Ok(library.into_iter().chain(framework).collect())
    }

    fn dependency(
        &self,
        dir: DirId,
        backtrace: &Backtrace,
        name: &[u8],
        options: &InstallOptions,
        kind: &str,
        default: (&[u8], &[u8]),
    ) -> Result<Option<Members>, GenError> {
        let Some(mut row) = self.base(dir, backtrace, options, "runtimeDependencySet", default)?
        else {
            return Ok(None);
        };
        row.remove(b"isOptional".as_slice());
        row.insert("runtimeDependencySetType".into(), Value::string(kind));
        if !name.is_empty() {
            row.insert("runtimeDependencySetName".into(), Value::string(name));
        }
        Ok(Some(row))
    }

    fn platform(&self, dir: DirId) -> &[u8] {
        self.api
            .model
            .definition(dir, b"CMAKE_SYSTEM_NAME")
            .map_or(b"", |value| value.as_slice())
    }

    fn base(
        &self,
        dir: DirId,
        backtrace: &Backtrace,
        options: &InstallOptions,
        kind: &str,
        (variable, fallback): (&[u8], &[u8]),
    ) -> Result<Option<Members>, GenError> {
        if !applies(&options.configurations, &self.config.0) {
            return Ok(None);
        }
        let model = self.api.model;
        let destination = options
            .destination
            .as_ref()
            .filter(|path| !path.is_empty())
            .or_else(|| model.definition(dir, variable))
            .map_or(fallback, |path| path.as_slice());
        let destination = if destination.windows(2).any(|pair| pair == b"$<") {
            let context = EvalContext {
                config: self.config.clone(),
                site: EvalSite {
                    backtrace: backtrace.clone(),
                    property: None,
                },
                ..EvalContext::default()
            };
            let genex =
                parse(destination).map_err(|err| GenError::Invalid(err.to_string().into()))?;
            let mut usage = UsageResolver::new(model);
            evaluate(&genex, &context, &mut usage.host(dir))
                .map_err(|err| GenError::Invalid(err.to_string().into()))?
                .0
        } else {
            BString::from(destination)
        };
        let destination = {
            let normalized = rcmake_base::path_lex::cmake_style(&destination);
            let mut slice = normalized.as_slice();
            while slice.len() > 1 && !slice.ends_with(b":/") {
                let Some(trimmed) = slice.strip_suffix(b"/") else {
                    break;
                };
                slice = trimmed;
            }
            BString::from(slice)
        };
        let component = options
            .component
            .as_ref()
            .or_else(|| model.definition(dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME"))
            .map_or(b"Unspecified".as_slice(), |name| name.as_slice());
        let mut row = members([
            ("type", Value::string(kind)),
            ("component", Value::string(component)),
            ("destination", Value::string(destination)),
        ]);
        if options.optional {
            row.insert("isOptional".into(), Bool(true));
        }
        if options.exclude_from_all {
            row.insert("isExcludeFromAll".into(), Bool(true));
        }
        Ok(Some(row))
    }
}

pub(super) fn append(
    output: &mut Vec<Value>,
    rows: impl IntoIterator<Item = Members>,
    backtrace: &Backtrace,
    graph: &mut Backtraces<'_>,
) {
    for mut row in rows {
        graph.tag(&mut row, backtrace);
        output.push(Object(row));
    }
}

fn target_ref(reference: &TargetRef<'_>) -> Value {
    object([
        ("id", Value::string(reference.id.as_str())),
        ("index", number(reference.index)),
    ])
}
