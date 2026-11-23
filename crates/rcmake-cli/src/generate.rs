use std::path::Path;

use rcmake_base::{
    BString,
    list::{Empty, split},
};
use rcmake_eval::Interp;
use rcmake_gen_common::{self as common, GenError};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path};
use rcmake_model::GeneratorKind;
use rcmake_resolve::{GenerationResult, ResolveError};

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
    write_common(interp, host, bin, &result).map_err(|err| err.to_string())?;
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
) -> Result<(), GenError> {
    let binary_dir = path::from_native(bin);
    write(
        host,
        &bin.join("cmake_install.cmake"),
        common::install_script::render(&result.install, &binary_dir),
    )?;
    let cmake_files = bin.join("CMakeFiles");
    match common::configure_log::render(&result.metadata) {
        Ok(content) => {
            host.create_dir_all(&cmake_files)
                .map_err(|err| GenError::Write {
                    path: path::from_native(&cmake_files),
                    message: err.to_string().into(),
                })?;
            write(
                host,
                &cmake_files.join("CMakeConfigureLog.yaml"),
                Ok(content),
            )?;
        }
        Err(GenError::Unimplemented(_)) => {}
        Err(err) => return Err(err),
    }

    let export_commands = interp.cx().var_is_on(b"CMAKE_EXPORT_COMPILE_COMMANDS");
    if export_commands && let Some(graph) = result.graphs.first() {
        write(
            host,
            &bin.join("compile_commands.json"),
            common::compile_commands::render(graph, &binary_dir),
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
            write(host, &test_file, common::ctest_file::render(model, id))?;
        }
    }
    for plan in &result.exports {
        if !plan.file.is_empty() {
            write(
                host,
                &bin.join(path::to_native(&plan.file)),
                common::export::render(plan),
            )?;
        }
    }
    let api_result = {
        let mut cx = interp.cx();
        common::fileapi::write_replies(cx.project(), result, host, bin)
    };
    match api_result {
        Err(GenError::Unimplemented(_)) => Ok(()),
        result => result,
    }
}

fn write(host: &dyn Host, path: &Path, content: Result<BString, GenError>) -> Result<(), GenError> {
    match content {
        Ok(content) => common::write_if_changed(host, path, &content),
        Err(GenError::Unimplemented(_)) => Ok(()),
        Err(err) => Err(err),
    }
}
