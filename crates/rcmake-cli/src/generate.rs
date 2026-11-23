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
use rcmake_model::GeneratorKind;
use rcmake_resolve::{GenerationResult, InstallActionKind, InstallManifest, ResolveError};

mod file_generate;

pub(crate) fn run(interp: &mut Interp, host: &dyn Host, bin: &Path) -> Result<bool, String> {
    let Some(kind) = interp.cx().project().generator else {
        return Ok(false);
    };
    let configs = if kind.multi_config() {
        match interp.cx().var(b"CMAKE_CONFIGURATION_TYPES") {
            Some(value) => split(&value, Empty::Drop)
                .into_iter()
                .map(ConfigId)
                .collect(),
            None => ["Debug", "Release", "RelWithDebInfo"]
                .into_iter()
                .map(|name| ConfigId(name.into()))
                .collect(),
        }
    } else {
        vec![ConfigId(interp.cx().var_or_empty(b"CMAKE_BUILD_TYPE"))]
    };
    let result = match rcmake_resolve::resolve(interp.cx().project(), host, &configs) {
        Ok(result) => result,
        Err(ResolveError::Unimplemented(_)) => return Ok(false),
        Err(err) => return Err(err.to_string()),
    };
    write_common(interp, host, bin, &result, kind).map_err(|err| err.to_string())?;
    let result = match kind {
        GeneratorKind::Ninja => rcmake_gen_ninja::generate_single_config(&result, host, bin),
        GeneratorKind::NinjaMultiConfig => {
            rcmake_gen_ninja::generate_multi_config(&result, host, bin)
        }
        GeneratorKind::UnixMakefiles => {
            rcmake_gen_make::generate_unix_makefiles(&result, host, bin)
        }
    };
    match result {
        Ok(()) => Ok(true),
        Err(GenError::Unimplemented(_)) => Ok(false),
        Err(err) => Err(err.to_string()),
    }
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
    let (manifest, export_files) = write_exports(result, bin, prefix, host)?;
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
    if export_commands && let Some(graph) = result.graphs.first() {
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
                common::ctest_file::render_resolved(model, id, result),
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
