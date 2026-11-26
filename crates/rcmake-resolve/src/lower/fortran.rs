// SPDX-License-Identifier: MIT OR Apache-2.0
//! Lower Fortran module dependencies and prepare compiler inputs.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{
    BString, list,
    path_lex::{Lexical, cmake_style},
    shell::quote,
    truth,
};
use rcmake_fortran::{DependSource, FortranBuildTdi, FortranCompiler, FortranTdi, SourceForm};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path};
use rcmake_model::{BT, DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};

use super::compile::compile_arguments;
use super::shell::{command, direct, join_words, rule_invocations};
use super::{definition, join, object_dir};
use crate::model::{DependInfo, FortranTargetInfo};
use crate::sources::{SourcePaths, TargetInputs};
use crate::usage::{ResolvedLinkItem, ResolvedLinkKind};
use crate::{BuildStep, CompileGroup, CompileUnit, Invocation, ResolveError, ResolvedTarget};

mod depend_info;
pub use depend_info::{fortran_depend_info_json, fortran_make_depend_info};

/// Fortran inputs for one target and configuration.
pub(crate) struct FortranPlan {
    /// Build paths to record on the resolved target.
    pub(crate) info: FortranTargetInfo,
    tdi: FortranBuildTdi,
    flag_module_dir: Option<BString>,
    units: IndexMap<BString, DependSource>,
    ninja: bool,
}

impl FortranPlan {
    /// Plan scans for local units and include module summaries from linked targets.
    /// Use link order for `linked` and final build order for `target_order`.
    /// Put object owners in `forwarded` when this target republishes their modules.
    ///
    /// # Errors
    ///
    /// Returns an error for missing target facts or a required preprocessing rule.
    pub(crate) fn new<'a>(
        model: &ProjectModel,
        resolved: &ResolvedTarget,
        linked: impl IntoIterator<Item = &'a ResolvedTarget>,
        forwarded: &IndexSet<TargetId>,
        target_order: &IndexSet<TargetId>,
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
        let mut source_formats = BTreeMap::new();
        let mut defines = None;
        for unit in &resolved.compile_units {
            let group = resolved.compile_groups.get(unit.group).ok_or_else(|| {
                ResolveError::Message("The compile group for a source is missing.".into())
            })?;
            if group.language != "Fortran" {
                continue;
            }
            let source = join(&resolved.metadata.source_dir, &unit.source.value);
            if let Some(form) = source_form(model, target, &source) {
                source_formats.insert(source.clone(), form);
            }
            defines.get_or_insert_with(|| {
                group
                    .defines
                    .iter()
                    .take(group.source_define_start.unwrap_or(group.defines.len()))
                    .map(|define| define.value.clone())
                    .collect()
            });
            let preprocessed = if ninja && preprocess(model, target, &source) != Some(false) {
                if get(b"CMAKE_Fortran_PREPROCESS_SOURCE")
                    .is_none_or(|rule| rule.trim_ascii().is_empty())
                {
                    return Err(ResolveError::Message(
                        format!("The Fortran preprocessing rule for {source} is missing.").into(),
                    ));
                }
                Some(preprocessed_path(model, target, &source, &unit.object))
            } else {
                None
            };
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
        let mut linked_target_dirs = IndexSet::new();
        let mut forward_target_dirs = IndexSet::new();
        let position = target_order.get_index_of(&resolved.id);
        for other in linked {
            let Some(provider) = model.target(other.id) else {
                continue;
            };
            if provider.is_imported()
                || provider.kind == TargetKind::InterfaceLibrary
                || other.config != resolved.config
                || !target_order
                    .get_index_of(&other.id)
                    .zip(position)
                    .is_some_and(|(dep, current)| dep < current)
                || !other.compile_units.iter().any(|unit| {
                    other
                        .compile_groups
                        .get(unit.group)
                        .is_some_and(|group| group.language == "Fortran")
                })
            {
                continue;
            }
            let dir = join(top, &support_dir(model, provider, other));
            if forwarded.contains(&other.id) {
                forward_target_dirs.insert(dir.clone());
            }
            linked_target_dirs.insert(dir);
        }
        if units.is_empty() && forward_target_dirs.is_empty() {
            return Ok(None);
        }
        let dir = support_dir(model, target, resolved);
        let flag_module_dir = get(b"CMAKE_Fortran_MODDIR_FLAG")
            .filter(|flag| !flag.trim_ascii().is_empty())
            .and_then(|_| {
                let directory = match target.props.get(b"Fortran_MODULE_DIRECTORY") {
                    Some(value) if value.is_empty() => None,
                    Some(value) => Some(join(current, value)),
                    None if current != top => Some(current.clone()),
                    None => None,
                };
                directory.or_else(|| {
                    get(b"CMAKE_Fortran_MODDIR_DEFAULT")
                        .filter(|value| !value.is_empty())
                        .map(|value| join(top, value))
                })
            })
            .map(|dir| Lexical::new(&dir).normalize());
        let module_dir = Some(flag_module_dir.as_ref().unwrap_or(top).clone());
        let compiler_id = get(b"CMAKE_Fortran_COMPILER_ID")
            .cloned()
            .unwrap_or_default();
        let compiler = FortranCompiler::from_id(&compiler_id);
        let linked_target_dirs: Vec<_> = linked_target_dirs.into_iter().collect();
        let building_intrinsic_modules = target
            .props
            .get(b"Fortran_BUILDING_INTRINSIC_MODULES")
            .or_else(|| target.props.get(b"Fortran_BUILDING_INSTRINSIC_MODULES"))
            .is_some_and(|value| truth::is_on(value));
        let scan = FortranTdi {
            dir_cur_bld: current.clone(),
            dir_top_bld: if ninja {
                ninja_dyndep_root(model, top)
            } else {
                top.clone()
            },
            include_dirs: includes.into_iter().collect(),
            module_dir: module_dir.clone(),
            linked_target_dirs: linked_target_dirs.clone(),
            submodule_sep: get(b"CMAKE_Fortran_SUBMODULE_SEP")
                .cloned()
                .unwrap_or_else(|| compiler.submodule_sep.into()),
            submodule_ext: get(b"CMAKE_Fortran_SUBMODULE_EXT")
                .cloned()
                .unwrap_or_else(|| compiler.submodule_ext.into()),
            module_case: compiler.module_case,
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
                make_depend_info: None,
            },
            tdi: FortranBuildTdi {
                scan,
                dir_cur_src: resolved.metadata.source_dir.clone(),
                dir_top_src: model.dir(DirId::ROOT).source_dir.clone(),
                forward_modules_from_target_dirs: forward_target_dirs.into_iter().collect(),
                building_intrinsic_modules,
                source_formats,
                defines: defines.unwrap_or_default(),
            },
            flag_module_dir,
            units,
            ninja,
        }))
    }

    /// Record the metadata needed to generate a Make dependency pass.
    pub(crate) fn set_make_depend_info(&mut self) {
        self.info.make_depend_info = Some(Box::new(DependInfo::Fortran(self.tdi.clone())));
    }

    /// Create the compiler's module output directory through the host.
    ///
    /// # Errors
    ///
    /// Returns an error if the host cannot create the directory.
    pub(crate) fn create_module_dir(&self, host: &dyn Host) -> Result<(), ResolveError> {
        if let Some(directory) = &self.flag_module_dir {
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
        host: Option<&dyn Host>,
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
            let object_node = ninja_node(model, &source.object);
            let scan_output_node = ninja_node(model, scan_output);
            let invocation = scanner(
                cmake,
                &self.info.depend_info,
                source,
                &object_node,
                &scan_output_node,
                &ddi,
                &depfile,
            );
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
                chain_commands(preprocess.into_iter().chain([invocation]), host)?
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
            depend_info: Box::new(DependInfo::Fortran(self.tdi.clone())),
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
        if let Some(flag) = get(b"CMAKE_Fortran_MODOUT_FLAG") {
            flags.extend(list_flags(flag));
        }
        if let Some(directory) = &self.flag_module_dir {
            let relative = super::build_relative(model, directory);
            let relative = if relative.is_empty() {
                b".".as_slice()
            } else {
                &relative
            };
            if let Some(flag) = get(b"CMAKE_Fortran_MODDIR_FLAG") {
                flags.extend(path_flag(flag, relative));
            }
            if let Some(flag) = get(b"CMAKE_Fortran_MODDIR_INCLUDE_FLAG") {
                flags.extend(path_flag(flag, relative));
            }
        }
        if let Some(flag) = get(b"CMAKE_Fortran_MODPATH_FLAG") {
            for include in &group.includes {
                flags.extend(path_flag(flag, &include.path.value));
            }
        }
        let format = match self.tdi.source_formats.get(&source.source) {
            Some(SourceForm::Fixed) => get(b"CMAKE_Fortran_FORMAT_FIXED_FLAG"),
            Some(SourceForm::Free) => get(b"CMAKE_Fortran_FORMAT_FREE_FLAG"),
            None => None,
        };
        if let Some(value) = format {
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
            if !original_dir.is_empty() {
                let system = group
                    .includes
                    .iter()
                    .any(|include| include.path.value == original_dir && include.system);
                group.includes.insert(
                    0,
                    crate::InterfaceInclude {
                        path: BT::new(original_dir.into(), target.backtrace.clone()),
                        system,
                    },
                );
            }
        } else if let Some(enabled) = preprocess(model, target, &source.source)
            && (!enabled || !self.ninja)
        {
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

/// Find the owners of object files included through source or link expressions.
pub(crate) fn forwarded_object_libraries(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    inputs: &TargetInputs,
    links: &[ResolvedLinkItem],
) -> IndexSet<TargetId> {
    fn object_files(items: &[ResolvedLinkItem]) -> impl Iterator<Item = &[u8]> {
        items
            .iter()
            .flat_map(|item| match &item.kind {
                ResolvedLinkKind::Group { members, .. } => members.as_slice(),
                _ => std::slice::from_ref(item),
            })
            .filter(|item| item.kind == ResolvedLinkKind::ObjectFile)
            .map(|item| item.text.value.as_slice())
    }

    inputs
        .objects
        .iter()
        .map(|object| object.as_slice())
        .chain(object_files(links))
        .filter_map(|object| paths.object_owner(&join(&model.dir(DirId::ROOT).binary_dir, object)))
        .collect()
}

fn ninja_prefix(model: &ProjectModel) -> BString {
    model
        .definition(DirId::ROOT, b"CMAKE_NINJA_OUTPUT_PATH_PREFIX")
        .map_or_else(BString::default, |prefix| {
            Lexical::new(&cmake_style(prefix)).normalize()
        })
}

fn ninja_node(model: &ProjectModel, object: &[u8]) -> BString {
    let object = super::build_relative(model, &cmake_style(object));
    let prefix = ninja_prefix(model);
    if prefix.is_empty()
        || Lexical::new(&object).absolute()
        || Lexical::new(&prefix).prefix(Lexical::new(&object))
    {
        object
    } else {
        join(&prefix, &object)
    }
}

// Relative Fortran graph paths are read from the physical build
// directory. The dyndep module paths are relative to the parent manifest.
fn ninja_dyndep_root(model: &ProjectModel, top: &BString) -> BString {
    let prefix = ninja_prefix(model);
    if prefix.is_empty() {
        return top.clone();
    }
    if Lexical::new(&prefix).absolute() {
        return BString::default();
    }
    let mut root = top.as_slice();
    for component in Lexical::new(&prefix)
        .components()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let path = Lexical::new(root);
        if path.filename() != component {
            return top.clone();
        }
        root = path.parent();
    }
    root.into()
}

fn support_dir(model: &ProjectModel, target: &Target, resolved: &ResolvedTarget) -> BString {
    resolved
        .artifacts
        .object_dir
        .clone()
        .unwrap_or_else(|| object_dir(model, target, &resolved.config))
}

fn source_property<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
    source: &[u8],
    key: &[u8],
) -> Option<&'a BString> {
    model
        .dir(target.dir)
        .source_props
        .get(&Lexical::new(source).normalize())?
        .get(key)
}

fn source_form(model: &ProjectModel, target: &Target, source: &[u8]) -> Option<SourceForm> {
    let key = b"Fortran_FORMAT";
    source_property(model, target, source, key)
        .and_then(|value| SourceForm::from_property(value))
        .or_else(|| SourceForm::from_property(target.props.get(key)?))
}

fn preprocess(model: &ProjectModel, target: &Target, source: &[u8]) -> Option<bool> {
    let key = b"Fortran_PREPROCESS";
    source_property(model, target, source, key)
        .filter(|value| !value.is_empty())
        .or_else(|| target.props.get(key))
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
    if prefix.len() != flag.len() {
        vec![prefix.into(), directory.into()]
    } else {
        vec![[prefix, directory].concat().into()]
    }
}

fn scanner(
    cmake: &[u8],
    tdi: &[u8],
    source: &DependSource,
    object_node: &[u8],
    scan_output_node: &[u8],
    ddi: &[u8],
    depfile: &[u8],
) -> crate::Invocation {
    let input = source.preprocessed.as_ref().unwrap_or(&source.source);
    let mut args = vec![
        cmake.into(),
        "-E".into(),
        "cmake_ninja_depends".into(),
        "--lang=Fortran".into(),
    ];
    for (flag, value) in [
        (b"--tdi=".as_slice(), tdi),
        (b"--src=", input.as_slice()),
        (b"--out=", scan_output_node),
        (b"--dep=", depfile),
        (b"--obj=", object_node),
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

fn chain_commands(
    invocations: impl IntoIterator<Item = Invocation>,
    host: Option<&dyn Host>,
) -> Result<Invocation, ResolveError> {
    let windows = host.map_or(cfg!(windows), |host| host.system().sysname == b"Windows");
    let mut shell = BString::default();
    for invocation in invocations {
        if !shell.is_empty() {
            shell.extend_from_slice(b" && ");
        }
        if windows {
            if invocation.argv.starts_with(&["sh".into(), "-c".into()]) {
                return Err(ResolveError::Message(
                    "The Fortran preprocessing rule requires a POSIX shell.".into(),
                ));
            }
            let words = invocation.argv.iter().map(|word| windows_argument(word));
            shell.extend_from_slice(&join_words(words));
        } else {
            shell.push(b'(');
            shell.extend_from_slice(&invocation.shell);
            shell.push(b')');
        }
    }
    if windows {
        let argv = vec![
            host.and_then(|host| host.env_get(b"COMSPEC"))
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "cmd.exe".into()),
            "/D".into(),
            "/S".into(),
            "/V:OFF".into(),
            "/C".into(),
            shell.clone(),
        ];
        Ok(Invocation {
            argv,
            shell,
            working_dir: None,
            link_response: None,
        })
    } else {
        Ok(command(shell))
    }
}

fn windows_argument(word: &[u8]) -> BString {
    if !word.is_empty()
        && word
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_+-./:=@\\".contains(b))
    {
        return word.into();
    }
    let mut quoted = Vec::new();
    rcmake_base::shell::windows_argument(word, &mut quoted, true);
    quoted.into()
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
            ("CMAKE_Fortran_MODDIR_FLAG".into(), "-J".into()),
            (
                "CMAKE_Fortran_PREPROCESS_SOURCE".into(),
                "<CMAKE_Fortran_COMPILER> <FLAGS> -E <SOURCE> -o <PREPROCESSED_SOURCE>".into(),
            ),
        ]);
        let provider = resolved(&mut model, "provider")?;
        let consumer = resolved(&mut model, "consumer")?;
        model
            .target_mut(consumer.id)
            .ok_or("Missing consumer target")?
            .props
            .set("Fortran_MODULE_DIRECTORY".into(), "module files".into());
        let target = model.target(consumer.id).ok_or("Missing consumer target")?;
        let forwarded = IndexSet::from([provider.id]);
        let order = IndexSet::from([provider.id, consumer.id]);
        let plan = FortranPlan::new(&model, &consumer, [&provider], &forwarded, &order)?
            .ok_or("Missing Fortran plan")?;
        assert_eq!(
            plan.tdi.forward_modules_from_target_dirs,
            ["/build/CMakeFiles/provider.dir"]
        );
        let steps = plan.scan_steps(&model, target, &consumer, None, None)?;
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
        let unit = consumer.compile_units.first().ok_or("Missing source")?;
        let mut group = consumer
            .compile_groups
            .first()
            .ok_or("Missing compile group")?
            .clone();
        group.includes = [("/other", false), ("/src", true)]
            .map(|(path, system)| crate::InterfaceInclude {
                path: BT::new(path.into(), Backtrace::default()),
                system,
            })
            .into();
        let compiled = plan.compile_group(&model, target, unit, &group);
        assert_eq!(
            compiled
                .includes
                .first()
                .map(|include| (&include.path.value, include.system)),
            Some((&BString::from("/src"), true))
        );
        assert_eq!(
            group.includes.first().map(|include| &include.path.value),
            Some(&BString::from("/other"))
        );
        assert_eq!(pp, "CMakeFiles/consumer.dir/consumer.F90-pp.f90");
        assert!(invocation.shell.to_string().contains("-Jmodule files"));
        assert_eq!(
            implicit_inputs,
            &[
                "CMakeFiles/consumer.dir/FortranDependInfo.json",
                "/build/CMakeFiles/provider.dir/FortranModules.json",
            ]
        );
        assert!(matches!(depend_info.as_ref(), DependInfo::Fortran(tdi)
            if tdi.scan.submodule_sep == "@" && tdi.scan.submodule_ext == ".smod"));
        Ok(())
    }
}
