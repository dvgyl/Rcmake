use std::path::Path;
use std::sync::Arc;

use rcmake_base::{
    BString,
    list::{Empty, split},
    path_lex::Lexical,
};
use rcmake_diag::{
    DiagAction, DiagCategory, Diagnostic, Effect, Location, MessageKind, RenderOptions, render,
};
use rcmake_eval::Interp;
use rcmake_gen_common::{self as common, GenError};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, Stream, path};
use rcmake_instrument::snippet::{self, ContentTarget};
use rcmake_instrument::{
    CustomContent, CustomContentType, InstrumentError, InstrumentOption, Query,
};
use rcmake_model::{FileSetId, GeneratorKind, ProjectModel, TargetOrigin};
use rcmake_resolve::ide::IdeFlavor;
use rcmake_resolve::{GenerationResult, InstallActionKind, InstallManifest, ResolveOptions};

mod file_generate;
mod install_index;
mod instrument;

pub(crate) enum Failure {
    Reported,
    Message(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

pub(crate) fn run(
    interp: &mut Interp,
    host: &dyn Host,
    bin: &Path,
    queries: &[Query],
) -> Result<bool, Failure> {
    let outcome = generate(interp, host, bin, queries);
    if outcome.is_err()
        && let Err(error) = common::fileapi::write_error_replies(interp.cx().project(), host, bin)
    {
        crate::emit_cli_error(host, error.to_string());
    }
    outcome
}

fn generate(
    interp: &mut Interp,
    host: &dyn Host,
    bin: &Path,
    queries: &[Query],
) -> Result<bool, Failure> {
    let Some(kind) = interp.cx().project().generator else {
        return Ok(false);
    };
    if matches!(kind, GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig) {
        interp.cx().var(b"CMAKE_NINJA_OUTPUT_PATH_PREFIX");
    }
    let configs = if kind.multi_config() {
        match interp.cx().var(b"CMAKE_CONFIGURATION_TYPES") {
            Some(value) => {
                let mut configs: Vec<_> = split(&value, Empty::Drop)
                    .into_iter()
                    .map(ConfigId)
                    .collect();
                if configs.is_empty() {
                    configs.push(ConfigId(BString::default()));
                }
                configs
            }
            None => kind
                .default_configuration_types()
                .iter()
                .map(|name| ConfigId((*name).into()))
                .collect(),
        }
    } else {
        vec![ConfigId(interp.cx().var_or_empty(b"CMAKE_BUILD_TYPE"))]
    };
    let options = ResolveOptions {
        provenance: false,
        ide: match kind {
            GeneratorKind::Xcode => Some(IdeFlavor::Xcode),
            GeneratorKind::VisualStudio(_) => Some(IdeFlavor::VisualStudio),
            GeneratorKind::Ninja
            | GeneratorKind::NinjaMultiConfig
            | GeneratorKind::UnixMakefiles => None,
        },
    };
    configure_rules(interp)?;
    let resolved = {
        let mut cx = interp.cx();
        let model = cx.project();
        let key = b"_RCMAKE_INSTRUMENTATION_QUERIES";
        if queries.is_empty() || !model.global.instrumentation.is_empty() {
            rcmake_resolve::resolve_with_options(model, host, &configs, &options)
        } else {
            let previous = model.global.props.get(key).cloned();
            model.global.props.set(key.as_slice().into(), "1".into());
            let resolved = rcmake_resolve::resolve_with_options(model, host, &configs, &options);
            if let Some(previous) = previous {
                model.global.props.set(key.as_slice().into(), previous);
            } else {
                model.global.props.unset(key);
            }
            resolved
        }
    };
    let mut result = match resolved {
        Ok(result) => result,
        Err(rcmake_resolve::ResolveError::Reported(_)) => return Err(Failure::Reported),
        Err(err) => {
            return Err(err
                .with_export_linker_language(interp.cx().project())
                .to_string()
                .into());
        }
    };
    if result.graphs.iter().flat_map(|graph| &graph.steps).any(|step| {
        matches!(step, rcmake_resolve::BuildStep::Collate { depend_info, .. } if matches!(depend_info.as_ref(), rcmake_resolve::DependInfo::Cxx(info) if info.database_info.is_some()))
    }) {
        let category = DiagCategory::Experimental;
        let action = interp.cx().diag_state().get(category);
        if action != DiagAction::Ignore {
            let failure = action > DiagAction::Warn;
            let diagnostic = Diagnostic { annotations: None,
                kind: if failure { MessageKind::AuthorError } else { MessageKind::AuthorWarning },
                effect: if failure { Effect::SetError } else { Effect::None },
                category: Some(category),
                policy: None,
                location: Location::FileLine { file: Arc::from(b"CMakeLists.txt".as_slice()), line: 0 },
                body: format!("{}  It is meant only for experimentation and feedback to CMake developers.",
                    rcmake_diag::EXPORT_BUILD_DATABASE_DESCRIPTION).into(),
            };
            host.write_out(Stream::Stderr, &render(&diagnostic, &RenderOptions::default()));
            if failure {
                return Err(Failure::Reported);
            }
        }
    }
    if !queries.is_empty() && !kind.is_ide() {
        instrument::wrap_custom_commands(&mut result, interp.cx().project(), host, bin);
    }
    let ide_result: Option<Result<(), GenError>> = match kind {
        #[cfg(any(target_os = "macos", feature = "xcode-cross"))]
        GeneratorKind::Xcode => {
            let generated = rcmake_gen_xcode::generate(&result, interp.cx().project(), host, bin);
            if generated.is_ok() {
                rcmake_gen_xcode::populate_guid_cache(&result, interp.cx().project(), host);
            }
            Some(generated)
        }
        #[cfg(any(windows, feature = "vs-cross"))]
        GeneratorKind::VisualStudio(version) => Some(rcmake_gen_vs::generate(
            version,
            &result,
            interp.cx().project(),
            host,
            bin,
        )),
        #[cfg(not(any(target_os = "macos", feature = "xcode-cross")))]
        GeneratorKind::Xcode => {
            return Err(format!("Could not create named generator {}", kind.name()).into());
        }
        #[cfg(not(any(windows, feature = "vs-cross")))]
        GeneratorKind::VisualStudio(_) => {
            return Err(format!("Could not create named generator {}", kind.name()).into());
        }
        GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig | GeneratorKind::UnixMakefiles => {
            None
        }
    };
    if let Some(Err(err)) = ide_result {
        return Err(err.to_string().into());
    }
    write_common(interp, host, bin, &result, kind).map_err(|err| match err {
        GenError::Invalid(message) if message.starts_with(b"\nCMake Error") => {
            host.write_out(
                Stream::Stderr,
                message.strip_prefix(b"\n").unwrap_or(&message),
            );
            Failure::Reported
        }
        error => Failure::Message(error.to_string()),
    })?;
    let generated = match kind {
        GeneratorKind::Ninja => rcmake_gen_ninja::generate_single_config_with_model(
            &result,
            host,
            bin,
            interp.cx().project(),
        ),
        GeneratorKind::NinjaMultiConfig => rcmake_gen_ninja::generate_multi_config_with_model(
            &result,
            host,
            bin,
            interp.cx().project(),
        ),
        GeneratorKind::UnixMakefiles => {
            rcmake_gen_make::generate_with_model(&result, host, bin, interp.cx().project())
        }
        GeneratorKind::Xcode | GeneratorKind::VisualStudio(_) => Ok(()),
    };
    match generated {
        Ok(()) => {
            if !queries.is_empty() || interp.cx().var_is_on(b"CTEST_USE_LAUNCHERS") {
                let project_name = interp.cx().var_or_empty(b"CMAKE_PROJECT_NAME");
                write_instrumentation_content(
                    host,
                    bin,
                    interp.cx().project(),
                    &project_name,
                    queries,
                )
                .map_err(|err| err.to_string())?;
            }
            common::fileapi::write_replies(interp.cx().project(), &result, host, bin)
                .map_err(|err| err.to_string())?;
            Ok(true)
        }
        Err(GenError::Unimplemented(_)) => Ok(false),
        Err(err) => Err(err.to_string().into()),
    }
}

fn configure_rules(interp: &mut Interp) -> Result<(), Failure> {
    struct RuleCall {
        set: FileSetId,
        dir: rcmake_model::DirId,
        name: BString,
        target: BString,
        file_set: BString,
        output_set: BString,
        files: Vec<BString>,
        file_set_configurators: Vec<BString>,
        source_configurators: Vec<BString>,
    }
    let calls = {
        let mut cx = interp.cx();
        let model: &ProjectModel = cx.project();
        model
            .targets()
            .flat_map(|(_, target)| {
                target.file_sets.iter().filter_map(move |&id| {
                    let set = model.file_set(id)?;
                    let name = set.props.get(b"_RCMAKE_RULE")?;
                    let dir = model.file_set_rule_dir(id).unwrap_or(target.dir);
                    let rule = model.custom_rule(dir, name)?;
                    let list = |key: &[u8]| {
                        rule.get(key)
                            .map_or_else(Vec::new, |value| split(value, Empty::Drop))
                    };
                    let file_set_configurators = list(b"FILE_SET_CONFIGURATORS");
                    let source_configurators = list(b"SOURCE_CONFIGURATORS");
                    if file_set_configurators.is_empty() && source_configurators.is_empty() {
                        return None;
                    }
                    let output_set = list(b"OUTPUT_FILE_SET")
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    use rcmake_base::ByteSlice;
                    let output_set = output_set
                        .replace(b"<RULE>", name)
                        .replace(b"<TARGET>", &target.name)
                        .replace(b"<FILE_SET>", &set.name)
                        .into();
                    Some(RuleCall {
                        set: id,
                        dir,
                        name: name.clone(),
                        target: target.name.clone(),
                        file_set: set.name.clone(),
                        output_set,
                        files: set.files.iter().map(|file| file.value.clone()).collect(),
                        file_set_configurators,
                        source_configurators,
                    })
                })
            })
            .collect::<Vec<_>>()
    };
    let mut failed = false;
    for call in calls {
        let mut cx = interp.cx();
        let mut invoke = |commands: &[BString], source: Option<&BString>| {
            let mut args = vec![
                call.name.clone(),
                call.target.clone(),
                call.file_set.clone(),
                call.output_set.clone(),
            ];
            args.extend(source.cloned());
            let variable = b"_CMAKE_RULE_PATTERNS";
            args.push(variable.as_slice().into());
            let args = args
                .into_iter()
                .map(|text| rcmake_syntax::Argument {
                    kind: rcmake_syntax::ArgKind::Bracket,
                    text,
                    line: 0,
                    span: 0..0,
                })
                .collect::<Vec<_>>();
            cx.with_directory_scope(call.dir, |cx| {
                cx.set_var(variable, BString::default());
                cx.global().props.set(
                    b"\0RCMAKE_IN_RULE_CONFIGURATOR".as_slice().into(),
                    "1".into(),
                );
                for command in commands {
                    let result = cx
                        .expand_raw_args(&args)
                        .and_then(|args| cx.call_command(command, args));
                    failed |= result.is_err();
                }
                cx.global().props.unset(b"\0RCMAKE_IN_RULE_CONFIGURATOR");
                cx.var_or_empty(variable)
            })
        };
        let file_set = invoke(&call.file_set_configurators, None);
        let sources = call
            .files
            .iter()
            .filter(|_| !call.source_configurators.is_empty())
            .map(|file| (file, invoke(&call.source_configurators, Some(file))))
            .collect::<Vec<_>>();
        if let Some(set) = cx.project().file_set_mut(call.set) {
            if !file_set.is_empty() {
                set.props
                    .set("_RCMAKE_RULE_FILE_SET_PATTERNS".into(), file_set);
            }
            for (source, value) in sources {
                if !value.is_empty() {
                    set.props.set(
                        [b"_RCMAKE_RULE_PATTERNS_".as_slice(), source]
                            .concat()
                            .into(),
                        value,
                    );
                }
            }
        }
    }
    if failed || interp.cx().diag_engine().error_occurred() {
        return Err(Failure::Reported);
    }
    Ok(())
}

fn write_instrumentation_content(
    host: &dyn Host,
    bin: &Path,
    model: &ProjectModel,
    project_name: &[u8],
    queries: &[Query],
) -> Result<(), InstrumentError> {
    let entries = model
        .global
        .instrumentation
        .iter()
        .flat_map(|request| &request.custom_content)
        .filter_map(|entry| {
            Some(CustomContent {
                name: entry.name.clone(),
                ty: CustomContentType::from_name(&entry.ty)?,
                content: entry.content.clone(),
            })
        })
        .collect::<Vec<_>>();
    let targets = model
        .targets()
        .filter(|(_, target)| target.origin == TargetOrigin::Normal)
        .map(|(_, target)| ContentTarget {
            name: target.name.clone(),
            kind: target.kind.as_str().into(),
            labels: target
                .props
                .get(b"LABELS")
                .into_iter()
                .chain(model.dir(target.dir).props.get(b"LABELS"))
                .chain(model.definition(target.dir, b"CMAKE_DIRECTORY_LABELS"))
                .flat_map(|value| split(value, Empty::Drop))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        })
        .collect::<Vec<_>>();
    let include_static = queries.iter().any(|query| {
        query
            .options
            .contains(&InstrumentOption::StaticSystemInformation)
    });
    snippet::write_content(host, bin, project_name, &entries, &targets, include_static)?;
    Ok(())
}

fn write_common(
    interp: &mut Interp,
    host: &dyn Host,
    bin: &Path,
    result: &GenerationResult,
    generator: GeneratorKind,
) -> Result<(), GenError> {
    let binary_dir = path::from_native(bin);
    let prefix = interp.cx().var(b"CMAKE_INSTALL_PREFIX");
    let prefix = prefix
        .as_ref()
        .map_or(b"/usr/local".as_slice(), |value| value.as_slice());
    let configuration = interp.cx().var_or_empty(b"CMAKE_BUILD_TYPE");
    let configuration = if configuration.is_empty()
        && interp
            .cx()
            .project()
            .generator
            .is_some_and(|kind| kind.multi_config())
    {
        [
            b"Release".as_slice(),
            b"MinSizeRel",
            b"RelWithDebInfo",
            b"Debug",
        ]
        .iter()
        .find_map(|name| result.configs.iter().find(|config| config.matches(name)))
        .or_else(|| result.configs.first())
        .map_or(configuration.as_slice(), |config| config.0.as_slice())
    } else {
        configuration.as_slice()
    };
    let supplementary =
        common::write_supplementary_outputs(interp.cx().project(), result, host, bin)?;
    let (mut manifest, export_files) = write_exports(result, bin, prefix, host)?;
    manifest.actions.extend(supplementary.actions);
    if interp.cx().var_is_on(b"CMAKE_SKIP_INSTALL_RULES") {
        if !manifest.actions.is_empty() {
            let warning = Diagnostic { annotations: None,
                kind: MessageKind::Warning,
                effect: Effect::None,
                category: None,
                policy: None,
                location: Location::FileLine {
                    file: Arc::from(b"CMakeLists.txt".as_slice()),
                    line: 0,
                },
                body: "CMAKE_SKIP_INSTALL_RULES was enabled even though installation rules have been specified".into(),
            };
            host.write_out(Stream::Stderr, &render(&warning, &RenderOptions::default()));
        }
    } else {
        let options = common::install_script::InstallScriptOptions {
            prefix,
            configuration,
            export_files: &export_files,
            ..Default::default()
        };
        install_index::write(host, bin, interp.cx().project(), &manifest, options)?;
    }
    let cmake_files = bin.join("CMakeFiles");
    let log_path = cmake_files.join("CMakeConfigureLog.yaml");
    if host.metadata(&log_path, rcmake_host::Follow::Yes).is_err() {
        let content = common::configure_log::render(&result.metadata)?;
        host.create_dir_all(&cmake_files)
            .map_err(|err| GenError::Write {
                path: rcmake_base::path_lex::cmake_style(&path::from_native(&cmake_files)),
                message: err.to_string().into(),
            })?;
        common::write_if_changed(host, &log_path, &content)?;
    }

    let export_commands = interp.cx().var_is_on(b"CMAKE_EXPORT_COMPILE_COMMANDS");
    if export_commands
        && !generator.is_ide()
        && let Some(graph) = result.graphs.first()
    {
        write(
            host,
            &bin.join("compile_commands.json"),
            common::compile_commands::render_for_generator(result, graph, &binary_dir, generator),
        )?;
    }
    let testing = interp.cx().var_is_on(b"CMAKE_TESTING_ENABLED");
    {
        let mut cx = interp.cx();
        let model = cx.project();
        for (id, dir) in model.dirs() {
            if !testing
                && !dir
                    .props
                    .get(b"CMAKE_TESTING_ENABLED")
                    .is_some_and(|value| rcmake_base::truth::is_on(value))
            {
                continue;
            }
            let test_file = path::to_native(&dir.binary_dir).join("CTestTestfile.cmake");
            write(
                host,
                &test_file,
                common::ctest_file::render_resolved_with_host(model, id, result, host),
            )?;
        }
    }
    file_generate::write(interp.cx().project(), result, host)
}

fn write_exports(
    result: &GenerationResult,
    bin: &Path,
    prefix: &[u8],
    host: &dyn Host,
) -> Result<(InstallManifest, Vec<BString>), GenError> {
    let mut destinations = vec![None; result.exports.len()];
    for action in &result.install.actions {
        if let InstallActionKind::Export { plan } = action.kind {
            *destinations
                .get_mut(plan)
                .ok_or(GenError::MissingExport(plan))? = Some(action.destination.as_slice());
        }
    }
    let mut staged = Vec::with_capacity(result.exports.len());
    for (index, (plan, destination)) in result.exports.iter().zip(destinations).enumerate() {
        let files = if plan.install {
            let destination = destination.ok_or(GenError::MissingExport(index))?;
            if plan.file.iter().any(|byte| matches!(byte, b'/' | b'\\')) {
                return Err(GenError::Write {
                    path: plan.file.clone(),
                    message: "Specify an install export file name without a directory.".into(),
                });
            }
            common::export::write_export_plan_with_prefix(
                plan,
                &bin.join("CMakeFiles/Export").join(index.to_string()),
                Some(destination),
                Some(prefix),
                host,
            )?
        } else {
            if !plan.file.is_empty() {
                common::export::write_export_plan(plan, bin, None, host)?;
            }
            Vec::new()
        };
        staged.push(files);
    }

    let mut manifest = InstallManifest::default();
    let mut export_files = staged
        .iter()
        .map(|files| {
            files
                .first()
                .map_or_else(BString::default, |file| path::from_native_cmake(file))
        })
        .collect::<Vec<_>>();
    let mut offsets = Vec::with_capacity(result.install.actions.len() + 1);
    for action in &result.install.actions {
        offsets.push(manifest.actions.len());
        manifest.actions.push(action.clone());
        if let InstallActionKind::Export { plan } = action.kind {
            let (main, configurations) = staged
                .get(plan)
                .and_then(|files| files.split_first())
                .ok_or(GenError::MissingExport(plan))?;
            // The script writer accepts one file per export action.
            let main = path::from_native_cmake(main);
            let filename = Lexical::new(&main).filename();
            let stem = filename.strip_suffix(b".cmake").unwrap_or(filename);
            let filename_prefix = [stem, b"-"].concat();
            for file in configurations {
                let file = path::from_native_cmake(file);
                let config = Lexical::new(&file)
                    .filename()
                    .strip_prefix(filename_prefix.as_slice())
                    .and_then(|name| name.strip_suffix(b".cmake"))
                    .unwrap_or_default();
                let mut action = action.clone();
                if result.configs.len() > 1 {
                    action.configurations = vec![config.into()];
                }
                let plan = export_files.len();
                export_files.push(file);
                action.kind = InstallActionKind::Export { plan };
                manifest.actions.push(action);
            }
        }
    }
    offsets.push(manifest.actions.len());
    manifest.directories = result.install.directories.clone();
    for entry in manifest.directories.values_mut().flatten() {
        if let rcmake_resolve::InstallEntry::Actions(range) = entry {
            let Some((&start, &end)) = offsets.get(range.start).zip(offsets.get(range.end)) else {
                return Err(GenError::Invalid(
                    "Invalid range of install actions.".into(),
                ));
            };
            *range = start..end;
        }
    }
    Ok((manifest, export_files))
}

fn write(host: &dyn Host, path: &Path, content: Result<BString, GenError>) -> Result<(), GenError> {
    common::write_if_changed(host, path, &content?)
}

#[cfg(test)]
mod tests {
    use rcmake_host::{Follow, HostError, ProcessResult, ReadRange, RunOutput, Stream};

    use super::*;

    #[test]
    fn configure_and_build_collect_instrumentation() -> Result<(), Box<dyn std::error::Error>> {
        let host = crate::driver_tests::project_host();
        for (module, content) in [
            (
                "CMakeDetermineCCompiler.cmake",
                "file(WRITE \"${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake\" \"set(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
            ),
            ("CMakeCInformation.cmake", ""),
            ("CMakeTestCCompiler.cmake", ""),
        ] {
            host.seed_file(
                &Path::new("/usr/local/share/cmake-4.5/Modules").join(module),
                content.as_bytes(),
            )?;
        }
        host.seed_file(Path::new("/tools/cc"), b"")?;
        let source = Path::new("/work/CMakeLists.txt");
        let bin = Path::new("/work/out");
        let api = bin.join(".cmake/instrumentation/v1");
        let data = api.join("data");
        let project = br#"cmake_minimum_required(VERSION 4.5)
set(CMAKE_C_COMPILER /tools/cc)
project(example LANGUAGES C)
add_executable(app main.c)
"#;
        host.seed_file(source, project)?;
        host.seed_file(Path::new("/work/main.c"), b"int main(void) { return 0; }\n")?;

        let invoke = |args: &[&str]| {
            let argv: Vec<_> = args.iter().copied().map(BString::from).collect();
            assert_eq!(
                crate::run(&argv, host.clone()),
                0,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
        };
        let configure = || {
            invoke(&[
                "cmake",
                "-S",
                ".",
                "-B",
                "out",
                "-G",
                "Ninja",
                "-DCMAKE_MAKE_PROGRAM=ninja",
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
            ]);
        };
        let read = |name: &str| {
            host.read(&bin.join(name), ReadRange::All { limit: 65536 })
                .map(BString::from)
        };
        configure();
        let manifest = read("build.ninja")?;
        let commands = read("compile_commands.json")?;
        assert!(!manifest.to_string().contains("--instrument"));
        assert!(!commands.to_string().contains("--instrument"));
        assert!(host.metadata(&api, Follow::Yes).is_err());

        let mut instrumented = project.to_vec();
        instrumented.extend_from_slice(
            br#"
cmake_instrumentation(API_VERSION 1 DATA_VERSION 1.2
  HOOKS postGenerate preCMakeBuild postCMakeBuild postCMakeInstall
  CALLBACK record)
"#,
        );
        host.seed_file(source, &instrumented)?;
        let callback_dir = data.clone();
        host.set_command_handler(move |host, request, _| {
            let command = request
                .pipeline
                .first()
                .ok_or_else(|| HostError::unsupported("Missing command."))?;
            if command.program == "record" {
                let argument = command
                    .args
                    .first()
                    .ok_or_else(|| HostError::unsupported("Missing index argument."))?;
                let index = path::to_native(argument);
                assert!(index.starts_with(callback_dir.join("index")));
                assert!(
                    host.metadata(Path::new("/work/out/CMakeCache.txt"), Follow::Yes)
                        .is_ok()
                );
                let index = host.read(&index, ReadRange::All { limit: 65536 })?;
                let index = rcmake_json::parse(&index, &rcmake_json::ReadOptions::default())
                    .map_err(|error| HostError::unsupported(error.to_string()))?;
                let rcmake_json::Value::Object(fields) = index else {
                    panic!("Expected an index object.");
                };
                let Some(rcmake_json::Value::String(hook)) = fields.get(b"hook".as_slice()) else {
                    panic!("Expected the hook name.");
                };
                let Some(rcmake_json::Value::Array(snippets)) = fields.get(b"snippets".as_slice())
                else {
                    panic!("Expected a list of snippet files.");
                };
                let roles: Vec<_> = snippets
                    .iter()
                    .map(|snippet| {
                        let rcmake_json::Value::String(name) = snippet else {
                            panic!("Expected a snippet file name.");
                        };
                        assert!(
                            host.metadata(&callback_dir.join(path::to_native(name)), Follow::Yes)
                                .is_ok()
                        );
                        name.split(|byte| *byte == b'-').next().unwrap_or_default()
                    })
                    .collect();
                let expected: &[&[u8]] = match hook.as_slice() {
                    b"postGenerate" => &[b"configure", b"generate"],
                    b"preCMakeBuild" => &[],
                    b"postCMakeBuild" => &[b"cmakeBuild"],
                    b"postCMakeInstall" => &[b"cmakeInstall"],
                    _ => panic!("Unexpected hook {hook}."),
                };
                assert_eq!(roles, expected);
                host.write_out(Stream::Stdout, hook);
            } else {
                assert_eq!(command.program, "ninja");
                if command.args.iter().any(|arg| arg == "interrupted") {
                    return Ok(RunOutput {
                        results: vec![ProcessResult::Signaled(2)],
                        ..RunOutput::default()
                    });
                }
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        configure();
        let rules = read("CMakeFiles/rules.ninja")?
            .to_string()
            .replace("$ ", " ");
        let wrapped = read("build.ninja")?.to_string().replace("$ ", " ");
        assert!(rules.contains("--instrument $COMMAND"));
        assert!(
            wrapped.contains("COMPILE_INSTRUMENTED")
                || wrapped.contains("COMPILE_DEPFILE_INSTRUMENTED")
        );
        assert!(wrapped.contains("--command-type compile"));
        assert!(wrapped.contains("--instrument --command-type link"));
        assert!(
            host.read_dir(&api.join("query/generated"))?
                .iter()
                .any(|entry| entry.name.starts_with(b"query-"))
        );
        assert!(
            host.read_dir(&data.join("content"))?
                .iter()
                .any(|entry| entry.name.starts_with(b"cmake-"))
        );
        invoke(&["cmake", "--build", "out"]);
        invoke(&["cmake", "--install", "out"]);
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        for hook in [
            "postGenerate",
            "preCMakeBuild",
            "postCMakeBuild",
            "postCMakeInstall",
        ] {
            assert!(stdout.contains(hook));
        }

        assert!(stdout.contains("postGenerate-- Build files have been written to: /work/out\n"));

        let interrupted = ["cmake", "--build", "out", "--target", "interrupted"].map(BString::from);
        assert_eq!(crate::run(&interrupted, host.clone()), 130);
        let after_interrupt = host.stream_bytes(Stream::Stdout).to_string();
        assert_eq!(after_interrupt.strip_prefix(&stdout), Some("preCMakeBuild"));
        let snippet = host
            .read_dir(&data)?
            .into_iter()
            .find(|entry| entry.name.starts_with(b"cmakeBuild-"))
            .ok_or("Missing build snippet.")?;
        let snippet = host.read(
            &data.join(path::to_native(&snippet.name)),
            ReadRange::All { limit: 65536 },
        )?;
        let snippet = rcmake_json::parse(&snippet, &rcmake_json::ReadOptions::default())?;
        let rcmake_json::Value::Object(fields) = snippet else {
            panic!("Expected a snippet object.");
        };
        assert_eq!(
            fields.get(b"result".as_slice()),
            Some(&rcmake_json::Value::Null)
        );
        assert_eq!(
            fields.get(b"interruptSignal".as_slice()),
            Some(&rcmake_json::Value::Int(2))
        );

        host.seed_file(source, project)?;
        configure();
        assert_eq!(read("build.ninja")?, manifest);
        assert_eq!(read("compile_commands.json")?, commands);
        assert!(
            host.metadata(&api.join("query/generated"), Follow::Yes)
                .is_err()
        );
        Ok(())
    }
}
