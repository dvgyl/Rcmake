// SPDX-License-Identifier: MIT OR Apache-2.0
//! Lower Fortran module dependencies and prepare compiler inputs.

#![forbid(unsafe_code)]

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_fortran::{DependSource, FortranCompiler, FortranTdi, ModuleCase};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path};
use rcmake_model::{BT, DirId, GeneratorKind, ProjectModel, Target};

use super::compile::compile_arguments;
use super::shell::{command, direct, join_words, rule_invocations};
use super::{definition, join, object_dir, target_property};
use crate::model::{DependInfo, FortranTargetInfo};
use crate::{BuildStep, CompileGroup, CompileUnit, ResolveError, ResolvedTarget};

/// Fortran inputs for one target and configuration.
pub(crate) struct FortranPlan {
    /// Build paths to record on the resolved target.
    pub(crate) info: FortranTargetInfo,
    /// Dependency information for Ninja or Unix Makefiles.
    pub(crate) depend_info: DependInfo,
    units: IndexMap<BString, DependSource>,
    ninja: bool,
}

impl FortranPlan {
    /// Create a plan if the target has Fortran units.
    ///
    /// Supply the resolved link closure in link order for this configuration.
    /// It includes modules from targets reached through interfaces.
    ///
    /// # Errors
    ///
    /// Returns an error if the model is missing a target or a compile group.
    pub(crate) fn new<'a>(
        model: &ProjectModel,
        resolved: &ResolvedTarget,
        linked: impl IntoIterator<Item = &'a ResolvedTarget>,
    ) -> Result<Option<Self>, ResolveError> {
        let target = model
            .target(resolved.id)
            .ok_or_else(|| ResolveError::Message("The Fortran target is missing.".into()))?;
        let top = &model.dir(DirId::ROOT).binary_dir;
        let current = &resolved.metadata.binary_dir;
        let get = |key: &[u8]| definition(model, target.dir, key);
        let ninja = matches!(
            model.generator,
            Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
        );
        let mut units = IndexMap::new();
        let mut includes = IndexSet::new();
        for unit in &resolved.compile_units {
            let group = resolved.compile_groups.get(unit.group).ok_or_else(|| {
                ResolveError::Message("The compile group for a source is missing.".into())
            })?;
            if group.language != "Fortran" {
                continue;
            }
            let source = join(&resolved.metadata.source_dir, &unit.source.value);
            let preprocessed = (ninja
                && preprocess(model, target, &source) != Some(false)
                && get(b"CMAKE_Fortran_PREPROCESS_SOURCE")
                    .is_some_and(|rule| !rule.trim_ascii().is_empty()))
            .then(|| preprocessed_path(model, target, &source, &unit.object));
            includes.extend(
                group
                    .includes
                    .iter()
                    .map(|include| include.path.value.clone()),
            );
            units.insert(
                unit.object.clone(),
                DependSource {
                    object: unit.object.clone(),
                    source,
                    preprocessed,
                },
            );
        }
        if units.is_empty() {
            return Ok(None);
        }
        let dir = support_dir(model, target, resolved);
        let linked_target_dirs = linked
            .into_iter()
            .filter(|other| other.id != resolved.id && other.config == resolved.config)
            .filter(|other| {
                other
                    .compile_groups
                    .iter()
                    .any(|group| group.language == "Fortran")
            })
            .filter_map(|other| Some(support_dir(model, model.target(other.id)?, other)))
            .map(|dir| join(top, &dir))
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let module_dir = target_property(model, target, b"Fortran_MODULE_DIRECTORY")
            .filter(|value| !value.is_empty())
            .map(|value| Lexical::new(&join(current, value)).normalize())
            .or_else(|| (top != current).then(|| current.clone()))
            .or_else(|| {
                get(b"CMAKE_Fortran_MODDIR_DEFAULT")
                    .filter(|value| !value.is_empty())
                    .map(|value| Lexical::new(&join(top, value)).normalize())
            });
        let compiler_id = get(b"CMAKE_Fortran_COMPILER_ID")
            .cloned()
            .unwrap_or_default();
        let compiler = FortranCompiler::from_id(&compiler_id);
        let module_case = match get(b"CMAKE_Fortran_MODULE_CASE").map(|value| value.as_slice()) {
            Some(value) if value.eq_ignore_ascii_case(b"upper") => ModuleCase::Upper,
            Some(value) if value.eq_ignore_ascii_case(b"lower") => ModuleCase::Lower,
            _ => compiler.module_case,
        };
        let tdi = FortranTdi {
            dir_cur_bld: current.clone(),
            dir_top_bld: top.clone(),
            include_dirs: includes.into_iter().collect(),
            module_dir: module_dir.clone(),
            linked_target_dirs: linked_target_dirs.clone(),
            submodule_sep: get(b"CMAKE_Fortran_SUBMODULE_SEP")
                .cloned()
                .unwrap_or_else(|| compiler.submodule_sep.into()),
            submodule_ext: get(b"CMAKE_Fortran_SUBMODULE_EXT")
                .cloned()
                .unwrap_or_else(|| compiler.submodule_ext.into()),
            module_case,
            compiler_id,
            sources: units.values().cloned().collect(),
        };
        Ok(Some(Self {
            info: FortranTargetInfo {
                module_dir,
                depend_info: join(
                    &dir,
                    if ninja {
                        b"FortranDependInfo.json"
                    } else {
                        b"DependInfo.cmake"
                    },
                ),
                dyndep: join(&dir, b"Fortran.dd"),
                modules_json: join(&dir, b"FortranModules.json"),
                linked_target_dirs,
            },
            depend_info: DependInfo::Fortran(tdi),
            units,
            ninja,
        }))
    }

    /// Create the compiler's module output directory through the host.
    ///
    /// # Errors
    ///
    /// Returns an error if the host cannot create the directory.
    pub(crate) fn create_module_dir(&self, host: &dyn Host) -> Result<(), ResolveError> {
        if let Some(directory) = &self.info.module_dir {
            host.create_dir_all(&path::to_native(directory))
                .map_err(|error| {
                    ResolveError::Message(
                        format!("Cannot create Fortran module directory {directory}: {error}")
                            .into(),
                    )
                })?;
        }
        Ok(())
    }

    /// Return the actual compiler input when preprocessing is a separate step.
    pub(crate) fn compile_input(&self, object: &[u8]) -> Option<&BString> {
        self.units.get(object)?.preprocessed.as_ref()
    }

    /// Add the Fortran flags for an original or preprocessed source.
    ///
    /// Pass the returned group to the normal compiler lowering. Use
    /// [Self::compile_input] for the compiler's source placeholder.
    pub(crate) fn compile_group(
        &self,
        model: &ProjectModel,
        target: &Target,
        unit: &CompileUnit,
        group: &CompileGroup,
    ) -> CompileGroup {
        let Some(source) = self.units.get(&unit.object) else {
            return group.clone();
        };
        self.source_group(model, target, source, group, source.preprocessed.is_some())
    }

    /// Set Fortran graph fields on a normal compile step.
    ///
    /// The compiler invocation must already use [Self::compile_input] if
    /// present. The source identity and the compiler depfile stay intact.
    pub(crate) fn bind_compile(&self, step: &mut BuildStep) {
        if let BuildStep::Compile {
            object,
            compile_input,
            dyndep,
            ..
        } = step
            && let Some(source) = self.units.get(object)
        {
            compile_input.clone_from(&source.preprocessed);
            *dyndep = self.ninja.then(|| self.info.dyndep.clone());
        }
    }

    /// Lower the Ninja scans and collation. Other generators get an empty list.
    ///
    /// # Errors
    ///
    /// Returns an error if a required preprocessing rule is empty or a
    /// compile group cannot supply its flags.
    pub(crate) fn scan_steps(
        &self,
        model: &ProjectModel,
        target: &Target,
        resolved: &ResolvedTarget,
        position_independent: Option<bool>,
    ) -> Result<Vec<BuildStep>, ResolveError> {
        if !self.ninja {
            return Ok(Vec::new());
        }
        let get = |key: &[u8]| definition(model, target.dir, key);
        let cmake = get(b"CMAKE_COMMAND").map_or(b"cmake".as_slice(), |value| value.as_slice());
        let mut steps = Vec::with_capacity(self.units.len() + 1);
        let mut ddis = Vec::with_capacity(self.units.len());
        for unit in &resolved.compile_units {
            let Some(source) = self.units.get(&unit.object) else {
                continue;
            };
            let ddi = suffix(&source.object, b".ddi");
            let scan_output = source.preprocessed.as_ref().unwrap_or(&ddi);
            let depfile = suffix(scan_output, b".d");
            let invocation = scanner(cmake, &self.info.depend_info, source, &ddi, &depfile);
            let invocation = if let Some(output) = &source.preprocessed {
                let group = resolved.compile_groups.get(unit.group).ok_or_else(|| {
                    ResolveError::Message("The Fortran scan has no compile group.".into())
                })?;
                let group = self.source_group(model, target, source, group, false);
                let compiler = Preprocessor {
                    model,
                    target,
                    config: &resolved.config,
                    position_independent,
                };
                let preprocess = compiler.commands(&group, source, output, &depfile)?;
                command(chain_commands(preprocess.into_iter().chain([invocation])))
            } else {
                invocation
            };
            steps.push(BuildStep::Scan {
                lang: "Fortran".into(),
                source: source.source.clone(),
                ddi: ddi.clone(),
                object: source.object.clone(),
                preprocessed: source.preprocessed.clone(),
                invocation,
                depfile: Some(depfile),
            });
            ddis.push(ddi);
        }
        steps.push(BuildStep::Collate {
            lang: "Fortran".into(),
            target: target.name.clone(),
            ddis,
            dd: self.info.dyndep.clone(),
            modmaps: Vec::new(),
            tdi: self.info.depend_info.clone(),
            depend_info: Box::new(self.depend_info.clone()),
            modmap_format: None,
            implicit_inputs: std::iter::once(self.info.depend_info.clone())
                .chain(
                    self.info
                        .linked_target_dirs
                        .iter()
                        .map(|dir| join(dir, b"FortranModules.json")),
                )
                .collect(),
        });
        Ok(steps)
    }

    fn source_group(
        &self,
        model: &ProjectModel,
        target: &Target,
        source: &DependSource,
        group: &CompileGroup,
        postprocess: bool,
    ) -> CompileGroup {
        let mut group = group.clone();
        let get = |key: &[u8]| definition(model, target.dir, key);
        let mut flags = Vec::new();
        let output_flag = get(b"CMAKE_Fortran_MODOUT_FLAG");
        let directory_flag = get(b"CMAKE_Fortran_MODDIR_FLAG").map(|value| value.as_slice());
        // Some toolchain files put the directory flag in MODOUT_FLAG.
        let directory_flag = directory_flag.or_else(|| {
            output_flag.map(|value| value.as_slice()).filter(|flag| {
                matches!(
                    flag.trim_ascii_end(),
                    b"-J" | b"-module" | b"-moddir" | b"-mdir"
                )
            })
        });
        if let Some(flag) = output_flag
            && Some(flag.as_slice()) != directory_flag
        {
            flags.extend(list_flags(flag));
        }
        if let Some(directory) = &self.info.module_dir {
            let relative = super::build_relative(model, directory);
            let directory = if relative.is_empty() {
                b".".as_slice()
            } else {
                &relative
            };
            if let Some(flag) = directory_flag {
                flags.extend(path_flag(flag, directory));
            }
            if let Some(flag) = get(b"CMAKE_Fortran_MODDIR_INCLUDE_FLAG") {
                flags.extend(path_flag(flag, directory));
            }
        }
        if let Some(flag) = get(b"CMAKE_Fortran_MODPATH_FLAG") {
            for include in &group.includes {
                flags.extend(path_flag(flag, &include.path.value));
            }
        }
        let format = source_or_target(model, target, &source.source, b"Fortran_FORMAT");
        if let Some(key) = match format.map(|value| value.as_slice()) {
            Some(value) if value.eq_ignore_ascii_case(b"fixed") => {
                Some(b"CMAKE_Fortran_FORMAT_FIXED_FLAG".as_slice())
            }
            Some(value) if value.eq_ignore_ascii_case(b"free") => {
                Some(b"CMAKE_Fortran_FORMAT_FREE_FLAG".as_slice())
            }
            _ => None,
        } && let Some(value) = get(key)
        {
            flags.extend(list_flags(value));
        }
        if postprocess {
            if let Some(flag) = get(b"CMAKE_Fortran_POSTPROCESS_FLAG") {
                flags.extend(list_flags(flag));
            }
            if !get(b"CMAKE_Fortran_COMPILE_WITH_DEFINES").is_some_and(|value| truth::is_on(value))
            {
                group.defines.clear();
            }
            let original_dir = Lexical::new(&source.source).parent();
            if !original_dir.is_empty()
                && !group
                    .includes
                    .iter()
                    .any(|include| include.path.value == original_dir)
            {
                group.includes.insert(
                    0,
                    crate::InterfaceInclude {
                        path: BT::new(original_dir.into(), target.backtrace.clone()),
                        system: false,
                    },
                );
            }
        } else if let Some(enabled) = preprocess(model, target, &source.source) {
            let key: &[u8] = if enabled {
                b"CMAKE_Fortran_COMPILE_OPTIONS_PREPROCESS_ON"
            } else {
                b"CMAKE_Fortran_COMPILE_OPTIONS_PREPROCESS_OFF"
            };
            if let Some(options) = get(key) {
                flags.extend(list_flags(options));
            }
        }
        group.flags.extend(
            flags
                .into_iter()
                .map(|flag| BT::new(flag, target.backtrace.clone())),
        );
        group
    }
}

/// Collect Fortran dependency data for a Make target that has resolved module facts.
///
/// The Make generator uses the same toolchain and source data as Ninja. It
/// writes a DependInfo.cmake file and scans the original source at build time.
///
/// # Errors
///
/// Returns an error if the model is missing a target or a compile group.
pub fn fortran_make_depend_info(
    model: &ProjectModel,
    resolved: &ResolvedTarget,
) -> Result<Option<DependInfo>, ResolveError> {
    let Some(info) = &resolved.fortran else {
        return Ok(None);
    };
    let Some(plan) = FortranPlan::new(model, resolved, [])? else {
        return Ok(None);
    };
    let mut depend_info = plan.depend_info;
    if let DependInfo::Fortran(tdi) = &mut depend_info {
        tdi.module_dir.clone_from(&info.module_dir);
        tdi.linked_target_dirs.clone_from(&info.linked_target_dirs);
    }
    Ok(Some(depend_info))
}

fn support_dir(model: &ProjectModel, target: &Target, resolved: &ResolvedTarget) -> BString {
    resolved
        .artifacts
        .object_dir
        .clone()
        .unwrap_or_else(|| object_dir(model, target, &resolved.config))
}

fn source_or_target<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
    source: &[u8],
    key: &[u8],
) -> Option<&'a BString> {
    model
        .dir(target.dir)
        .source_props
        .get(&Lexical::new(source).normalize())
        .and_then(|props| props.get(key))
        .filter(|value| !value.is_empty())
        .or_else(|| target_property(model, target, key))
}

fn preprocess(model: &ProjectModel, target: &Target, source: &[u8]) -> Option<bool> {
    source_or_target(model, target, source, b"Fortran_PREPROCESS")
        .filter(|value| !value.is_empty())
        .map(|value| truth::is_on(value))
}

fn preprocessed_path(
    model: &ProjectModel,
    target: &Target,
    source: &[u8],
    object: &[u8],
) -> BString {
    let (_, source_ext) = Lexical::new(source).stem_extension(true);
    let compiler_ext = definition(model, target.dir, b"CMAKE_Fortran_OUTPUT_EXTENSION")
        .filter(|ext| !ext.is_empty());
    let stem = compiler_ext
        .and_then(|ext| object.strip_suffix(ext.as_slice()))
        .or_else(|| object.strip_suffix(b".o"))
        .or_else(|| object.strip_suffix(b".obj"))
        .unwrap_or(object);
    let extension = source_ext.to_ascii_lowercase();
    let extension: &[u8] = match extension.as_slice() {
        b".fpp" => b".f",
        b"" => b".f90",
        _ => &extension,
    };
    [stem, b"-pp", extension].concat().into()
}

fn suffix(value: &[u8], ending: &[u8]) -> BString {
    [value, ending].concat().into()
}

fn list_flags(value: &[u8]) -> Vec<BString> {
    list::split(value, list::Empty::Drop)
}

fn path_flag(flag: &[u8], directory: &[u8]) -> Vec<BString> {
    let prefix = flag.trim_ascii_end();
    if prefix.is_empty() {
        return Vec::new();
    }
    if prefix.len() != flag.len() || matches!(prefix, b"-module" | b"-moddir" | b"-mdir") {
        vec![prefix.into(), directory.into()]
    } else {
        vec![[prefix, directory].concat().into()]
    }
}

fn scanner(
    cmake: &[u8],
    tdi: &[u8],
    source: &DependSource,
    ddi: &[u8],
    depfile: &[u8],
) -> crate::Invocation {
    let input = source.preprocessed.as_ref().unwrap_or(&source.source);
    let output = source
        .preprocessed
        .as_ref()
        .map_or(ddi, |value| value.as_slice());
    let mut args = vec![
        cmake.into(),
        "-E".into(),
        "cmake_ninja_depends".into(),
        "--lang=Fortran".into(),
    ];
    for (flag, value) in [
        (b"--tdi=".as_slice(), tdi),
        (b"--src=", input.as_slice()),
        (b"--out=", output),
        (b"--dep=", depfile),
        (b"--obj=", source.object.as_slice()),
        (b"--ddi=", ddi),
    ] {
        args.push([flag, value].concat().into());
    }
    if source.preprocessed.is_some() {
        args.push([b"--src-orig=".as_slice(), &source.source].concat().into());
    }
    direct(args, None)
}

struct Preprocessor<'a> {
    model: &'a ProjectModel,
    target: &'a Target,
    config: &'a ConfigId,
    position_independent: Option<bool>,
}

impl Preprocessor<'_> {
    fn commands(
        &self,
        group: &CompileGroup,
        source: &DependSource,
        output: &[u8],
        depfile: &[u8],
    ) -> Result<Vec<crate::Invocation>, ResolveError> {
        let template = definition(
            self.model,
            self.target.dir,
            b"CMAKE_Fortran_PREPROCESS_SOURCE",
        )
        .ok_or_else(|| {
            ResolveError::Message("The Fortran preprocessing rule is missing.".into())
        })?;
        let (flags, defines, includes) = compile_arguments(
            self.model,
            self.target,
            self.config,
            group,
            None,
            &source.source,
            self.position_independent,
        )?;
        let flags = join_words(flags);
        let commands = rule_invocations(self.model, self.target.dir, template, |key| {
            Some(match key {
                b"FLAGS" => flags.clone(),
                b"DEFINES" => defines.clone(),
                b"INCLUDES" => includes.clone(),
                b"SOURCE" => quote(&source.source),
                b"PREPROCESSED_SOURCE" => quote(output),
                b"DEP_FILE" => quote(depfile),
                b"DEP_TARGET" => quote(output),
                b"OBJECT" => quote(&source.object),
                b"OBJECT_DIR" | b"OBJECT_FILE_DIR" => quote(Lexical::new(&source.object).parent()),
                b"CONFIG" => quote(&self.config.0),
                _ => return None,
            })
        });
        if commands.is_empty() {
            return Err(ResolveError::Message(
                "The Fortran preprocessing rule has no commands.".into(),
            ));
        }
        Ok(commands)
    }
}

fn chain_commands(invocations: impl IntoIterator<Item = crate::Invocation>) -> BString {
    let mut shell = BString::default();
    for invocation in invocations {
        if !shell.is_empty() {
            shell.extend_from_slice(b" && ");
        }
        shell.push(b'(');
        shell.extend_from_slice(&invocation.shell);
        shell.push(b')');
    }
    shell
}

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_model::{TargetKind, TargetOrigin};

    use super::*;

    fn resolved(
        model: &mut ProjectModel,
        name: &str,
    ) -> Result<ResolvedTarget, rcmake_model::ModelError> {
        let target = Target::new(
            name.into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        );
        let id = model.add_target(target)?;
        let directory = format!("CMakeFiles/{name}.dir");
        Ok(ResolvedTarget {
            id,
            config: ConfigId::default(),
            metadata: crate::TargetMetadata {
                name: name.into(),
                kind: TargetKind::StaticLibrary,
                source_dir: "/src".into(),
                binary_dir: "/build".into(),
                backtrace: Backtrace::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: crate::ArtifactSet {
                object_dir: Some(directory.clone().into()),
                ..crate::ArtifactSet::default()
            },
            compile_groups: vec![CompileGroup {
                language: "Fortran".into(),
                units: vec![0],
                ..CompileGroup::default()
            }],
            compile_units: vec![CompileUnit {
                source: BT::new(format!("/src/{name}.F90").into(), Backtrace::default()),
                object: format!("{directory}/{name}.F90.o").into(),
                ..CompileUnit::default()
            }],
            link: None,
            dependencies: Vec::new(),
            cxx_modules: None,
            fortran: None,
            ide: None,
        })
    }

    #[test]
    fn linked_modules_and_preprocessed_input_enter_the_graph()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = ProjectModel::default();
        model.generator = Some(GeneratorKind::Ninja);
        model.set_root_dirs("/src".into(), "/build".into());
        model.dir_mut(DirId::ROOT).vars.extend([
            ("CMAKE_Fortran_COMPILER".into(), "gfortran".into()),
            ("CMAKE_Fortran_COMPILER_ID".into(), "GNU".into()),
            (
                "CMAKE_Fortran_MODULE_DIRECTORY".into(),
                "module files".into(),
            ),
            ("CMAKE_Fortran_MODULE_CASE".into(), "UPPER".into()),
            ("CMAKE_Fortran_MODOUT_FLAG".into(), "-J".into()),
            (
                "CMAKE_Fortran_PREPROCESS_SOURCE".into(),
                "<CMAKE_Fortran_COMPILER> <FLAGS> -E <SOURCE> -o <PREPROCESSED_SOURCE>".into(),
            ),
        ]);
        let provider = resolved(&mut model, "provider")?;
        let consumer = resolved(&mut model, "consumer")?;
        let target = model.target(consumer.id).ok_or("Missing consumer target")?;
        let plan =
            FortranPlan::new(&model, &consumer, [&provider])?.ok_or("Missing Fortran plan")?;
        let steps = plan.scan_steps(&model, target, &consumer, None)?;
        let [
            BuildStep::Scan {
                preprocessed: Some(pp),
                invocation,
                ..
            },
            BuildStep::Collate {
                depend_info,
                implicit_inputs,
                modmap_format: None,
                ..
            },
        ] = steps.as_slice()
        else {
            return Err("Expected a Fortran scan and collation".into());
        };
        assert_eq!(pp, "CMakeFiles/consumer.dir/consumer.F90-pp.f90");
        assert!(
            invocation
                .shell
                .windows(b"'-Jmodule files'".len())
                .any(|bytes| bytes == b"'-Jmodule files'")
        );
        assert_eq!(
            implicit_inputs,
            &[
                "CMakeFiles/consumer.dir/FortranDependInfo.json",
                "/build/CMakeFiles/provider.dir/FortranModules.json",
            ]
        );
        let DependInfo::Fortran(tdi) = depend_info.as_ref() else {
            return Err("Expected Fortran dependency information".into());
        };
        assert_eq!(tdi.module_case, ModuleCase::Upper);
        assert_eq!(tdi.submodule_sep, "@");
        assert_eq!(tdi.submodule_ext, ".smod");
        let mut compile = BuildStep::Compile {
            source: "/src/consumer.F90".into(),
            object: "CMakeFiles/consumer.dir/consumer.F90.o".into(),
            compile_input: None,
            invocation: crate::Invocation::default(),
            depfile: Some("compile.d".into()),
            modmap: None,
            dyndep: None,
        };
        plan.bind_compile(&mut compile);
        assert!(matches!(compile, BuildStep::Compile {
            source, compile_input: Some(input), depfile: Some(depfile), dyndep: Some(dd), ..
        } if source == "/src/consumer.F90" && &input == pp && depfile == "compile.d" && dd == plan.info.dyndep));
        Ok(())
    }

    #[test]
    fn scanner_uses_the_original_source_only_for_location_context() {
        let source = DependSource {
            object: "obj/a.f90.o".into(),
            source: "/src/a.F90".into(),
            preprocessed: Some("obj/a.F90-pp.f90".into()),
        };
        let command = scanner(
            "/my tools/cmake".as_bytes(),
            b"obj/FortranDependInfo.json",
            &source,
            b"obj/a.f90.o.ddi",
            b"obj/a.F90-pp.f90.d",
        );
        assert_eq!(
            command.argv,
            [
                "/my tools/cmake",
                "-E",
                "cmake_ninja_depends",
                "--lang=Fortran",
                "--tdi=obj/FortranDependInfo.json",
                "--src=obj/a.F90-pp.f90",
                "--out=obj/a.F90-pp.f90",
                "--dep=obj/a.F90-pp.f90.d",
                "--obj=obj/a.f90.o",
                "--ddi=obj/a.f90.o.ddi",
                "--src-orig=/src/a.F90",
            ]
        );
        let source = DependSource {
            preprocessed: None,
            ..source
        };
        let command = scanner(
            b"cmake",
            b"obj/FortranDependInfo.json",
            &source,
            b"a.ddi",
            b"a.ddi.d",
        );
        assert!(command.argv.iter().any(|arg| arg == "--src=/src/a.F90"));
        assert!(command.argv.iter().any(|arg| arg == "--out=a.ddi"));
        assert!(
            !command
                .argv
                .iter()
                .any(|arg| arg.starts_with(b"--src-orig="))
        );
    }

    #[test]
    fn module_flags_keep_directory_arguments_intact() {
        assert_eq!(path_flag(b"-J", b"module files"), ["-Jmodule files"]);
        assert_eq!(
            path_flag(b"-module ", b"module files"),
            ["-module", "module files"]
        );
        assert_eq!(
            path_flag(b"-moddir", b"module files"),
            ["-moddir", "module files"]
        );
    }
}
