use std::path::Path;

use rcmake_base::{
    BString,
    list::{Empty, split},
    path_lex::Lexical,
};
use rcmake_eval::Interp;
use rcmake_gen_common::{self as common, GenError};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path};
use rcmake_instrument::snippet::{self, ContentTarget};
use rcmake_instrument::{
    CustomContent, CustomContentType, InstrumentError, InstrumentOption, Query,
};
use rcmake_model::{GeneratorKind, ProjectModel, TargetOrigin};
use rcmake_resolve::ide::IdeFlavor;
use rcmake_resolve::{
    GenerationResult, InstallActionKind, InstallManifest, ResolveError, ResolveOptions,
};

mod file_generate;
mod instrument;

pub(crate) fn run(
    interp: &mut Interp,
    host: &dyn Host,
    bin: &Path,
    queries: &[Query],
) -> Result<bool, String> {
    let Some(kind) = interp.cx().project().generator else {
        return Ok(false);
    };
    let configs = if kind.multi_config() {
        match interp.cx().var(b"CMAKE_CONFIGURATION_TYPES") {
            Some(value) => split(&value, Empty::Drop)
                .into_iter()
                .map(ConfigId)
                .collect(),
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
        ide: match kind {
            GeneratorKind::Xcode => Some(IdeFlavor::Xcode),
            GeneratorKind::VisualStudio(_) => Some(IdeFlavor::VisualStudio),
            GeneratorKind::Ninja
            | GeneratorKind::NinjaMultiConfig
            | GeneratorKind::UnixMakefiles => None,
        },
    };
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
        Err(ResolveError::Unimplemented(_)) => return Ok(false),
        Err(err) => return Err(err.to_string()),
    };
    if !queries.is_empty() && !kind.is_ide() {
        instrument::wrap_custom_commands(&mut result, interp.cx().project(), host, bin);
    }
    // An IDE generator writes its project files first, so that a generator that is not
    // implemented yet leaves no partial output.
    let ide_result = match kind {
        GeneratorKind::Xcode => {
            let generated = rcmake_gen_xcode::generate(&result, interp.cx().project(), host, bin);
            if generated.is_ok() {
                rcmake_gen_xcode::populate_guid_cache(&result, interp.cx().project(), host);
            }
            Some(generated)
        }
        GeneratorKind::VisualStudio(version) => Some(rcmake_gen_vs::generate(
            version,
            &result,
            interp.cx().project(),
            host,
            bin,
        )),
        GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig | GeneratorKind::UnixMakefiles => {
            None
        }
    };
    match ide_result {
        Some(Err(GenError::Unimplemented(_))) => return Ok(false),
        Some(Err(err)) => return Err(err.to_string()),
        Some(Ok(())) | None => {}
    }
    write_common(interp, host, bin, &result, kind).map_err(|err| err.to_string())?;
    let result = match kind {
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
    match result {
        Ok(()) => {
            if !queries.is_empty() {
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
            Ok(true)
        }
        Err(GenError::Unimplemented(_)) => Ok(false),
        Err(err) => Err(err.to_string()),
    }
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
                .map_or_else(Vec::new, |v| split(v, Empty::Drop)),
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
    let (mut manifest, export_files) = write_exports(result, bin, prefix, host)?;
    let supplementary =
        common::write_supplementary_outputs(interp.cx().project(), result, host, bin)?;
    manifest.actions.extend(supplementary.actions);
    write(
        host,
        &bin.join("cmake_install.cmake"),
        common::install_script::render_with_options(
            &manifest,
            &binary_dir,
            &common::install_script::InstallScriptOptions {
                prefix,
                configuration,
                export_files: &export_files,
                ..Default::default()
            },
        ),
    )?;
    let cmake_files = bin.join("CMakeFiles");
    let log_path = cmake_files.join("CMakeConfigureLog.yaml");
    if host.metadata(&log_path, rcmake_host::Follow::Yes).is_err() {
        match common::configure_log::render(&result.metadata) {
            Ok(content) => {
                host.create_dir_all(&cmake_files)
                    .map_err(|err| GenError::Write {
                        path: path::from_native(&cmake_files),
                        message: err.to_string().into(),
                    })?;
                write(host, &log_path, Ok(content))?;
            }
            Err(GenError::Unimplemented(_)) => {}
            Err(err) => return Err(err),
        }
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
    file_generate::write(interp.cx().project(), result, host)?;
    let api_result = {
        let mut cx = interp.cx();
        common::fileapi::write_replies(cx.project(), result, host, bin)
    };
    match api_result {
        Err(GenError::Unimplemented(_)) => Ok(()),
        result => result,
    }
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
                .map_or_else(BString::default, |file| path::from_native(file))
        })
        .collect::<Vec<_>>();
    for action in &result.install.actions {
        manifest.actions.push(action.clone());
        if let InstallActionKind::Export { plan } = action.kind {
            let (main, configurations) = staged
                .get(plan)
                .and_then(|files| files.split_first())
                .ok_or(GenError::MissingExport(plan))?;
            // The script writer accepts one file per export action.
            let main = path::from_native(main);
            let filename = Lexical::new(&main).filename();
            let stem = filename.strip_suffix(b".cmake").unwrap_or(filename);
            let filename_prefix = [stem, b"-"].concat();
            for file in configurations {
                let file = path::from_native(file);
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
    Ok((manifest, export_files))
}

fn write(host: &dyn Host, path: &Path, content: Result<BString, GenError>) -> Result<(), GenError> {
    match content {
        Ok(content) => common::write_if_changed(host, path, &content),
        Err(GenError::Unimplemented(_)) => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_host::{Follow, ProcessResult, PureHost, ReadRange, RunOutput, Stream};

    use super::*;

    #[test]
    fn configure_and_build_collect_instrumentation() {
        let host = Arc::new(PureHost::default());
        let source = Path::new("/work/CMakeLists.txt");
        let bin = Path::new("/work/out");
        let api = bin.join(".cmake/instrumentation/v1");
        let data = api.join("data");
        let project = br#"cmake_minimum_required(VERSION 4.5)
project(example NONE)
set(CMAKE_C_COMPILER cc)
add_executable(app main.c)
"#;
        host.seed_file(source, project).unwrap();
        host.seed_file(Path::new("/work/main.c"), b"int main(void) { return 0; }\n")
            .unwrap();

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
            BString::from(
                host.read(&bin.join(name), ReadRange::All { limit: 65536 })
                    .unwrap(),
            )
        };
        configure();
        let manifest = read("build.ninja");
        let commands = read("compile_commands.json");
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
        host.seed_file(source, &instrumented).unwrap();
        let callback_dir = data.clone();
        host.set_command_handler(move |host, request, _| {
            let command = &request.pipeline[0];
            if command.program == "record" {
                let index = path::to_native(&command.args[0]);
                assert!(index.starts_with(callback_dir.join("index")));
                assert!(
                    host.metadata(Path::new("/work/out/CMakeCache.txt"), Follow::Yes)
                        .is_ok()
                );
                let index = host.read(&index, ReadRange::All { limit: 65536 })?;
                let index =
                    rcmake_json::parse(&index, &rcmake_json::ReadOptions::default()).unwrap();
                let rcmake_json::Value::Object(fields) = index else {
                    panic!("Expected an index object.");
                };
                let rcmake_json::Value::String(hook) = &fields[b"hook".as_slice()] else {
                    panic!("Expected the hook name.");
                };
                let rcmake_json::Value::Array(snippets) = &fields[b"snippets".as_slice()] else {
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
                        name.split(|byte| *byte == b'-').next().unwrap()
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
        })
        .unwrap();
        configure();
        let wrapped = read("build.ninja").to_string().replace("$ ", " ");
        assert!(wrapped.contains("--instrument --command-type compile"));
        assert!(wrapped.contains("--instrument --command-type link"));
        assert!(
            host.read_dir(&api.join("query/generated"))
                .unwrap()
                .iter()
                .any(|entry| entry.name.starts_with(b"query-"))
        );
        assert!(
            host.read_dir(&data.join("content"))
                .unwrap()
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
            .read_dir(&data)
            .unwrap()
            .into_iter()
            .find(|entry| entry.name.starts_with(b"cmakeBuild-"))
            .unwrap();
        let snippet = host
            .read(
                &data.join(path::to_native(&snippet.name)),
                ReadRange::All { limit: 65536 },
            )
            .unwrap();
        let snippet = rcmake_json::parse(&snippet, &rcmake_json::ReadOptions::default()).unwrap();
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

        host.seed_file(source, project).unwrap();
        configure();
        assert_eq!(read("build.ninja"), manifest);
        assert_eq!(read("compile_commands.json"), commands);
        assert!(
            host.metadata(&api.join("query/generated"), Follow::Yes)
                .is_err()
        );
    }
}
