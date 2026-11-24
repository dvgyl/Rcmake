use std::path::Path;

use rcmake_gen_common::{GenError, install_script};
use rcmake_host::{Host, path};
use rcmake_json::{Value, write_styled};
use rcmake_resolve::InstallManifest;

pub(super) fn write(
    host: &dyn Host,
    bin: &Path,
    manifest: &InstallManifest,
    mut options: install_script::InstallScriptOptions<'_>,
    parallel: bool,
) -> Result<(), GenError> {
    let cmake_files = bin.join("CMakeFiles");
    let directory = if parallel {
        cmake_files.join("install")
    } else {
        cmake_files.clone()
    };
    host.create_dir_all(&directory)
        .map_err(|error| GenError::Write {
            path: path::from_native(&directory),
            message: error.to_string().into(),
        })?;
    let script = if parallel {
        let dir = directory;
        let script = dir.join("cmake_install.cmake");
        options.top_level = false;
        let mut content =
            install_script::render_with_options(manifest, &path::from_native(bin), &options)?;
        content.extend_from_slice(
            b"\nstring(REPLACE \";\" \"\\n\" CMAKE_INSTALL_MANIFEST_CONTENT \"${CMAKE_INSTALL_MANIFEST_FILES}\")\nfile(WRITE \"${CMAKE_CURRENT_LIST_DIR}/install_local_manifest.txt\" \"${CMAKE_INSTALL_MANIFEST_CONTENT}\")\n",
        );
        super::write(host, &script, Ok(content))?;
        script
    } else {
        bin.join("cmake_install.cmake")
    };
    let value = crate::object([
        (
            "InstallScripts",
            Value::Array(vec![Value::string(path::from_native(&script))]),
        ),
        ("Parallel", Value::Bool(parallel)),
    ]);
    super::write(
        host,
        &cmake_files.join("InstallScripts.json"),
        Ok(write_styled(&value, "  ")),
    )
}
