//! `rcmake explain --targets [-B <build-dir>] [-S <source-dir>] [--config <cfg>] [--json] [--all]`.
//!
//! Reads a fresh File API codemodel reply when available and current; otherwise
//! evaluates the project in memory using an overlay without writing to disk.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use rcmake_api::{
    ApiError, CompletionState, ConfigureRequest, ProcessPolicy, Services, WriteMode, run_configure,
};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_buildinfo::{ArtifactReader, DataSource, Evidence, ReadLimits, Section, TraceInput};
use rcmake_eval::FindTraceLevel;
use rcmake_host::{FileKind, Follow, Host, Stream, path};
use rcmake_json::Value;
use rcmake_model::{DirId, GeneratorKind, TargetKind};

use super::args::TargetsOptions;
use super::{ReportHost, error, request};
use crate::resolve_cli_path;

const SCHEMA: &str = "rcmake-explain-targets-v1";
const REPLY_DIR: &str = ".cmake/api/v1/reply";
const MAX_REPLY_FILES: usize = 200_000;
const MAX_TARGETS: usize = 100_000;

pub(super) const HELP: &str = "\
Usage: rcmake explain --targets [-B <build-dir>] [-S <source-dir>]
                      [--config <cfg>] [--all] [--json]
                      [--color=auto|always|never]

List targets in a project with their type and relative source directory.

Reads a fresh File API codemodel reply when available and up to date; otherwise
evaluates the project in memory without running subprocesses or writing to the
build directory.

  -B <build-dir>   Read the configured build directory. Default: current directory.
  -S <source-dir>  Evaluate a source directory directly in memory when no
                   configured build directory exists.
  --config <cfg>   Select a configuration in a multi-config build tree.
  --all            Include imported, utility, and synthetic targets.
  --json           Write one JSON report to standard output.
  --color=<mode>   Set terminal color. Default: auto.
  -h, --help       Show this help.

Examples
  rcmake explain --targets -B build
  rcmake explain --targets -B build --all --json
  rcmake explain --targets -S .

Exit status
  0  Targets were listed.
  1  Evaluation failed or named an unavailable configuration.
  2  Invalid arguments or an unavailable source or build directory.
";

#[derive(Clone, Debug, PartialEq, Eq)]
struct TargetEntry {
    name: BString,
    kind: BString,
    directory: BString,
    imported: bool,
    synthetic: bool,
}

#[derive(Debug)]
struct TargetsReport {
    source: &'static str,
    complete: bool,
    source_dir: BString,
    build_dir: BString,
    config: Option<BString>,
    all: bool,
    targets: Vec<TargetEntry>,
    total_count: usize,
}

pub(super) fn run(argv0: Option<&BString>, options: &TargetsOptions, host: &Arc<dyn Host>) -> i32 {
    let build = options.build.as_ref().map_or_else(
        || host.initial_cwd(),
        |dir| resolve_cli_path(host.as_ref(), dir),
    );
    let explicit_source = options
        .source
        .as_ref()
        .map(|dir| resolve_cli_path(host.as_ref(), dir));
    let has_cache = host
        .metadata(&build.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File);

    let use_cached_build = has_cache
        && explicit_source
            .as_deref()
            .is_none_or(|src| cached_source_matches(host.as_ref(), &build, src));

    let report = if use_cached_build {
        match try_load_fresh_fileapi(host.as_ref(), &build, options) {
            Ok(Some(report)) => report,
            Ok(None) => match evaluate_from_build(argv0, options, host) {
                Ok(report) => report,
                Err((message, code)) => return error(host.as_ref(), &message, code),
            },
            Err((message, code)) => return error(host.as_ref(), &message, code),
        }
    } else if let Some(source) = explicit_source {
        let overlay_build = if options.build.is_some() {
            build
        } else {
            source.join("build")
        };
        match evaluate_from_source(argv0, &source, &overlay_build, options, host) {
            Ok(report) => report,
            Err((message, code)) => return error(host.as_ref(), &message, code),
        }
    } else {
        match evaluate_from_build(argv0, options, host) {
            Ok(report) => report,
            Err((message, code)) => return error(host.as_ref(), &message, code),
        }
    };

    if options.json {
        let doc = render_json(&report);
        let mut bytes = rcmake_json::write_compact(&doc);
        bytes.push(b'\n');
        host.write_out(Stream::Stdout, bytes.as_slice());
    } else {
        let color = super::find::use_color(host.as_ref(), options.color);
        let text = render_text(&report, color);
        host.write_out(Stream::Stdout, text.as_bytes());
    }
    0
}

fn cached_source_matches(host: &dyn Host, build: &Path, explicit_source: &Path) -> bool {
    let Ok(saved) = crate::cache::load(host, build) else {
        return false;
    };
    let Some(home) = saved
        .get(b"CMAKE_HOME_DIRECTORY")
        .filter(|entry| !entry.value.is_empty())
    else {
        return false;
    };
    let cached = Lexical::new(&home.value).normalize();
    let given = Lexical::new(&path::from_native_cmake(explicit_source)).normalize();
    if cached == given {
        return true;
    }
    host.canonicalize(&path::to_native(&home.value))
        .ok()
        .zip(host.canonicalize(explicit_source).ok())
        .is_some_and(|(a, b)| a == b)
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.contains(&b'/')
        && !name.contains(&b'\\')
        && !name.contains(&b':')
        && name != b"."
        && name != b".."
}

fn current_index_file(entries: Vec<(BString, FileKind)>) -> Option<(BString, bool)> {
    entries
        .into_iter()
        .filter(|(_, kind)| *kind == FileKind::File)
        .filter_map(|(name, _)| {
            let stem = name.strip_suffix(b".json")?;
            let (suffix, is_error) = if let Some(rest) = stem.strip_prefix(b"index-") {
                (rest, false)
            } else {
                (stem.strip_prefix(b"error-")?, true)
            };
            Some((BString::from(suffix), is_error, name.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, is_error, name)| (name, is_error))
}

fn find_codemodel_file(index: &Value) -> Option<(u64, BString)> {
    let mut best: Option<(u64, BString)> = None;
    let mut consider = |obj: &Value| {
        if obj.get("kind").and_then(Value::as_bytes) != Some(b"codemodel") {
            return;
        }
        let version = obj.get("version");
        let major = version.and_then(|v| v.get("major")).and_then(Value::as_u64);
        let minor = version
            .and_then(|v| v.get("minor"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if major != Some(2) {
            return;
        }
        let Some(file) = obj.get("jsonFile").and_then(Value::as_bytes) else {
            return;
        };
        if !plain_name(file) {
            return;
        }
        if best
            .as_ref()
            .is_none_or(|(prev_minor, _)| minor > *prev_minor)
        {
            best = Some((minor, BString::from(file)));
        }
    };
    if let Some(objects) = index.get("objects").and_then(Value::as_array) {
        for obj in objects {
            consider(obj);
        }
    }
    if let Some(reply) = index.get("reply").and_then(Value::as_object) {
        for value in reply.values() {
            consider(value);
            if let Some(nested) = value.as_object() {
                for inner in nested.values() {
                    consider(inner);
                }
            }
        }
    }
    best
}

fn try_load_fresh_fileapi(
    host: &dyn Host,
    build: &Path,
    options: &TargetsOptions,
) -> Result<Option<TargetsReport>, (String, i32)> {
    let Ok(snapshot) = rcmake_buildinfo::load(host, build, TraceInput::Missing) else {
        return Ok(None);
    };
    if snapshot.evidence(Section::Targets) != Evidence::Complete(DataSource::FileApi) {
        return Ok(None);
    }

    let Some(mut reader) = ArtifactReader::new(host, build, ReadLimits::default()) else {
        return Ok(None);
    };
    let reply_dir = Path::new(REPLY_DIR);
    let Ok(entries) = reader.list_checked(reply_dir, MAX_REPLY_FILES) else {
        return Ok(None);
    };
    let Some((index_name, is_error)) = current_index_file(entries) else {
        return Ok(None);
    };
    if is_error {
        return Ok(None);
    }
    let Ok(index) = reader.json(&reply_dir.join(String::from_utf8_lossy(&index_name).as_ref()))
    else {
        return Ok(None);
    };
    let Some((minor, codemodel_file)) = find_codemodel_file(&index) else {
        return Ok(None);
    };
    // Codemodel < 2.9 omits abstractTargets (which hold both imported targets and
    // project-owned header-only INTERFACE libraries). Always fall back to in-memory
    // overlay evaluation so no targets are silently omitted.
    if minor < 9 {
        return Ok(None);
    }
    let Ok(codemodel) =
        reader.json(&reply_dir.join(String::from_utf8_lossy(&codemodel_file).as_ref()))
    else {
        return Ok(None);
    };
    let Some(configs) = codemodel.get("configurations").and_then(Value::as_array) else {
        return Ok(None);
    };
    let available_configs: Vec<BString> = configs
        .iter()
        .map(|cfg| {
            cfg.get("name")
                .and_then(Value::as_bytes)
                .map_or_else(BString::default, BString::from)
        })
        .collect();
    let selected_index = match select_config_index(&available_configs, options.config.as_ref()) {
        Ok(index) => index,
        Err(message) => return Err((message, 1)),
    };
    let Some(selected_cfg) = configs.get(selected_index) else {
        return Ok(None);
    };
    let selected_name = available_configs
        .get(selected_index)
        .filter(|name| !name.is_empty())
        .cloned();

    let dir_sources: Vec<BString> = selected_cfg
        .get("directories")
        .and_then(Value::as_array)
        .map(|dirs| {
            dirs.iter()
                .map(|dir| {
                    normalize_relative_dir(
                        dir.get("source").and_then(Value::as_bytes).unwrap_or(b"."),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let mut targets = Vec::new();
    let mut seen_count = 0usize;
    for array_key in ["targets", "abstractTargets"] {
        let Some(field) = selected_cfg.get(array_key) else {
            continue;
        };
        let Some(items) = field.as_array() else {
            return Ok(None);
        };
        for item in items {
            seen_count += 1;
            if seen_count > MAX_TARGETS {
                return Ok(None);
            }
            let Some(file) = item.get("jsonFile").and_then(Value::as_bytes) else {
                return Ok(None);
            };
            if !plain_name(file) {
                return Ok(None);
            }
            let Ok(target_doc) =
                reader.json(&reply_dir.join(String::from_utf8_lossy(file).as_ref()))
            else {
                return Ok(None);
            };
            let Some(name) = target_doc.get("name").and_then(Value::as_bytes) else {
                return Ok(None);
            };
            let Some(kind) = target_doc.get("type").and_then(Value::as_bytes) else {
                return Ok(None);
            };
            let dir_from_target = target_doc
                .get("paths")
                .and_then(|paths| paths.get("source"))
                .and_then(Value::as_bytes);
            let dir_from_index = item
                .get("directoryIndex")
                .and_then(Value::as_u64)
                .and_then(|idx| usize::try_from(idx).ok())
                .and_then(|idx| dir_sources.get(idx))
                .map(|s| s.as_slice());
            let Some(raw_dir) = dir_from_target.or(dir_from_index) else {
                return Ok(None);
            };
            let directory = normalize_relative_dir(raw_dir);
            let imported = target_doc
                .get("imported")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let synthetic = kind == b"UTILITY" || kind == b"GLOBAL_TARGET";
            targets.push(TargetEntry {
                name: BString::from(name),
                kind: BString::from(kind),
                directory,
                imported,
                synthetic,
            });
        }
    }

    let source_dir = codemodel
        .get("paths")
        .and_then(|paths| paths.get("source"))
        .and_then(Value::as_bytes)
        .map(BString::from)
        .or(snapshot.source_root)
        .unwrap_or_default();
    let build_dir = codemodel
        .get("paths")
        .and_then(|paths| paths.get("build"))
        .and_then(Value::as_bytes)
        .map(BString::from)
        .unwrap_or_else(|| path::from_native_cmake(build));

    Ok(Some(finalize_report(
        "file-api",
        true,
        source_dir,
        build_dir,
        selected_name,
        options.all,
        targets,
    )))
}

fn evaluate_from_build(
    argv0: Option<&BString>,
    options: &TargetsOptions,
    host: &Arc<dyn Host>,
) -> Result<TargetsReport, (String, i32)> {
    let req = request(options.build.as_ref(), argv0, host, FindTraceLevel::Off)
        .map_err(|message| (message, 2))?;
    evaluate_request(argv0, req, options, host)
}

fn evaluate_from_source(
    argv0: Option<&BString>,
    source: &Path,
    build: &Path,
    options: &TargetsOptions,
    host: &Arc<dyn Host>,
) -> Result<TargetsReport, (String, i32)> {
    let lists = source.join("CMakeLists.txt");
    if !host
        .metadata(&lists, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err((format!("Cannot read {}.", lists.display()), 2));
    }
    let (exe, cmake_root) = rcmake_host::discover_cmake_root(host.as_ref(), argv0);
    let cmake_root = cmake_root.ok_or_else(|| {
        (
            "Cannot find the CMake modules. Set RCMAKE_VENDORED_DATA_ROOT to the CMake data directory."
                .to_owned(),
            2,
        )
    })?;
    let configure_host = Arc::new(ReportHost {
        base: Arc::clone(host),
        exe,
    });
    let req = ConfigureRequest::new(source, build, cmake_root, configure_host)
        .generator(GeneratorKind::Ninja)
        .write_mode(WriteMode::Overlay)
        .process_policy(ProcessPolicy::Deny)
        .services(Services::default());
    evaluate_request(argv0, req, options, host)
}

fn evaluate_request(
    argv0: Option<&BString>,
    req: ConfigureRequest,
    options: &TargetsOptions,
    host: &Arc<dyn Host>,
) -> Result<TargetsReport, (String, i32)> {
    let build_dir = path::from_native_cmake(&req.build_dir);
    let result = match run_configure(req) {
        Ok(result) => result,
        Err(err) => {
            let code = if matches!(err, ApiError::InvalidConfig { .. }) {
                2
            } else {
                1
            };
            return Err((err.to_string(), code));
        }
    };
    super::report_partial(host.as_ref(), &result, false);
    if matches!(
        result.completion,
        CompletionState::Partial {
            cancelled: true,
            ..
        }
    ) {
        return Err(("Configure was cancelled.".to_owned(), 1));
    }
    let provisional = result.completion.provisional_outside(
        super::module_root(host.as_ref(), argv0)
            .as_deref()
            .map(Vec::as_slice),
    );
    let complete = !provisional;

    let available_configs = extract_configs(&result);
    let selected_index = select_config_index(&available_configs, options.config.as_ref())
        .map_err(|message| (message, 1))?;
    let selected_name = available_configs
        .get(selected_index)
        .filter(|name| !name.is_empty())
        .cloned();

    let root_source = result.model.dir(DirId::ROOT).source_dir.clone();
    let mut targets = Vec::new();
    for (_, target) in result.model.targets() {
        let dir_source = &result.model.dir(target.dir).source_dir;
        let directory = relative_source_dir(&root_source, dir_source);
        targets.push(TargetEntry {
            name: target.name.clone(),
            kind: BString::from(target.kind.as_str()),
            directory,
            imported: target.is_imported(),
            synthetic: matches!(target.kind, TargetKind::Utility | TargetKind::GlobalTarget),
        });
    }

    Ok(finalize_report(
        "reevaluated",
        complete,
        root_source,
        build_dir,
        selected_name,
        options.all,
        targets,
    ))
}

fn extract_configs(result: &rcmake_api::ConfigureResult) -> Vec<BString> {
    if let Some(generation) = &result.generation
        && !generation.configs.is_empty()
    {
        return generation.configs.iter().map(|cfg| cfg.0.clone()).collect();
    }
    let cache = result.model.cache();
    let multi = result
        .model
        .generator
        .is_some_and(GeneratorKind::multi_config);
    if multi {
        if let Some(types) = cache
            .get(b"CMAKE_CONFIGURATION_TYPES")
            .filter(|entry| !entry.value.is_empty())
        {
            let items = list::split(&types.value, list::Empty::Drop);
            if !items.is_empty() {
                return items;
            }
        }
        return ["Debug", "Release", "RelWithDebInfo", "MinSizeRel"]
            .map(BString::from)
            .to_vec();
    }
    vec![
        cache
            .get(b"CMAKE_BUILD_TYPE")
            .map(|entry| entry.value.clone())
            .unwrap_or_default(),
    ]
}

fn select_config_index(
    available: &[BString],
    requested: Option<&BString>,
) -> Result<usize, String> {
    let Some(requested) = requested else {
        return Ok(0);
    };
    if let Some(index) = available
        .iter()
        .position(|cfg| cfg.eq_ignore_ascii_case(requested))
    {
        return Ok(index);
    }
    let requested = String::from_utf8_lossy(requested);
    let message = match available {
        [only] if !only.is_empty() => format!(
            "This build directory is configured for {only} only. Reconfigure with -DCMAKE_BUILD_TYPE={requested}, or use a multi-config generator."
        ),
        [_] => format!(
            "This build directory is configured for (default) only. Reconfigure with -DCMAKE_BUILD_TYPE={requested}, or use a multi-config generator."
        ),
        [] => format!(
            "Configuration {requested} is unavailable because no configurations were resolved."
        ),
        many => {
            let names = many
                .iter()
                .map(|cfg| {
                    if cfg.is_empty() {
                        "(default)".to_owned()
                    } else {
                        cfg.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("Configuration {requested} is not one of: {names}.")
        }
    };
    Err(message)
}

fn normalize_relative_dir(raw: &[u8]) -> BString {
    if raw.is_empty() || raw == b"." {
        return BString::from(".");
    }
    let normalized = Lexical::new(raw).normalize();
    if normalized.is_empty() || normalized.as_slice() == b"." {
        BString::from(".")
    } else {
        normalized
    }
}

fn relative_source_dir(root_source: &[u8], target_dir: &[u8]) -> BString {
    let root = Lexical::new(root_source).normalize();
    let dir = Lexical::new(target_dir).normalize();
    if root == dir {
        return BString::from(".");
    }
    Lexical::new(&dir)
        .relative(Lexical::new(&root))
        .map(|rel| normalize_relative_dir(rel.as_slice()))
        .unwrap_or(dir)
}

fn finalize_report(
    source: &'static str,
    complete: bool,
    source_dir: BString,
    build_dir: BString,
    config: Option<BString>,
    all: bool,
    mut targets: Vec<TargetEntry>,
) -> TargetsReport {
    let total_count = targets.len();
    if !all {
        targets.retain(|target| !target.imported && !target.synthetic);
    }
    targets.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.directory.cmp(&b.directory))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.imported.cmp(&b.imported))
    });
    let mut seen = BTreeSet::new();
    targets.retain(|t| {
        seen.insert((
            t.name.clone(),
            t.directory.clone(),
            t.kind.clone(),
            t.imported,
        ))
    });
    TargetsReport {
        source,
        complete,
        source_dir,
        build_dir,
        config,
        all,
        targets,
        total_count,
    }
}

fn render_text(report: &TargetsReport, color: bool) -> String {
    if report.targets.is_empty() {
        return if report.total_count > 0 && !report.all {
            "No build targets found (pass --all to include imported and utility targets).\n"
                .to_owned()
        } else {
            "No targets found.\n".to_owned()
        };
    }
    let name_width = report
        .targets
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let kind_width = report
        .targets
        .iter()
        .map(|t| t.kind.len())
        .max()
        .unwrap_or(0)
        .max("TYPE".len());

    let mut out = String::new();
    if color {
        out.push_str(&format!(
            "\x1b[1m{:<name_width$}  {:<kind_width$}  DIRECTORY\x1b[0m\n",
            "NAME", "TYPE"
        ));
    } else {
        out.push_str(&format!(
            "{:<name_width$}  {:<kind_width$}  DIRECTORY\n",
            "NAME", "TYPE"
        ));
    }
    for target in &report.targets {
        let name = String::from_utf8_lossy(&target.name);
        let kind = String::from_utf8_lossy(&target.kind);
        out.push_str(&format!(
            "{name:<name_width$}  {kind:<kind_width$}  {}\n",
            target.directory
        ));
    }
    out
}

fn render_json(report: &TargetsReport) -> Value {
    let mut root = BTreeMap::new();
    root.insert(BString::from("schema"), Value::string(SCHEMA));
    root.insert(BString::from("source"), Value::string(report.source));
    root.insert(BString::from("complete"), Value::Bool(report.complete));
    root.insert(
        BString::from("sourceDir"),
        Value::string(report.source_dir.clone()),
    );
    root.insert(
        BString::from("buildDir"),
        Value::string(report.build_dir.clone()),
    );
    if let Some(config) = &report.config {
        root.insert(BString::from("config"), Value::string(config.clone()));
    }
    root.insert(BString::from("all"), Value::Bool(report.all));
    let items = report
        .targets
        .iter()
        .map(|target| {
            let mut obj = BTreeMap::new();
            obj.insert(BString::from("name"), Value::string(target.name.clone()));
            obj.insert(BString::from("type"), Value::string(target.kind.clone()));
            obj.insert(
                BString::from("directory"),
                Value::string(target.directory.clone()),
            );
            obj.insert(BString::from("imported"), Value::Bool(target.imported));
            Value::Object(obj)
        })
        .collect();
    root.insert(BString::from("targets"), Value::Array(items));
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rcmake_host::{PureHost, WriteMode as HostWriteMode};

    use super::*;

    fn seed_multi_target_project(host: &PureHost) {
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            br#"cmake_minimum_required(VERSION 4.5)
set(CMAKE_SYSTEM Generic)
set(CMAKE_SYSTEM_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)
project(TargetsTest LANGUAGES NONE)
add_library(api INTERFACE)
add_library(ext_dep INTERFACE IMPORTED)
add_custom_target(docs)
add_subdirectory(sub)
"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/src/sub/CMakeLists.txt"),
            b"add_library(sublib INTERFACE)\n",
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/data/Modules/CMakeDetermineSystem.cmake"),
            b"",
        )
        .unwrap();
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));
    }

    #[test]
    fn evaluates_targets_in_memory_and_filters_imported_and_utility_by_default() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        seed_multi_target_project(&host);
        host.seed_file(
            Path::new("/work/out/CMakeCache.txt"),
            b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\nCMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_BUILD_TYPE:STRING=Debug\n",
        )
        .unwrap();

        let shared: Arc<dyn Host> = host.clone();
        let code = crate::explain_cmd::run(
            None,
            &["--targets".into(), "-B".into(), "out".into()],
            shared.clone(),
        );
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        assert!(stdout.contains("NAME"), "{stdout}");
        assert!(stdout.contains("api"), "{stdout}");
        assert!(stdout.contains("INTERFACE_LIBRARY"), "{stdout}");
        assert!(stdout.contains("sublib"), "{stdout}");
        assert!(stdout.contains("sub"), "{stdout}");
        assert!(
            !stdout.contains("ext_dep"),
            "imported hidden by default: {stdout}"
        );
        assert!(
            !stdout.contains("docs"),
            "utility hidden by default: {stdout}"
        );

        // `--all` + `--json` includes imported and utility targets.
        let host2 = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        seed_multi_target_project(&host2);
        host2
            .seed_file(
                Path::new("/work/out/CMakeCache.txt"),
                b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\nCMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_BUILD_TYPE:STRING=Debug\n",
            )
            .unwrap();
        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-B".into(),
                "out".into(),
                "--all".into(),
                "--json".into(),
            ],
            host2.clone(),
        );
        assert_eq!(code, 0, "{}", host2.stream_bytes(Stream::Stderr));
        let json_text = host2.stream_bytes(Stream::Stdout);
        let parsed = rcmake_json::parse(&json_text, &rcmake_json::ReadOptions::default())
            .expect("valid json");
        assert_eq!(parsed.get("schema").and_then(Value::as_str), Some(SCHEMA));
        assert_eq!(
            parsed.get("source").and_then(Value::as_str),
            Some("reevaluated")
        );
        let targets = parsed
            .get("targets")
            .and_then(Value::as_array)
            .expect("targets");
        let names: Vec<&str> = targets
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"api"), "{names:?}");
        assert!(names.contains(&"sublib"), "{names:?}");
        assert!(names.contains(&"ext_dep"), "{names:?}");
        assert!(names.contains(&"docs"), "{names:?}");
    }

    #[test]
    fn reads_fresh_fileapi_codemodel_without_reevaluating() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        host.seed_file(
            Path::new("/work/out/CMakeCache.txt"),
            b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\nCMAKE_GENERATOR:INTERNAL=Ninja Multi-Config\nCMAKE_CONFIGURATION_TYPES:STRING=Debug;Release\n",
        )
        .unwrap();
        let reply = Path::new("/work/out/.cmake/api/v1/reply");
        host.create_dir_all(reply).unwrap();
        host.write(
            &reply.join("index-2026-01-01T00-00-00-0000.json"),
            br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":12},"jsonFile":"codemodel-v2-abc.json"}]}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("codemodel-v2-abc.json"),
            br#"{
  "version": { "major": 2, "minor": 12 },
  "paths": { "source": "/work/src", "build": "/work/out" },
  "configurations": [
    {
      "name": "Debug",
      "directories": [{ "source": "." }, { "source": "src/app" }],
      "targets": [
        { "name": "demo", "directoryIndex": 1, "jsonFile": "target-demo-Debug.json" },
        { "name": "gen_docs", "directoryIndex": 0, "jsonFile": "target-gen_docs-Debug.json" }
      ],
      "abstractTargets": [
        { "name": "Pkg::Lib", "directoryIndex": 0, "jsonFile": "target-pkg-Debug.json" }
      ]
    },
    {
      "name": "Release",
      "directories": [{ "source": "." }, { "source": "src/app" }],
      "targets": [
        { "name": "demo", "directoryIndex": 1, "jsonFile": "target-demo-Release.json" }
      ]
    }
  ]
}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("target-demo-Debug.json"),
            br#"{"name":"demo","type":"EXECUTABLE","paths":{"source":"src/app","build":"src/app"}}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("target-demo-Release.json"),
            br#"{"name":"demo","type":"EXECUTABLE","paths":{"source":"src/app","build":"src/app"}}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("target-gen_docs-Debug.json"),
            br#"{"name":"gen_docs","type":"UTILITY","paths":{"source":".","build":"."}}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("target-pkg-Debug.json"),
            br#"{"name":"Pkg::Lib","type":"INTERFACE_LIBRARY","imported":true,"paths":{"source":".","build":"."}}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();

        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-B".into(),
                "out".into(),
                "--config".into(),
                "Release".into(),
                "--json".into(),
            ],
            host.clone(),
        );
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let parsed = rcmake_json::parse(
            &host.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )
        .expect("valid json");
        assert_eq!(
            parsed.get("source").and_then(Value::as_str),
            Some("file-api")
        );
        assert_eq!(
            parsed.get("config").and_then(Value::as_str),
            Some("Release")
        );
        let targets = parsed
            .get("targets")
            .and_then(Value::as_array)
            .expect("targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].get("name").and_then(Value::as_str), Some("demo"));
        assert_eq!(
            targets[0].get("type").and_then(Value::as_str),
            Some("EXECUTABLE")
        );
        assert_eq!(
            targets[0].get("directory").and_then(Value::as_str),
            Some("src/app")
        );

        // Unavailable config exits 1 cleanly.
        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-B".into(),
                "out".into(),
                "--config".into(),
                "MinSizeRel".into(),
            ],
            host.clone(),
        );
        assert_eq!(code, 1);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains("Configuration MinSizeRel is not one of: Debug, Release.")
        );
    }

    #[test]
    fn evaluates_unconfigured_source_dir_with_dash_s_without_writing() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        seed_multi_target_project(&host);

        let code = crate::explain_cmd::run(
            None,
            &["--targets".into(), "-S".into(), "src".into()],
            host.clone(),
        );
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        assert!(stdout.contains("api"), "{stdout}");
        assert!(stdout.contains("sublib"), "{stdout}");
        assert!(
            host.metadata(Path::new("/work/src/build"), Follow::Yes)
                .is_err(),
            "overlay evaluation must not write to disk"
        );
    }

    #[test]
    fn codemodel_below_2_9_and_malformed_entries_fall_back_to_overlay_evaluation() {
        // 1. Codemodel 2.8 (without --all) falls back so header-only INTERFACE targets are not lost.
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        seed_multi_target_project(&host);
        host.seed_file(
            Path::new("/work/out/CMakeCache.txt"),
            b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\nCMAKE_GENERATOR:INTERNAL=Ninja\n",
        )
        .unwrap();
        let reply = Path::new("/work/out/.cmake/api/v1/reply");
        host.create_dir_all(reply).unwrap();
        host.write(
            &reply.join("index-2026-01-01T00-00-00-0000.json"),
            br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":8},"jsonFile":"codemodel-v2-old.json"}]}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();
        host.write(
            &reply.join("codemodel-v2-old.json"),
            br#"{"version":{"major":2,"minor":8},"paths":{"source":"/work/src","build":"/work/out"},"configurations":[{"name":"","directories":[{"source":"."}],"targets":[]}]}"#,
            HostWriteMode::Truncate,
        )
        .unwrap();

        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-B".into(),
                "out".into(),
                "--json".into(),
            ],
            host.clone(),
        );
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let parsed = rcmake_json::parse(
            &host.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )
        .expect("valid json");
        assert_eq!(
            parsed.get("source").and_then(Value::as_str),
            Some("reevaluated")
        );
        assert_eq!(parsed.get("complete").and_then(Value::as_bool), Some(true));
        let names: Vec<&str> = parsed
            .get("targets")
            .and_then(Value::as_array)
            .expect("targets")
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"api"), "{names:?}");
        assert!(names.contains(&"sublib"), "{names:?}");

        // 2. Malformed File API target entry (missing `type`) falls back instead of dropping targets.
        let host2 = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        seed_multi_target_project(&host2);
        host2
            .seed_file(
                Path::new("/work/out/CMakeCache.txt"),
                b"CMAKE_HOME_DIRECTORY:INTERNAL=/work/src\nCMAKE_GENERATOR:INTERNAL=Ninja\n",
            )
            .unwrap();
        host2.create_dir_all(reply).unwrap();
        host2
            .write(
                &reply.join("index-2026-01-01T00-00-00-0000.json"),
                br#"{"objects":[{"kind":"codemodel","version":{"major":2,"minor":12},"jsonFile":"codemodel-v2-bad.json"}]}"#,
                HostWriteMode::Truncate,
            )
            .unwrap();
        host2
            .write(
                &reply.join("codemodel-v2-bad.json"),
                br#"{"version":{"major":2,"minor":12},"paths":{"source":"/work/src","build":"/work/out"},"configurations":[{"name":"","directories":[{"source":"."}],"targets":[{"name":"broken","directoryIndex":0,"jsonFile":"target-broken.json"}]}]}"#,
                HostWriteMode::Truncate,
            )
            .unwrap();
        host2
            .write(
                &reply.join("target-broken.json"),
                br#"{"name":"broken","paths":{"source":".","build":"."}}"#,
                HostWriteMode::Truncate,
            )
            .unwrap();

        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-B".into(),
                "out".into(),
                "--json".into(),
            ],
            host2.clone(),
        );
        assert_eq!(code, 0, "{}", host2.stream_bytes(Stream::Stderr));
        let parsed = rcmake_json::parse(
            &host2.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )
        .expect("valid json");
        assert_eq!(
            parsed.get("source").and_then(Value::as_str),
            Some("reevaluated")
        );
    }

    #[test]
    fn json_reports_complete_false_when_overlay_denies_project_subprocess() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            br#"cmake_minimum_required(VERSION 4.5)
set(CMAKE_SYSTEM Generic)
set(CMAKE_SYSTEM_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)
set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)
project(PartialTargets LANGUAGES NONE)
execute_process(COMMAND git rev-parse HEAD RESULT_VARIABLE rc)
add_library(app INTERFACE)
"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/data/Modules/CMakeDetermineSystem.cmake"),
            b"",
        )
        .unwrap();
        host.env_set(b"RCMAKE_VENDORED_DATA_ROOT", Some(b"data"));

        let code = crate::explain_cmd::run(
            None,
            &[
                "--targets".into(),
                "-S".into(),
                "src".into(),
                "--json".into(),
            ],
            host.clone(),
        );
        assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let parsed = rcmake_json::parse(
            &host.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )
        .expect("valid json");
        assert_eq!(parsed.get("complete").and_then(Value::as_bool), Some(false));
    }
}
