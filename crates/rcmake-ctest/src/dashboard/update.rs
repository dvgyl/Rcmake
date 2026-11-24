use rcmake_base::truth;
use rcmake_host::{Host, ProcessResult, path};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::cli::Invocation;
use crate::dart::{Configuration, Model, Tag};
use crate::junit::{escape, write_file};

use super::definition;

pub(super) fn run(host: &dyn Host, invocation: &Invocation) -> Result<i32, String> {
    let cwd = host.initial_cwd();
    let initial = &invocation.options.test_dir;
    let preliminary = Configuration::load(host, initial, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let source = invocation.source_dir.as_ref().map_or_else(
        || initial.join(path::to_native(&preliminary.source_directory)),
        |source| cwd.join(source),
    );
    let mut build = initial.clone();
    if invocation.build_dir.is_none() {
        if !preliminary.build_directory.is_empty() {
            build = cwd.join(path::to_native(&preliminary.build_directory));
        } else if let Some(preset) = definition(invocation, b"CTEST_CONFIGURE_PRESET")
            .or_else(|| definition(invocation, b"CTEST_PRESET"))
        {
            let set = match &invocation.presets_file {
                Some(file) => rcmake_presets::load_file(&source, file, host),
                None => rcmake_presets::load(&source, host),
            }
            .map_err(|error| error.message)?;
            let preset = set
                .resolve(PresetKind::Configure, preset, &EnvSnapshot::from_host(host))
                .map_err(|error| error.message)?;
            if let Some(dir) = preset.binary_dir {
                build = source.join(path::to_native(&dir));
            }
        }
    }
    let config = Configuration::load(host, &build, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let tag = Tag::read(host, &build)
        .or_else(|_| {
            Tag::create(
                host,
                &build,
                host.now(),
                invocation.dashboard_model.unwrap_or(Model::Experimental),
                None,
                &config.nightly_start,
            )
        })
        .map_err(|error| error.to_string())?;
    let program = definition(invocation, b"CTEST_UPDATE_COMMAND").unwrap_or(&config.update_command);
    let version_only =
        definition(invocation, b"CTEST_UPDATE_VERSION_ONLY").is_some_and(truth::is_on);
    let output = if program.is_empty() {
        None
    } else {
        let args = if version_only {
            vec!["--version".into()]
        } else {
            Vec::new()
        };
        Some(
            crate::script::run_process(host, source, program.into(), args, host.env_all(), false)
                .map_err(|error| error.to_string())?,
        )
    };
    let succeeded = output
        .as_ref()
        .is_none_or(|output| matches!(output.results.as_slice(), [ProcessResult::Exited(0)]));
    let status = if succeeded {
        ""
    } else {
        "Update command failed."
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Update mode=\"Client\">\n<Site>{}</Site>\n<BuildName>{}</BuildName>\n<BuildStamp>{}-{}</BuildStamp>\n<UpdateReturnStatus>{status}</UpdateReturnStatus>\n</Update>\n",
        escape(&config.site, false),
        escape(&config.build_name, false),
        tag.stamp(),
        escape(tag.track.as_bytes(), false),
    );
    write_file(
        host,
        &build.join("Testing").join(tag.stamp()).join("Update.xml"),
        xml.as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    Ok(if succeeded { 0 } else { 1 })
}
