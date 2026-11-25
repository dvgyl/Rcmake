//! Configure-time planning for the Qt moc, uic, and rcc tools.
#![forbid(unsafe_code)]

mod checksum;
mod graph;
mod info;
mod scan;

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::Diagnostic;
use rcmake_genex::ConfigId;
use rcmake_host::Host;
use rcmake_model::{GeneratorKind, ProjectModel, TargetId};

use self::graph::{concatenate, named};
use self::scan::{ConfigValues, QtVersion, Scan, ToolPlan};
use crate::lower::join;

use crate::sources::SourcePaths;
use crate::{ResolveError, ResolvedTarget};

/// A resolved target source before unity or other source overlays.
#[derive(Clone, Debug)]
pub(crate) struct SourceInput {
    /// Absolute source path.
    pub(crate) path: BString,
    /// Whether the source is a member of a CXX_MODULES file set.
    pub(crate) cxx_module: bool,
    /// Whether a custom command or source property marks the file as generated.
    pub(crate) generated: bool,
}

/// Evaluated source and link information for one target and configuration.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConfiguredSources {
    /// Configuration that supplies the sources and links.
    pub(crate) config: ConfigId,
    /// Sources before Qt and unity overlays.
    pub(crate) sources: Vec<SourceInput>,
    /// Link targets in link order. The first Qt major version is significant.
    pub(crate) link_targets: Vec<TargetId>,
}

/// Model overlay and metadata for Qt automatic generation.
pub(crate) struct AutogenPlan {
    /// Model with Qt utility targets, custom commands, sources, and compile includes.
    pub(crate) model: ProjectModel,
    /// Author diagnostics from target selection and input scanning.
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Source manifests that cause reconfiguration when they change.
    pub(crate) reconfigure_inputs: Vec<BString>,
    /// Configure outputs that remain available after cleaning the build.
    pub(crate) info_outputs: Vec<BString>,
    /// Generated directories and files that the build tool removes during clean.
    pub(crate) clean_paths: Vec<BString>,
    targets: Vec<TargetPlan>,
}

/// Prepare Qt utilities, generated sources, and metadata from evaluated target sources.
///
/// Supply an original source inventory for each eligible target in the order of configs.
/// Populate the source path index for generated sources before collecting that inventory.
pub(crate) fn prepare(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configs: &[ConfigId],
    sources: &IndexMap<TargetId, Vec<ConfiguredSources>>,
) -> Result<Option<AutogenPlan>, ResolveError> {
    graph::prepare(model, paths, configs, sources)
}

impl AutogenPlan {
    /// Write metadata after resolving the final C++ compile groups.
    ///
    /// Each file contains all its configurations and is written only if its bytes change.
    pub(crate) fn write_info(
        &self,
        host: &dyn Host,
        resolved: &[ResolvedTarget],
    ) -> Result<(), ResolveError> {
        info::write_info(host, &self.model, &self.targets, resolved)
    }
}

struct TargetPlan {
    id: TargetId,
    version: QtVersion,
    generator: GeneratorKind,
    configs: Vec<ConfigId>,
    multi: bool,
    cross: bool,
    better: bool,
    use_stamp: bool,
    cmake: BString,
    info_dir: BString,
    build_dir: BString,
    moc: ToolPlan,
    uic: ToolPlan,
    rcc: ToolPlan,
    scan: Scan,
    resources: Vec<ResourceJob>,
    verbosity: u32,
    parallel: u32,
    command_length: Option<u32>,
}

struct ResourceJob {
    source: BString,
    stem: BString,
    checksum: BString,
    prefix: BString,
    output: BString,
    options: Vec<BString>,
    inputs: ConfigValues<Vec<BString>>,
    generated: bool,
    unique: bool,
}

impl ResourceJob {
    fn info_path(&self) -> BString {
        concatenate(&[&self.prefix, b"_Info.json"])
    }
}

impl TargetPlan {
    fn moc_or_uic(&self) -> bool {
        self.moc.enabled || self.uic.enabled
    }

    fn info_path(&self) -> BString {
        join(&self.info_dir, b"AutogenInfo.json")
    }

    fn include_dir(&self, config: Option<&[u8]>) -> BString {
        named(&join(&self.build_dir, b"include"), config, b"")
    }

    fn compilation_file(&self, config: Option<&[u8]>) -> BString {
        named(&join(&self.build_dir, b"mocs_compilation"), config, b".cpp")
    }

    fn stamp(&self, config: Option<&[u8]>) -> BString {
        named(&join(&self.build_dir, b"timestamp"), config, b"")
    }

    fn depfile(&self, config: Option<&[u8]>) -> BString {
        named(&join(&self.build_dir, b"deps"), config, b"")
    }

    fn config_pattern(&self) -> Option<&'static [u8]> {
        (self.multi && self.generator != GeneratorKind::Xcode).then_some(b"$<CONFIG>".as_slice())
    }

    fn stamp_config(&self) -> Option<&'static [u8]> {
        (self.multi && self.better).then_some(b"$<CONFIG>".as_slice())
    }

    fn tool_lines(&self, command: &str, info: &BString) -> Vec<Vec<BString>> {
        let first = || {
            vec![
                self.cmake.clone(),
                "-E".into(),
                command.into(),
                info.clone(),
            ]
        };
        if self.cross && self.better {
            let mut line = first();
            line.extend(["$<CONFIG>".into(), "$<COMMAND_CONFIG:$<CONFIG>>".into()]);
            vec![line]
        } else if self.cross || (self.multi && self.generator == GeneratorKind::Xcode) {
            self.configs
                .iter()
                .map(|config| {
                    let mut line = first();
                    line.push(config.0.clone());
                    line
                })
                .collect()
        } else {
            let mut line = first();
            let config = if self.multi {
                "$<CONFIG>".into()
            } else {
                self.configs
                    .first()
                    .map_or_else(BString::default, |config| config.0.clone())
            };
            line.push(config);
            vec![line]
        }
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use rcmake_diag::{Backtrace, PolicyId, PolicyMap, PolicyStatus};
    use rcmake_host::{Follow, PureHost};
    use rcmake_json::{ReadOptions, parse};
    use rcmake_model::{BT, CustomCommandKind, LinkItem, TargetKind, TargetOrigin};

    use super::*;
    use rcmake_host::{ReadRange, path::to_native};
    use rcmake_json::Value;
    use rcmake_model::{DirId, Target};
    use std::collections::{BTreeMap, BTreeSet};

    type Object = BTreeMap<BString, Value>;

    fn fixture(
        host: &PureHost,
        generator: GeneratorKind,
        version: &str,
        better: Option<&str>,
    ) -> Result<AutogenPlan, Box<dyn std::error::Error>> {
        fixture_with(host, generator, version, better, |_, _, _| Ok(()))
    }

    fn fixture_with(
        host: &PureHost,
        generator: GeneratorKind,
        version: &str,
        better: Option<&str>,
        configure: impl FnOnce(
            &mut ProjectModel,
            TargetId,
            &mut [ConfiguredSources],
        ) -> Result<(), Box<dyn std::error::Error>>,
    ) -> Result<AutogenPlan, Box<dyn std::error::Error>> {
        host.seed_file(&to_native(b"/src/main.cpp"), b"int main() { return 0; }")?;
        host.seed_file(&to_native(b"/src/main.h"), b"")?;
        host.seed_file(&to_native(b"/src/main_p.h"), b"")?;
        host.seed_file(&to_native(b"/src/main.hh"), b"")?;
        host.seed_file(
            &to_native(b"/src/sample.qrc"),
            b"<RCC><qresource><file>asset.txt</file><file>asset.txt</file></qresource></RCC>",
        )?;
        host.seed_file(&to_native(b"/src/asset.txt"), b"")?;
        let mut model = ProjectModel::default();
        model.generator = Some(generator);
        model.set_root_dirs("/src".into(), "/build".into());
        let root = model.dir_mut(DirId::ROOT);
        root.vars.extend([
            ("CMAKE_COMMAND".into(), "/cmake".into()),
            ("Qt6Core_VERSION_MAJOR".into(), "6".into()),
            ("Qt6Core_VERSION_MINOR".into(), version.into()),
        ]);
        root.listfiles.push("/src/CMakeLists.txt".into());
        let mut policies = PolicyMap::default();
        policies.set(PolicyId::new(100), PolicyStatus::Old)?;
        policies.set(PolicyId::new(151), PolicyStatus::New)?;
        let mut target = Target::new(
            "sample".into(),
            TargetKind::Executable,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            policies,
        );
        for (prop, value) in [
            ("AUTOMOC", "ON"),
            ("AUTOUIC", "ON"),
            ("AUTORCC", "ON"),
            ("AUTOMOC_EXECUTABLE", "/qt/moc"),
            ("AUTOUIC_EXECUTABLE", "/qt/uic"),
            ("AUTORCC_EXECUTABLE", "/qt/rcc"),
            ("AUTOGEN_PARALLEL", "2"),
        ] {
            target.props.set(prop.into(), value.into());
        }
        if let Some(better) = better {
            target
                .props
                .set("AUTOGEN_BETTER_GRAPH_MULTI_CONFIG".into(), better.into());
        }
        let id = model.add_target(target)?;
        let configs: Vec<_> = if generator.multi_config() {
            ["Debug", "Release"]
                .map(|name| ConfigId(name.into()))
                .into()
        } else {
            vec![ConfigId::default()]
        };
        let mut input: Vec<_> = configs
            .iter()
            .map(|config| ConfiguredSources {
                config: config.clone(),
                sources: ["/src/main.cpp", "/src/sample.qrc"]
                    .map(|path| SourceInput {
                        path: path.into(),
                        cxx_module: false,
                        generated: false,
                    })
                    .into(),
                link_targets: Vec::new(),
            })
            .collect();
        configure(&mut model, id, &mut input)?;
        let paths = SourcePaths::new(host);
        prepare(&model, &paths, &configs, &IndexMap::from([(id, input)]))?
            .ok_or_else(|| "Expected an AUTOGEN plan.".into())
    }

    fn read_info(host: &PureHost, name: &[u8]) -> Result<Object, Box<dyn std::error::Error>> {
        let path = join(b"/build/CMakeFiles/sample_autogen.dir", name);
        let bytes = host.read(&to_native(&path), ReadRange::All { limit: 32_000 })?;
        let Value::Object(object) = parse(&bytes, &ReadOptions::default())? else {
            return Err("Expected a JSON object.".into());
        };
        Ok(object)
    }

    #[test]
    fn autorcc_no_zstd_uses_target_qt_feature() -> Result<(), Box<dyn std::error::Error>> {
        for (feature, explicit, expected) in [
            (None, false, 0),
            (Some("ON"), false, 0),
            (Some(""), false, 1),
            (Some("OFF"), false, 1),
            (Some("OFF"), true, 1),
        ] {
            let host = PureHost::default();
            let plan = fixture_with(&host, GeneratorKind::Ninja, "8", None, |model, id, _| {
                if let Some(value) = feature {
                    model
                        .dir_mut(DirId::ROOT)
                        .vars
                        .insert("QT_FEATURE_zstd".into(), value.into());
                }
                if explicit {
                    model
                        .target_mut(id)
                        .ok_or("Missing target")?
                        .props
                        .set("AUTORCC_OPTIONS".into(), "--no-zstd".into());
                }
                Ok(())
            })?;
            plan.write_info(&host, &[])?;
            let rcc = read_info(&host, b"AutoRcc_sample_EWIEGA46WW_Info.json")?;
            let Some(Value::Array(options)) = rcc.get(b"OPTIONS".as_slice()) else {
                return Err("Missing RCC options".into());
            };
            assert_eq!(
                options
                    .iter()
                    .filter(|option| **option == Value::String("--no-zstd".into()))
                    .count(),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn metadata_matches_the_single_config_schema() -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture(&host, GeneratorKind::Ninja, "8", None)?;
        plan.write_info(&host, &[])?;
        let info = read_info(&host, b"AutogenInfo.json")?;
        let expected_keys = [
            "BUILD_DIR",
            "CMAKE_BINARY_DIR",
            "CMAKE_CURRENT_BINARY_DIR",
            "CMAKE_CURRENT_SOURCE_DIR",
            "CMAKE_EXECUTABLE",
            "CMAKE_LIST_FILES",
            "CMAKE_SOURCE_DIR",
            "CROSS_CONFIG",
            "CXX_MODULE_UNITS",
            "DEP_FILE",
            "DEP_FILE_RULE_NAME",
            "HEADERS",
            "HEADER_EXTENSIONS",
            "INCLUDE_DIR",
            "MOC_COMPILATION_FILE",
            "MOC_DEFINITIONS",
            "MOC_DEPEND_FILTERS",
            "MOC_INCLUDES",
            "MOC_MACRO_NAMES",
            "MOC_OPTIONS",
            "MOC_PATH_PREFIX",
            "MOC_PREDEFS_CMD",
            "MOC_PREDEFS_FILE",
            "MOC_RELAXED_MODE",
            "MOC_SKIP",
            "MULTI_CONFIG",
            "PARALLEL",
            "PARSE_CACHE_FILE",
            "QT_MOC_EXECUTABLE",
            "QT_UIC_EXECUTABLE",
            "QT_VERSION_MAJOR",
            "QT_VERSION_MINOR",
            "SETTINGS_FILE",
            "SOURCES",
            "UIC_OPTIONS",
            "UIC_SEARCH_PATHS",
            "UIC_SKIP",
            "UIC_UI_FILES",
            "USE_BETTER_GRAPH",
            "VERBOSITY",
        ];
        let expected: BTreeSet<_> = expected_keys.iter().map(|key| key.as_bytes()).collect();
        let mut actual: BTreeSet<_> = info.keys().map(|key| key.as_slice()).collect();
        if cfg!(windows) {
            actual.remove(b"AUTOGEN_COMMAND_LINE_LENGTH_MAX".as_slice());
        }
        assert_eq!(actual, expected);
        assert_eq!(
            info.get(b"HEADERS".as_slice()),
            Some(&Value::Array(vec![
                Value::Array(vec![
                    Value::string("/src/main.h"),
                    Value::string("MU"),
                    Value::string("EWIEGA46WW/moc_main.cpp"),
                    Value::Null
                ]),
                Value::Array(vec![
                    Value::string("/src/main_p.h"),
                    Value::string("MU"),
                    Value::string("EWIEGA46WW/moc_main_p.cpp"),
                    Value::Null
                ]),
            ]))
        );
        assert_eq!(
            info.get(b"DEP_FILE_RULE_NAME".as_slice()),
            Some(&Value::string("sample_autogen/timestamp"))
        );
        let rcc = read_info(&host, b"AutoRcc_sample_EWIEGA46WW_Info.json")?;
        let expected_rcc = [
            "BUILD_DIR",
            "CMAKE_BINARY_DIR",
            "CMAKE_CURRENT_BINARY_DIR",
            "CMAKE_CURRENT_SOURCE_DIR",
            "CMAKE_SOURCE_DIR",
            "CROSS_CONFIG",
            "GENERATOR",
            "INCLUDE_DIR",
            "INPUTS",
            "LOCK_FILE",
            "MULTI_CONFIG",
            "OPTIONS",
            "OUTPUT_CHECKSUM",
            "OUTPUT_NAME",
            "RCC_EXECUTABLE",
            "RCC_LIST_OPTIONS",
            "SETTINGS_FILE",
            "SOURCE",
            "USE_BETTER_GRAPH",
            "VERBOSITY",
        ];
        assert_eq!(
            rcc.keys()
                .map(|key| key.as_slice())
                .collect::<BTreeSet<_>>(),
            expected_rcc.iter().map(|key| key.as_bytes()).collect(),
        );
        assert_eq!(
            rcc.get(b"OUTPUT_CHECKSUM".as_slice()),
            Some(&Value::string("EWIEGA46WW"))
        );
        assert_eq!(
            rcc.get(b"INPUTS".as_slice()),
            Some(&Value::Array(vec![Value::string("/src/asset.txt"); 3]))
        );
        assert_eq!(plan.reconfigure_inputs, ["/src/sample.qrc"]);
        assert_eq!(
            info.get(b"CMAKE_LIST_FILES".as_slice()),
            Some(&Value::Array(vec![
                Value::string("/src/CMakeLists.txt"),
                Value::string("/src/sample.qrc"),
            ]))
        );
        let path = to_native(b"/build/CMakeFiles/sample_autogen.dir/AutogenInfo.json");
        let before = host.metadata(&path, Follow::Yes)?.ino;
        plan.write_info(&host, &[])?;
        assert_eq!(host.metadata(&path, Follow::Yes)?.ino, before);
        Ok(())
    }

    #[test]
    fn multi_config_graph_and_metadata_share_timestamp_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture(&host, GeneratorKind::NinjaMultiConfig, "8", None)?;
        plan.write_info(&host, &[])?;
        let info = read_info(&host, b"AutogenInfo.json")?;
        assert_eq!(info.get(b"DEP_FILE".as_slice()), Some(&Value::string("")));
        assert_eq!(
            info.get(b"DEP_FILE_Debug".as_slice()),
            Some(&Value::string("/build/sample_autogen/deps_Debug"))
        );
        assert_eq!(
            info.get(b"QT_MOC_EXECUTABLE_Debug".as_slice()),
            Some(&Value::string("/qt/moc"))
        );
        assert_eq!(
            info.get(b"MOC_COMPILATION_FILE_Release".as_slice()),
            Some(&Value::string(
                "/build/sample_autogen/mocs_compilation_Release.cpp"
            ))
        );
        let stamp = plan
            .model
            .commands()
            .map(|(_, command)| command)
            .find(|command| command.kind == CustomCommandKind::Output && command.depfile.is_some())
            .ok_or("Expected a timestamp command.")?;
        assert_eq!(
            stamp.outputs.first().map(|file| file.value.as_slice()),
            Some(b"/build/sample_autogen/timestamp_$<CONFIG>".as_slice())
        );
        assert_eq!(
            stamp.depfile.as_ref().map(|value| value.as_slice()),
            Some(b"/build/sample_autogen/deps_$<CONFIG>".as_slice())
        );
        assert_eq!(
            stamp
                .command_lines
                .last()
                .and_then(|line| line.get(2))
                .map(|arg| arg.as_slice()),
            Some(b"touch".as_slice())
        );
        let rcc = read_info(&host, b"AutoRcc_sample_EWIEGA46WW_Info.json")?;
        assert_eq!(
            rcc.get(b"OUTPUT_NAME".as_slice()),
            Some(&Value::string("qrc_sample_$<CONFIG>.cpp"))
        );
        assert_eq!(
            rcc.get(b"INPUTS_Debug".as_slice()),
            Some(&Value::Array(vec![Value::string("/src/asset.txt"); 3]))
        );

        let legacy = fixture(&host, GeneratorKind::NinjaMultiConfig, "8", Some("OFF"))?;
        legacy.write_info(&host, &[])?;
        let info = read_info(&host, b"AutogenInfo.json")?;
        assert_eq!(
            info.get(b"DEP_FILE".as_slice()),
            Some(&Value::string("/build/sample_autogen/deps"))
        );
        assert!(!info.contains_key(b"DEP_FILE_Debug".as_slice()));
        let rcc = read_info(&host, b"AutoRcc_sample_EWIEGA46WW_Info.json")?;
        assert_eq!(
            rcc.get(b"OUTPUT_NAME".as_slice()),
            Some(&Value::string("qrc_sample.cpp"))
        );
        assert!(!rcc.contains_key(b"RCC_EXECUTABLE_Debug".as_slice()));
        Ok(())
    }

    #[test]
    fn late_autogen_variables_do_not_enable_existing_targets()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture_with(&host, GeneratorKind::Ninja, "8", None, |model, _, _| {
            let earlier = Target::new(
                "earlier".into(),
                TargetKind::Executable,
                TargetOrigin::Normal,
                DirId::ROOT,
                Backtrace::default(),
                PolicyMap::default(),
            );
            model.add_target(earlier)?;
            model.dir_mut(DirId::ROOT).vars.extend([
                ("CMAKE_AUTOMOC".into(), "ON".into()),
                ("CMAKE_AUTOMOC_EXECUTABLE".into(), "/qt/moc".into()),
            ]);
            Ok(())
        })?;
        assert!(
            plan.model
                .find_target(DirId::ROOT, b"sample_autogen")
                .is_some()
        );
        assert!(
            plan.model
                .find_target(DirId::ROOT, b"earlier_autogen")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn missing_qt_reports_one_directory_warning() -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture_with(&host, GeneratorKind::Ninja, "8", None, |model, id, _| {
            let vars = &mut model.dir_mut(DirId::ROOT).vars;
            vars.shift_remove(b"Qt6Core_VERSION_MAJOR".as_slice());
            vars.shift_remove(b"Qt6Core_VERSION_MINOR".as_slice());
            let props = &mut model
                .target_mut(id)
                .ok_or("Expected the sample target.")?
                .props;
            // An explicit executable keeps RCC enabled without a Qt version.
            props.set("AUTOMOC_EXECUTABLE".into(), "".into());
            props.set("AUTOUIC_EXECUTABLE".into(), "".into());
            Ok(())
        })?;
        let warnings = plan
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .body
                    .windows(19)
                    .any(|part| part == b"No valid Qt version")
            })
            .collect::<Vec<_>>();
        let [warning] = warnings.as_slice() else {
            return Err(format!("Expected one warning, found {warnings:?}.").into());
        };
        assert_eq!(
            warning.body,
            "AUTOGEN: No valid Qt version found for target sample.  AUTOMOC and AUTOUIC disabled.  Consider adding:\n  find_package(Qt<QTVERSION> COMPONENTS Widgets)\nto your CMakeLists.txt file."
        );
        assert!(matches!(
            &warning.location,
            rcmake_diag::Location::FileLine { file, line: 0 } if file.ends_with(b"CMakeLists.txt")
        ));
        Ok(())
    }

    #[test]
    fn autogen_requires_an_absolute_build_directory() -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let result = fixture_with(&host, GeneratorKind::Ninja, "8", None, |model, id, _| {
            model
                .target_mut(id)
                .ok_or("Expected the sample target.")?
                .props
                .set("AUTOGEN_BUILD_DIR".into(), "generated".into());
            Ok(())
        });
        let Err(error) = result else {
            return Err("Expected a relative build directory error.".into());
        };
        assert!(
            error
                .to_string()
                .contains("AUTOGEN_BUILD_DIR for target \"sample\" must be an absolute path")
        );
        Ok(())
    }

    fn static_ordering(cyclic: bool) -> Result<Vec<BString>, Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture_with(
            &host,
            GeneratorKind::NinjaMultiConfig,
            "8",
            None,
            |model, id, input| {
                let mut dep = Target::new(
                    "dep".into(),
                    TargetKind::StaticLibrary,
                    TargetOrigin::Normal,
                    DirId::ROOT,
                    Backtrace::default(),
                    PolicyMap::default(),
                );
                let link = |name: &str| LinkItem {
                    opens_lookup_scope: false,
                    closes_lookup_scope: false,
                    scope: None,
                    text: BT::new(name.into(), Backtrace::default()),
                    lookup_dir: DirId::ROOT,
                };
                if cyclic {
                    dep.usage.link_libraries = Some(vec![link("$<$<CONFIG:Release>:sample>")]);
                }
                let dep = model.add_target(dep)?;
                let target = model.target_mut(id).ok_or("Expected the sample target.")?;
                target.kind = TargetKind::StaticLibrary;
                target.usage.link_libraries = Some(vec![link("dep")]);
                for config in input {
                    config.link_targets.push(dep);
                }
                Ok(())
            },
        )?;
        let ordering = plan
            .model
            .find_target(DirId::ROOT, b"sample_autogen_timestamp_deps")
            .and_then(|id| plan.model.target(id))
            .ok_or("Expected an AUTOGEN timestamp dependency target.")?;
        Ok(ordering
            .utility_deps
            .iter()
            .map(|dep| dep.value.clone())
            .collect())
    }

    #[test]
    fn autogen_keeps_acyclic_static_library_dependencies() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(static_ordering(false)?, ["dep"]);
        Ok(())
    }

    #[test]
    fn autogen_excludes_static_library_cycles_in_any_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(static_ordering(true)?.is_empty());
        Ok(())
    }

    #[test]
    fn legacy_cross_config_stamp_produces_all_module_moc_outputs()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::new("/".into(), "/cmake".into());
        let plan = fixture_with(
            &host,
            GeneratorKind::NinjaMultiConfig,
            "13",
            Some("OFF"),
            |model, _, input| {
                model
                    .dir_mut(DirId::ROOT)
                    .vars
                    .insert("CMAKE_CROSS_CONFIGS".into(), "all".into());
                for config in input {
                    config.sources.push(SourceInput {
                        path: "/src/shared.cppm".into(),
                        cxx_module: true,
                        generated: false,
                    });
                    if config.config.0 == b"Debug" {
                        config.sources.push(SourceInput {
                            path: "/src/debug.cppm".into(),
                            cxx_module: true,
                            generated: false,
                        });
                    }
                }
                Ok(())
            },
        )?;
        let stamp = plan
            .model
            .commands()
            .map(|(_, command)| command)
            .find(|command| {
                command
                    .depfile
                    .as_ref()
                    .is_some_and(|file| file == b"/build/sample_autogen/deps")
            })
            .ok_or("Expected a shared timestamp command.")?;
        assert_eq!(
            stamp
                .command_lines
                .iter()
                .filter(|line| line.get(2).is_some_and(|arg| arg == b"cmake_autogen"))
                .filter_map(|line| line.last())
                .map(|config| config.as_slice())
                .collect::<Vec<_>>(),
            [b"Debug".as_slice(), b"Release"],
        );
        let target = plan
            .model
            .find_target(DirId::ROOT, b"sample")
            .and_then(|id| plan.model.target(id))
            .ok_or("Expected the sample target.")?;
        for (config, source) in [
            ("Debug", "debug"),
            ("Debug", "shared"),
            ("Release", "shared"),
        ] {
            let path =
                format!("/build/sample_autogen/include_{config}/EWIEGA46WW/moc_{source}.cpp");
            assert!(
                stamp
                    .byproducts
                    .iter()
                    .any(|output| output.value == path.as_bytes())
            );
            let source = format!("$<$<CONFIG:{config}>:{path}>");
            assert!(
                target
                    .sources
                    .iter()
                    .any(|item| item.value == source.as_bytes())
            );
        }
        assert!(!stamp.byproducts.iter().any(|item| {
            item.value
                .ends_with(b"include_Release/EWIEGA46WW/moc_debug.cpp")
        }));
        Ok(())
    }
}
