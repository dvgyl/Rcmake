//! Native `rcmake preset show` subcommand.
//!
//! Resolves presets through [`rcmake_presets`] without running configure or
//! writing to the filesystem.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{
    BString,
    path_lex::Lexical,
    suggest::{Threshold, closest},
};
use rcmake_host::{Host, Stream, path};
use rcmake_json::Value;
use rcmake_presets::{
    BuildSettings, ConfigureErrors, ConfigureSettings, ConfigureWarnings, EnvSnapshot,
    GeneratorSelection, PackageSettings, PresetInspection, PresetKind, PresetSettings, Presets,
    TestIndex, TestJobs, TestSettings, WorkflowSettings,
};

use crate::resolve_cli_path;

const SCHEMA: &str = "rcmake-preset-show-v1";

const ALL_KINDS: [PresetKind; 5] = [
    PresetKind::Configure,
    PresetKind::Build,
    PresetKind::Test,
    PresetKind::Package,
    PresetKind::Workflow,
];

const HELP: &str = "\
Usage: rcmake preset show <name> [--kind configure|build|test|package|workflow]
                          [--json] [--show-secrets] [-S <source-dir>]

Inspect a fully resolved CMake preset without configuring or writing files.

Applies include chains, inherits merges, macro expansion (${sourceDir},
${presetName}, etc.), $env{} expansion, and condition evaluation. Disabled
presets are shown explicitly as disabled rather than hidden. Sensitive
environment and cache variable values are redacted by default.

Options
  --kind <kind>    Select one preset category: configure, build, test,
                   package, or workflow. When omitted, all matching kinds are
                   shown in order: configure, build, test, package, workflow.
  -S <source-dir>  Read presets from this directory. Default: current directory.
  --json           Write one JSON report to standard output.
  --show-secrets   Show unredacted values for sensitive env and cache keys.
  -h, --help       Show this help.

Examples
  rcmake preset show debug
  rcmake preset show ci --kind configure --json
  rcmake preset show default -S path/to/project
";

const REDACTED: &str = "<redacted>";

#[derive(Debug, Default, PartialEq, Eq)]
struct ShowOptions {
    name: BString,
    kind: Option<PresetKind>,
    source: Option<BString>,
    json: bool,
    show_secrets: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Show(ShowOptions),
}

fn parse_kind(raw: &[u8]) -> Result<PresetKind, String> {
    ALL_KINDS
        .into_iter()
        .find(|kind| kind.label().as_bytes() == raw)
        .ok_or_else(|| {
            format!(
                "Invalid value for --kind: \"{}\". Expected one of: configure, build, test, package, workflow.",
                String::from_utf8_lossy(raw)
            )
        })
}

fn flag_name(raw: &[u8]) -> (&[u8], Option<&[u8]>) {
    if raw.starts_with(b"--") {
        let mut parts = raw.splitn(2, |byte| *byte == b'=');
        (parts.next().unwrap_or(raw), parts.next())
    } else if let Some(rest) = raw.strip_prefix(b"-S=") {
        (b"-S", Some(rest))
    } else if raw.len() > 2 && raw.starts_with(b"-S") {
        (b"-S", raw.get(2..))
    } else {
        (raw, None)
    }
}

fn take_value<'a>(
    args: &mut impl Iterator<Item = &'a BString>,
    attached: Option<&[u8]>,
    flag: &str,
) -> Result<BString, String> {
    let raw = match attached {
        Some(value) => value,
        None => match args.next().map(|s| s.as_slice()) {
            Some(next) if !next.is_empty() && !next.starts_with(b"-") => next,
            _ => return Err(format!("Supply a value for {flag}.")),
        },
    };
    if raw.is_empty() {
        return Err(format!("Supply a value for {flag}."));
    }
    Ok(BString::from(raw))
}

fn parse(args: &[BString]) -> Result<Invocation, String> {
    let Some((first, rest)) = args.split_first() else {
        return Err("Specify a subcommand: rcmake preset show <name>.".into());
    };
    if matches!(first.as_slice(), b"-h" | b"--help") {
        return Ok(Invocation::Help);
    }
    if first.as_slice() != b"show" {
        return Err(format!(
            "Unknown preset subcommand \"{}\". Expected \"show\".",
            String::from_utf8_lossy(first)
        ));
    }
    for token in rest.iter().take_while(|arg| arg.as_slice() != b"--") {
        if matches!(token.as_slice(), b"-h" | b"--help") {
            return Ok(Invocation::Help);
        }
    }

    let mut options = ShowOptions::default();
    let mut positionals = Vec::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let raw = arg.as_slice();
        if raw == b"--" {
            positionals.extend(iter.cloned());
            break;
        }
        let (flag, attached) = flag_name(raw);
        match flag {
            b"--json" => {
                if attached.is_some() {
                    return Err("--json takes no value.".into());
                }
                options.json = true;
            }
            b"--show-secrets" => {
                if attached.is_some() {
                    return Err("--show-secrets takes no value.".into());
                }
                options.show_secrets = true;
            }
            b"--kind" => {
                let value = take_value(&mut iter, attached, "--kind")?;
                options.kind = Some(parse_kind(value.as_slice())?);
            }
            b"-S" | b"--source" => {
                let label = if flag == b"-S" { "-S" } else { "--source" };
                options.source = Some(take_value(&mut iter, attached, label)?);
            }
            _ if raw.starts_with(b"-") => {
                return Err(format!(
                    "Unknown option \"{}\".",
                    String::from_utf8_lossy(raw)
                ));
            }
            _ => {
                if raw.is_empty() {
                    return Err("Specify a nonempty preset name.".into());
                }
                positionals.push(arg.clone());
            }
        }
    }

    match positionals.as_slice() {
        [] => Err("Specify a preset name: rcmake preset show <name>.".into()),
        [name] => {
            options.name = name.clone();
            Ok(Invocation::Show(options))
        }
        _ => Err("Specify only one preset name.".into()),
    }
}

/// Run `rcmake preset ...` after the Native dispatcher consumes `preset`.
pub(crate) fn run(args: &[BString], host: &dyn Host) -> i32 {
    let options = match parse(args) {
        Ok(Invocation::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(Invocation::Show(options)) => options,
        Err(message) => {
            host.write_out(
                Stream::Stderr,
                format!("rcmake preset: {message} Run `rcmake preset --help`.\n").as_bytes(),
            );
            return 1;
        }
    };

    let source_dir = options.source.as_ref().map_or_else(
        || host.initial_cwd(),
        |dir| resolve_cli_path(host, dir.as_slice()),
    );
    let presets = match rcmake_presets::load(&source_dir, host) {
        Ok(presets) => presets,
        Err(err) => {
            host.write_out(
                Stream::Stderr,
                format!("rcmake preset: {}\n", err.message).as_bytes(),
            );
            return 1;
        }
    };

    let env = EnvSnapshot::from_host(host);
    let kinds: &[PresetKind] = match &options.kind {
        Some(kind) => std::slice::from_ref(kind),
        None => &ALL_KINDS,
    };

    let mut matches = Vec::new();
    let mut errors: Vec<(PresetKind, String)> = Vec::new();
    for &kind in kinds {
        match presets.inspect(kind, options.name.as_slice(), &env) {
            Ok(Some(inspection)) => matches.push(inspection),
            Ok(None) => {}
            Err(err) => errors.push((kind, err.message)),
        }
    }

    if matches.is_empty() && errors.is_empty() {
        let source_display = path::from_native_cmake(&source_dir);
        let message = match options.kind {
            Some(kind) => format!(
                "rcmake preset: No such {} preset in {source_display}: \"{}\"\n",
                kind.label(),
                options.name
            ),
            None => format!(
                "rcmake preset: No preset named \"{}\" found in {source_display}\n",
                options.name
            ),
        };
        host.write_out(Stream::Stderr, message.as_bytes());
        if let Some(suggestion) = suggest_preset_name(&presets, kinds, options.name.as_slice()) {
            host.write_out(
                Stream::Stderr,
                format!("hint: Did you mean \"{suggestion}\"?\n").as_bytes(),
            );
        }
        return 1;
    }

    for (_, message) in &errors {
        host.write_out(
            Stream::Stderr,
            format!("rcmake preset: {message}\n").as_bytes(),
        );
    }

    if !matches.is_empty() {
        if options.json {
            let doc = render_json(&source_dir, &matches, &errors, options.show_secrets);
            let mut text = rcmake_json::write_styled(&doc, "  ");
            text.push(b'\n');
            host.write_out(Stream::Stdout, text.as_slice());
        } else {
            let text = render_text(&source_dir, &matches, options.show_secrets);
            host.write_out(Stream::Stdout, text.as_bytes());
        }
    }

    if errors.is_empty() { 0 } else { 1 }
}

fn suggest_preset_name(presets: &Presets, kinds: &[PresetKind], input: &[u8]) -> Option<BString> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    // Prefer non-hidden presets first so declaration order breaks ties toward runnable presets.
    for &kind in kinds {
        for summary in presets.list_defined(kind) {
            if seen.insert(summary.name.clone()) {
                candidates.push(summary.name.clone());
            }
        }
    }
    for &kind in kinds {
        for summary in presets.list_all(kind) {
            if seen.insert(summary.name.clone()) {
                candidates.push(summary.name.clone());
            }
        }
    }
    closest(input, &candidates, Threshold::CmakeCommand)
        .filter(|best| best.as_slice() != input)
        .cloned()
}

fn display_source_file(file: &Path, source_dir: &Path) -> BString {
    let file_cmake = Lexical::new(&path::from_native_cmake(file)).normalize();
    let root_cmake = Lexical::new(&path::from_native_cmake(source_dir)).normalize();
    Lexical::new(&file_cmake)
        .relative(Lexical::new(&root_cmake))
        .filter(|rel| !rel.is_empty() && rel.as_slice() != b".." && !rel.starts_with(b"../"))
        .unwrap_or(file_cmake)
}

fn is_sensitive_key(name: &[u8]) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let bytes = name;
    for (idx, &byte) in bytes.iter().enumerate() {
        if !byte.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        let is_upper = byte.is_ascii_uppercase();
        let prev_lower = idx
            .checked_sub(1)
            .and_then(|i| bytes.get(i))
            .is_some_and(u8::is_ascii_lowercase);
        let prev_upper = idx
            .checked_sub(1)
            .and_then(|i| bytes.get(i))
            .is_some_and(u8::is_ascii_uppercase);
        let next_lower = bytes.get(idx + 1).is_some_and(u8::is_ascii_lowercase);
        if is_upper && !current.is_empty() && (prev_lower || (prev_upper && next_lower)) {
            words.push(std::mem::take(&mut current));
        }
        current.push(char::from(byte.to_ascii_lowercase()));
    }
    if !current.is_empty() {
        words.push(current);
    }

    for word in &words {
        if matches!(
            word.as_str(),
            "token"
                | "tokens"
                | "secret"
                | "secrets"
                | "password"
                | "passwords"
                | "passwd"
                | "passphrase"
                | "auth"
                | "authorization"
                | "credential"
                | "credentials"
                | "creds"
                | "cookie"
                | "cookies"
                | "apikey"
                | "privatekey"
                | "accesskey"
                | "signingkey"
                | "bearer"
                | "jwt"
                | "pat"
                | "oauth"
        ) {
            return true;
        }
    }
    for pair in words.windows(2) {
        if let [first, second] = pair
            && matches!(
                (first.as_str(), second.as_str()),
                ("api", "key" | "keys")
                    | ("private", "key" | "keys")
                    | ("access", "key" | "keys")
                    | ("signing", "key" | "keys")
                    | ("client", "key" | "keys")
                    | ("master", "key" | "keys")
                    | ("encryption", "key" | "keys")
                    | ("ssh", "key" | "keys")
                    | ("gpg", "key" | "keys")
                    | ("tls", "key" | "keys")
                    | ("ssl", "key" | "keys")
            )
        {
            return true;
        }
    }
    false
}

fn redacted_value(key: &[u8], value: &BString, show_secrets: bool) -> BString {
    if !show_secrets && is_sensitive_key(key) {
        BString::from(REDACTED)
    } else {
        value.clone()
    }
}

fn render_text(source_dir: &Path, matches: &[PresetInspection], show_secrets: bool) -> String {
    let mut out = String::new();
    for (index, item) in matches.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let preset = &item.preset;
        let status = if item.enabled { "enabled" } else { "disabled" };
        let tags = if item.hidden {
            format!("{status}, hidden")
        } else {
            status.to_owned()
        };
        out.push_str(&format!(
            "Preset: {} ({}) [{tags}]\n",
            preset.name,
            preset.kind.label()
        ));
        out.push_str(&format!(
            "  Source: {}\n",
            display_source_file(&item.source_file, source_dir)
        ));
        if let Some(display) = preset.display_name.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("  Display name: {display}\n"));
        }
        if let Some(desc) = preset.description.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("  Description: {desc}\n"));
        }
        if !item.inherits.is_empty() {
            let parents = item
                .inherits
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("  Inherits: {parents}\n"));
        }
        if let Some(condition) = &item.condition {
            let cond_json = rcmake_json::write_compact(condition);
            out.push_str(&format!("  Condition: {cond_json} ({status})\n"));
        }
        if let Some(cfg) = preset.configure_preset.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("  Configure preset: {cfg}\n"));
        }
        if let Some(inherit_env) = preset.inherit_configure_environment {
            out.push_str(&format!("  Inherit configure env: {inherit_env}\n"));
        }
        if let Some(generator) = preset.generator.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("  Generator: {generator}\n"));
        }
        if let Some(binary_dir) = preset.binary_dir.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("  Binary dir: {binary_dir}\n"));
        }

        match &preset.settings {
            PresetSettings::Configure(cfg) => render_configure_text(&mut out, cfg, show_secrets),
            PresetSettings::Build(build) => render_build_text(&mut out, build),
            PresetSettings::Test(test) => render_test_text(&mut out, test),
            PresetSettings::Package(pkg) => render_package_text(&mut out, pkg, show_secrets),
            PresetSettings::Workflow(workflow) => render_workflow_text(&mut out, workflow),
        }

        if !preset.environment.is_empty() {
            out.push_str("  Environment:\n");
            for (key, value) in &preset.environment {
                match value {
                    Some(val) => {
                        let shown = redacted_value(key.as_slice(), val, show_secrets);
                        out.push_str(&format!("    {key}={shown}\n"));
                    }
                    None => out.push_str(&format!("    {key}=<unset>\n")),
                }
            }
        }
    }
    out
}

fn format_selection(sel: &GeneratorSelection) -> String {
    let val = sel
        .value
        .as_ref()
        .map_or_else(String::new, ToString::to_string);
    match sel.strategy {
        Some(strategy) if !val.is_empty() => format!("{val} (strategy: {})", strategy.as_str()),
        Some(strategy) => format!("(strategy: {})", strategy.as_str()),
        None => val,
    }
}

fn render_configure_text(out: &mut String, cfg: &ConfigureSettings, show_secrets: bool) {
    if let Some(arch) = &cfg.architecture {
        let text = format_selection(arch);
        if !text.is_empty() {
            out.push_str(&format!("  Architecture: {text}\n"));
        }
    }
    if let Some(toolset) = &cfg.toolset {
        let text = format_selection(toolset);
        if !text.is_empty() {
            out.push_str(&format!("  Toolset: {text}\n"));
        }
    }
    if let Some(install_dir) = cfg.install_dir.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Install dir: {install_dir}\n"));
    }
    if let Some(toolchain) = cfg.toolchain_file.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Toolchain file: {toolchain}\n"));
    }
    if let Some(graphviz) = cfg.graphviz.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Graphviz: {graphviz}\n"));
    }
    if let Some(exe) = cfg.cmake_executable.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  CMake executable: {exe}\n"));
    }
    if let Some(vars) = cfg.cache_variables.as_ref().filter(|m| !m.is_empty()) {
        out.push_str("  Cache variables:\n");
        for (name, var) in vars {
            let shown = redacted_value(name.as_slice(), &var.value, show_secrets);
            match &var.kind {
                Some(kind) => out.push_str(&format!("    {name}:{kind}={shown}\n")),
                None => out.push_str(&format!("    {name}={shown}\n")),
            }
        }
    }
    if let Some(warnings) = &cfg.warnings {
        let items = warning_pairs(warnings);
        if !items.is_empty() {
            out.push_str(&format!("  Warnings: {}\n", items.join(", ")));
        }
    }
    if let Some(errors) = &cfg.errors {
        let items = error_pairs(errors);
        if !items.is_empty() {
            out.push_str(&format!("  Errors: {}\n", items.join(", ")));
        }
    }
    if let Some(debug) = &cfg.debug {
        let mut items = Vec::new();
        for (name, val) in [
            ("output", debug.output),
            ("tryCompile", debug.try_compile),
            ("find", debug.find),
        ] {
            if let Some(flag) = val {
                items.push(format!("{name}={flag}"));
            }
        }
        if !items.is_empty() {
            out.push_str(&format!("  Debug: {}\n", items.join(", ")));
        }
    }
    if let Some(trace) = &cfg.trace {
        let mut items = Vec::new();
        if let Some(mode) = trace.mode {
            items.push(format!("mode={}", mode.as_str()));
        }
        if let Some(fmt) = trace.format {
            items.push(format!("format={}", fmt.as_str()));
        }
        if let Some(sources) = trace.source.as_ref().filter(|s| !s.is_empty()) {
            let joined = sources
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            items.push(format!("source=[{joined}]"));
        }
        if let Some(redirect) = trace.redirect.as_ref().filter(|s| !s.is_empty()) {
            items.push(format!("redirect={redirect}"));
        }
        if !items.is_empty() {
            out.push_str(&format!("  Trace: {}\n", items.join(", ")));
        }
    }
}

fn warning_pairs(w: &ConfigureWarnings) -> Vec<String> {
    let mut items = Vec::new();
    for (name, val) in [
        ("author", w.author),
        ("deprecated", w.deprecated),
        ("experimental", w.experimental),
        ("installAbsoluteDestination", w.install_absolute_destination),
        ("nonTargetDirective", w.non_target_directive),
        ("policy", w.policy),
        ("strict", w.strict),
        ("uninitialized", w.uninitialized),
        ("unusedCli", w.unused_cli),
        ("systemVars", w.system_vars),
    ] {
        if let Some(flag) = val {
            items.push(format!("{name}={flag}"));
        }
    }
    items
}

fn error_pairs(e: &ConfigureErrors) -> Vec<String> {
    let mut items = Vec::new();
    for (name, val) in [
        ("author", e.author),
        ("deprecated", e.deprecated),
        ("experimental", e.experimental),
        ("installAbsoluteDestination", e.install_absolute_destination),
        ("nonTargetDirective", e.non_target_directive),
        ("policy", e.policy),
        ("strict", e.strict),
        ("uninitialized", e.uninitialized),
        ("unusedCli", e.unused_cli),
    ] {
        if let Some(flag) = val {
            items.push(format!("{name}={flag}"));
        }
    }
    items
}

fn render_build_text(out: &mut String, build: &BuildSettings) {
    if let Some(config) = build.configuration.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Configuration: {config}\n"));
    }
    if let Some(jobs) = build.jobs {
        out.push_str(&format!("  Jobs: {jobs}\n"));
    }
    if let Some(targets) = build.targets.as_ref().filter(|v| !v.is_empty()) {
        let joined = targets
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  Targets: {joined}\n"));
    }
    if let Some(clean_first) = build.clean_first {
        out.push_str(&format!("  Clean first: {clean_first}\n"));
    }
    if let Some(refs) = build.resolve_package_references {
        out.push_str(&format!(
            "  Resolve package references: {}\n",
            refs.as_str()
        ));
    }
    if let Some(verbose) = build.verbose {
        out.push_str(&format!("  Verbose: {verbose}\n"));
    }
    if let Some(opts) = build.native_tool_options.as_ref().filter(|v| !v.is_empty()) {
        let joined = opts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("  Native tool options: {joined}\n"));
    }
}

fn render_test_text(out: &mut String, test: &TestSettings) {
    if let Some(config) = test.configuration.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Configuration: {config}\n"));
    }
    if let Some(overwrite) = test
        .overwrite_configuration_file
        .as_ref()
        .filter(|v| !v.is_empty())
    {
        let joined = overwrite
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  Overwrite config file: {joined}\n"));
    }
    if let Some(output) = &test.output {
        let mut items = Vec::new();
        if let Some(v) = output.verbosity {
            items.push(format!("verbosity={}", v.as_str()));
        }
        if let Some(v) = output.output_on_failure {
            items.push(format!("outputOnFailure={v}"));
        }
        if let Some(v) = output.quiet {
            items.push(format!("quiet={v}"));
        }
        if let Some(v) = output.debug {
            items.push(format!("debug={v}"));
        }
        if let Some(v) = output.short_progress {
            items.push(format!("shortProgress={v}"));
        }
        if let Some(v) = &output.output_log_file {
            items.push(format!("outputLogFile={v}"));
        }
        if let Some(v) = &output.output_junit_file {
            items.push(format!("outputJUnitFile={v}"));
        }
        if let Some(v) = output.label_summary {
            items.push(format!("labelSummary={v}"));
        }
        if let Some(v) = output.subproject_summary {
            items.push(format!("subprojectSummary={v}"));
        }
        if let Some(v) = output.max_passed_test_output_size {
            items.push(format!("maxPassedTestOutputSize={v}"));
        }
        if let Some(v) = output.max_failed_test_output_size {
            items.push(format!("maxFailedTestOutputSize={v}"));
        }
        if let Some(v) = output.test_output_truncation {
            items.push(format!("testOutputTruncation={}", v.as_str()));
        }
        if let Some(v) = output.max_test_name_width {
            items.push(format!("maxTestNameWidth={v}"));
        }
        if !items.is_empty() {
            out.push_str(&format!("  Output: {}\n", items.join(", ")));
        }
    }
    if let Some(filter) = &test.filter {
        if let Some(inc) = &filter.include {
            let mut items = Vec::new();
            if let Some(name) = &inc.name {
                items.push(format!("name={name}"));
            }
            if let Some(label) = &inc.label {
                items.push(format!("label={label}"));
            }
            if let Some(u) = inc.use_union {
                items.push(format!("useUnion={u}"));
            }
            if let Some(idx) = &inc.index {
                match idx {
                    TestIndex::File(file) => items.push(format!("index={file}")),
                    TestIndex::Range(range) => items.push(format!("index={range:?}")),
                }
            }
            if !items.is_empty() {
                out.push_str(&format!("  Filter include: {}\n", items.join(", ")));
            }
        }
        if let Some(exc) = &filter.exclude {
            let mut items = Vec::new();
            if let Some(name) = &exc.name {
                items.push(format!("name={name}"));
            }
            if let Some(label) = &exc.label {
                items.push(format!("label={label}"));
            }
            if let Some(fixtures) = &exc.fixtures {
                if let Some(any) = &fixtures.any {
                    items.push(format!("fixtures.any={any}"));
                }
                if let Some(setup) = &fixtures.setup {
                    items.push(format!("fixtures.setup={setup}"));
                }
                if let Some(cleanup) = &fixtures.cleanup {
                    items.push(format!("fixtures.cleanup={cleanup}"));
                }
            }
            if !items.is_empty() {
                out.push_str(&format!("  Filter exclude: {}\n", items.join(", ")));
            }
        }
    }
    if let Some(exec) = &test.execution {
        let mut items = Vec::new();
        if let Some(v) = exec.jobs {
            match v {
                TestJobs::Automatic => items.push("jobs=auto".into()),
                TestJobs::Count(n) => items.push(format!("jobs={n}")),
            }
        }
        if let Some(v) = exec.stop_on_failure {
            items.push(format!("stopOnFailure={v}"));
        }
        if let Some(v) = exec.enable_failover {
            items.push(format!("enableFailover={v}"));
        }
        if let Some(v) = &exec.resource_spec_file {
            items.push(format!("resourceSpecFile={v}"));
        }
        if let Some(v) = exec.test_load {
            items.push(format!("testLoad={v}"));
        }
        if let Some(v) = exec.show_only {
            items.push(format!("showOnly={}", v.as_str()));
        }
        if let Some(v) = exec.repeat {
            items.push(format!("repeat={}:{}", v.mode.as_str(), v.count));
        }
        if let Some(v) = exec.interactive_debugging {
            items.push(format!("interactiveDebugging={v}"));
        }
        if let Some(v) = exec.schedule_random {
            items.push(format!("scheduleRandom={v}"));
        }
        if let Some(v) = exec.timeout {
            items.push(format!("timeout={v}"));
        }
        if let Some(v) = exec.no_tests_action {
            items.push(format!("noTestsAction={}", v.as_str()));
        }
        if let Some(args) = exec
            .test_passthrough_arguments
            .as_ref()
            .filter(|v| !v.is_empty())
        {
            let joined = args
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            items.push(format!("testPassthroughArguments=[{joined}]"));
        }
        if !items.is_empty() {
            out.push_str(&format!("  Execution: {}\n", items.join(", ")));
        }
    }
}

fn render_package_text(out: &mut String, pkg: &PackageSettings, show_secrets: bool) {
    if let Some(gens) = pkg.generators.as_ref().filter(|v| !v.is_empty()) {
        let joined = gens
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  Generators: {joined}\n"));
    }
    if let Some(configs) = pkg.configurations.as_ref().filter(|v| !v.is_empty()) {
        let joined = configs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  Configurations: {joined}\n"));
    }
    if let Some(file) = pkg.config_file.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Config file: {file}\n"));
    }
    if let Some(name) = pkg.package_name.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Package name: {name}\n"));
    }
    if let Some(ver) = pkg.package_version.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Package version: {ver}\n"));
    }
    if let Some(dir) = pkg.package_directory.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Package directory: {dir}\n"));
    }
    if let Some(vendor) = pkg.vendor_name.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("  Vendor name: {vendor}\n"));
    }
    if let Some(output) = &pkg.output {
        let mut items = Vec::new();
        if let Some(v) = output.debug {
            items.push(format!("debug={v}"));
        }
        if let Some(v) = output.verbose {
            items.push(format!("verbose={v}"));
        }
        if !items.is_empty() {
            out.push_str(&format!("  Output: {}\n", items.join(", ")));
        }
    }
    if let Some(vars) = pkg.variables.as_ref().filter(|m| !m.is_empty()) {
        out.push_str("  Variables:\n");
        for (k, v) in vars {
            let shown = redacted_value(k.as_slice(), v, show_secrets);
            out.push_str(&format!("    {k}={shown}\n"));
        }
    }
}

fn render_workflow_text(out: &mut String, workflow: &WorkflowSettings) {
    if !workflow.steps.is_empty() {
        out.push_str("  Steps:\n");
        for (index, step) in workflow.steps.iter().enumerate() {
            out.push_str(&format!(
                "    {}. {}: {}\n",
                index + 1,
                step.kind.label(),
                step.name
            ));
        }
    }
}

fn render_json(
    source_dir: &Path,
    matches: &[PresetInspection],
    errors: &[(PresetKind, String)],
    show_secrets: bool,
) -> Value {
    let mut root = BTreeMap::new();
    root.insert(BString::from("schema"), Value::string(SCHEMA));
    root.insert(
        BString::from("sourceDir"),
        Value::string(path::from_native_cmake(source_dir)),
    );
    root.insert(
        BString::from("presets"),
        Value::Array(
            matches
                .iter()
                .map(|item| inspection_to_json(item, show_secrets))
                .collect(),
        ),
    );
    if !errors.is_empty() {
        let err_items = errors
            .iter()
            .map(|(kind, message)| {
                let mut entry = BTreeMap::new();
                entry.insert(BString::from("kind"), Value::string(kind.label()));
                entry.insert(BString::from("message"), Value::string(message.clone()));
                Value::Object(entry)
            })
            .collect();
        root.insert(BString::from("errors"), Value::Array(err_items));
    }
    Value::Object(root)
}

fn inspection_to_json(item: &PresetInspection, show_secrets: bool) -> Value {
    let preset = &item.preset;
    let mut obj = BTreeMap::new();
    obj.insert(BString::from("kind"), Value::string(preset.kind.label()));
    obj.insert(BString::from("name"), Value::string(preset.name.clone()));
    if let Some(display) = &preset.display_name {
        obj.insert(BString::from("displayName"), Value::string(display.clone()));
    }
    if let Some(desc) = &preset.description {
        obj.insert(BString::from("description"), Value::string(desc.clone()));
    }
    obj.insert(
        BString::from("sourceFile"),
        Value::string(path::from_native_cmake(&item.source_file)),
    );
    obj.insert(BString::from("hidden"), Value::Bool(item.hidden));
    obj.insert(BString::from("enabled"), Value::Bool(item.enabled));
    obj.insert(
        BString::from("status"),
        Value::string(if item.enabled { "enabled" } else { "disabled" }),
    );
    if !item.inherits.is_empty() {
        obj.insert(
            BString::from("inherits"),
            Value::Array(item.inherits.iter().cloned().map(Value::string).collect()),
        );
    }
    if let Some(condition) = &item.condition {
        obj.insert(BString::from("condition"), condition.clone());
    }
    if let Some(cfg) = preset.configure_preset.as_ref().filter(|s| !s.is_empty()) {
        obj.insert(BString::from("configurePreset"), Value::string(cfg.clone()));
    }
    if let Some(inherit_env) = preset.inherit_configure_environment {
        obj.insert(
            BString::from("inheritConfigureEnvironment"),
            Value::Bool(inherit_env),
        );
    }
    if let Some(generator) = &preset.generator {
        obj.insert(BString::from("generator"), Value::string(generator.clone()));
    }
    if let Some(binary_dir) = &preset.binary_dir {
        obj.insert(
            BString::from("binaryDir"),
            Value::string(binary_dir.clone()),
        );
    }
    if !preset.environment.is_empty() {
        let env_map = preset
            .environment
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_ref().map_or(Value::Null, |val| {
                        Value::string(redacted_value(k.as_slice(), val, show_secrets))
                    }),
                )
            })
            .collect();
        obj.insert(BString::from("environment"), Value::Object(env_map));
    }
    obj.insert(
        BString::from("settings"),
        settings_to_json(&preset.settings, show_secrets),
    );
    Value::Object(obj)
}

fn selection_to_json(sel: &GeneratorSelection) -> Value {
    let mut obj = BTreeMap::new();
    if let Some(val) = &sel.value {
        obj.insert(BString::from("value"), Value::string(val.clone()));
    }
    if let Some(strategy) = sel.strategy {
        obj.insert(BString::from("strategy"), Value::string(strategy.as_str()));
    }
    Value::Object(obj)
}

fn string_array(values: &[BString]) -> Value {
    Value::Array(values.iter().cloned().map(Value::string).collect())
}

fn settings_to_json(settings: &PresetSettings, show_secrets: bool) -> Value {
    let mut obj = BTreeMap::new();
    match settings {
        PresetSettings::Configure(cfg) => {
            if let Some(v) = &cfg.generator {
                obj.insert(BString::from("generator"), Value::string(v.clone()));
            }
            if let Some(v) = &cfg.architecture {
                obj.insert(BString::from("architecture"), selection_to_json(v));
            }
            if let Some(v) = &cfg.toolset {
                obj.insert(BString::from("toolset"), selection_to_json(v));
            }
            if let Some(v) = &cfg.binary_dir {
                obj.insert(BString::from("binaryDir"), Value::string(v.clone()));
            }
            if let Some(v) = &cfg.install_dir {
                obj.insert(BString::from("installDir"), Value::string(v.clone()));
            }
            if let Some(v) = &cfg.toolchain_file {
                obj.insert(BString::from("toolchainFile"), Value::string(v.clone()));
            }
            if let Some(v) = &cfg.graphviz {
                obj.insert(BString::from("graphviz"), Value::string(v.clone()));
            }
            if let Some(v) = &cfg.cmake_executable {
                obj.insert(BString::from("cmakeExecutable"), Value::string(v.clone()));
            }
            if let Some(vars) = &cfg.cache_variables {
                let map = vars
                    .iter()
                    .map(|(k, v)| {
                        let mut entry = BTreeMap::new();
                        if let Some(kind) = &v.kind {
                            entry.insert(BString::from("type"), Value::string(kind.clone()));
                        }
                        entry.insert(
                            BString::from("value"),
                            Value::string(redacted_value(k.as_slice(), &v.value, show_secrets)),
                        );
                        (k.clone(), Value::Object(entry))
                    })
                    .collect();
                obj.insert(BString::from("cacheVariables"), Value::Object(map));
            }
            if let Some(w) = &cfg.warnings {
                let mut map = BTreeMap::new();
                for (name, val) in [
                    ("author", w.author),
                    ("deprecated", w.deprecated),
                    ("experimental", w.experimental),
                    ("installAbsoluteDestination", w.install_absolute_destination),
                    ("nonTargetDirective", w.non_target_directive),
                    ("policy", w.policy),
                    ("strict", w.strict),
                    ("uninitialized", w.uninitialized),
                    ("unusedCli", w.unused_cli),
                    ("systemVars", w.system_vars),
                ] {
                    if let Some(flag) = val {
                        map.insert(BString::from(name), Value::Bool(flag));
                    }
                }
                obj.insert(BString::from("warnings"), Value::Object(map));
            }
            if let Some(e) = &cfg.errors {
                let mut map = BTreeMap::new();
                for (name, val) in [
                    ("author", e.author),
                    ("deprecated", e.deprecated),
                    ("experimental", e.experimental),
                    ("installAbsoluteDestination", e.install_absolute_destination),
                    ("nonTargetDirective", e.non_target_directive),
                    ("policy", e.policy),
                    ("strict", e.strict),
                    ("uninitialized", e.uninitialized),
                    ("unusedCli", e.unused_cli),
                ] {
                    if let Some(flag) = val {
                        map.insert(BString::from(name), Value::Bool(flag));
                    }
                }
                obj.insert(BString::from("errors"), Value::Object(map));
            }
            if let Some(d) = &cfg.debug {
                let mut map = BTreeMap::new();
                if let Some(v) = d.output {
                    map.insert(BString::from("output"), Value::Bool(v));
                }
                if let Some(v) = d.try_compile {
                    map.insert(BString::from("tryCompile"), Value::Bool(v));
                }
                if let Some(v) = d.find {
                    map.insert(BString::from("find"), Value::Bool(v));
                }
                obj.insert(BString::from("debug"), Value::Object(map));
            }
            if let Some(t) = &cfg.trace {
                let mut map = BTreeMap::new();
                if let Some(v) = t.mode {
                    map.insert(BString::from("mode"), Value::string(v.as_str()));
                }
                if let Some(v) = t.format {
                    map.insert(BString::from("format"), Value::string(v.as_str()));
                }
                if let Some(v) = &t.source {
                    map.insert(BString::from("source"), string_array(v));
                }
                if let Some(v) = &t.redirect {
                    map.insert(BString::from("redirect"), Value::string(v.clone()));
                }
                obj.insert(BString::from("trace"), Value::Object(map));
            }
        }
        PresetSettings::Build(build) => {
            if let Some(v) = build.jobs {
                obj.insert(BString::from("jobs"), Value::UInt(u64::from(v)));
            }
            if let Some(v) = &build.targets {
                obj.insert(BString::from("targets"), string_array(v));
            }
            if let Some(v) = &build.configuration {
                obj.insert(BString::from("configuration"), Value::string(v.clone()));
            }
            if let Some(v) = build.clean_first {
                obj.insert(BString::from("cleanFirst"), Value::Bool(v));
            }
            if let Some(v) = build.resolve_package_references {
                obj.insert(
                    BString::from("resolvePackageReferences"),
                    Value::string(v.as_str()),
                );
            }
            if let Some(v) = build.verbose {
                obj.insert(BString::from("verbose"), Value::Bool(v));
            }
            if let Some(v) = &build.native_tool_options {
                obj.insert(BString::from("nativeToolOptions"), string_array(v));
            }
        }
        PresetSettings::Test(test) => {
            if let Some(v) = &test.configuration {
                obj.insert(BString::from("configuration"), Value::string(v.clone()));
            }
            if let Some(v) = &test.overwrite_configuration_file {
                obj.insert(BString::from("overwriteConfigurationFile"), string_array(v));
            }
            if let Some(output) = &test.output {
                let mut map = BTreeMap::new();
                if let Some(v) = output.short_progress {
                    map.insert(BString::from("shortProgress"), Value::Bool(v));
                }
                if let Some(v) = output.verbosity {
                    map.insert(BString::from("verbosity"), Value::string(v.as_str()));
                }
                if let Some(v) = output.debug {
                    map.insert(BString::from("debug"), Value::Bool(v));
                }
                if let Some(v) = output.output_on_failure {
                    map.insert(BString::from("outputOnFailure"), Value::Bool(v));
                }
                if let Some(v) = output.quiet {
                    map.insert(BString::from("quiet"), Value::Bool(v));
                }
                if let Some(v) = &output.output_log_file {
                    map.insert(BString::from("outputLogFile"), Value::string(v.clone()));
                }
                if let Some(v) = &output.output_junit_file {
                    map.insert(BString::from("outputJUnitFile"), Value::string(v.clone()));
                }
                if let Some(v) = output.label_summary {
                    map.insert(BString::from("labelSummary"), Value::Bool(v));
                }
                if let Some(v) = output.subproject_summary {
                    map.insert(BString::from("subprojectSummary"), Value::Bool(v));
                }
                if let Some(v) = output.max_passed_test_output_size {
                    map.insert(
                        BString::from("maxPassedTestOutputSize"),
                        Value::Int(i64::from(v)),
                    );
                }
                if let Some(v) = output.max_failed_test_output_size {
                    map.insert(
                        BString::from("maxFailedTestOutputSize"),
                        Value::Int(i64::from(v)),
                    );
                }
                if let Some(v) = output.test_output_truncation {
                    map.insert(
                        BString::from("testOutputTruncation"),
                        Value::string(v.as_str()),
                    );
                }
                if let Some(v) = output.max_test_name_width {
                    map.insert(BString::from("maxTestNameWidth"), Value::Int(i64::from(v)));
                }
                obj.insert(BString::from("output"), Value::Object(map));
            }
            if let Some(filter) = &test.filter {
                let mut map = BTreeMap::new();
                if let Some(inc) = &filter.include {
                    let mut inc_map = BTreeMap::new();
                    if let Some(v) = &inc.name {
                        inc_map.insert(BString::from("name"), Value::string(v.clone()));
                    }
                    if let Some(v) = &inc.label {
                        inc_map.insert(BString::from("label"), Value::string(v.clone()));
                    }
                    if let Some(v) = inc.use_union {
                        inc_map.insert(BString::from("useUnion"), Value::Bool(v));
                    }
                    if let Some(idx) = &inc.index {
                        let idx_val = match idx {
                            TestIndex::File(file) => Value::string(file.clone()),
                            TestIndex::Range(range) => {
                                let mut r = BTreeMap::new();
                                if let Some(v) = range.start {
                                    r.insert(BString::from("start"), Value::Int(i64::from(v)));
                                }
                                if let Some(v) = range.end {
                                    r.insert(BString::from("end"), Value::Int(i64::from(v)));
                                }
                                if let Some(v) = range.stride {
                                    r.insert(BString::from("stride"), Value::Int(i64::from(v)));
                                }
                                if let Some(v) = &range.specific_tests {
                                    r.insert(
                                        BString::from("specificTests"),
                                        Value::Array(
                                            v.iter().map(|&n| Value::Int(i64::from(n))).collect(),
                                        ),
                                    );
                                }
                                Value::Object(r)
                            }
                        };
                        inc_map.insert(BString::from("index"), idx_val);
                    }
                    map.insert(BString::from("include"), Value::Object(inc_map));
                }
                if let Some(exc) = &filter.exclude {
                    let mut exc_map = BTreeMap::new();
                    if let Some(v) = &exc.name {
                        exc_map.insert(BString::from("name"), Value::string(v.clone()));
                    }
                    if let Some(v) = &exc.label {
                        exc_map.insert(BString::from("label"), Value::string(v.clone()));
                    }
                    if let Some(fix) = &exc.fixtures {
                        let mut fix_map = BTreeMap::new();
                        if let Some(v) = &fix.any {
                            fix_map.insert(BString::from("any"), Value::string(v.clone()));
                        }
                        if let Some(v) = &fix.setup {
                            fix_map.insert(BString::from("setup"), Value::string(v.clone()));
                        }
                        if let Some(v) = &fix.cleanup {
                            fix_map.insert(BString::from("cleanup"), Value::string(v.clone()));
                        }
                        exc_map.insert(BString::from("fixtures"), Value::Object(fix_map));
                    }
                    map.insert(BString::from("exclude"), Value::Object(exc_map));
                }
                obj.insert(BString::from("filter"), Value::Object(map));
            }
            if let Some(exec) = &test.execution {
                let mut map = BTreeMap::new();
                if let Some(v) = exec.stop_on_failure {
                    map.insert(BString::from("stopOnFailure"), Value::Bool(v));
                }
                if let Some(v) = exec.enable_failover {
                    map.insert(BString::from("enableFailover"), Value::Bool(v));
                }
                if let Some(v) = exec.jobs {
                    let jobs_val = match v {
                        TestJobs::Automatic => Value::string(""),
                        TestJobs::Count(n) => Value::UInt(u64::from(n)),
                    };
                    map.insert(BString::from("jobs"), jobs_val);
                }
                if let Some(v) = &exec.resource_spec_file {
                    map.insert(BString::from("resourceSpecFile"), Value::string(v.clone()));
                }
                if let Some(v) = exec.test_load {
                    map.insert(BString::from("testLoad"), Value::Int(i64::from(v)));
                }
                if let Some(v) = exec.show_only {
                    map.insert(BString::from("showOnly"), Value::string(v.as_str()));
                }
                if let Some(v) = exec.repeat {
                    let mut rep = BTreeMap::new();
                    rep.insert(BString::from("mode"), Value::string(v.mode.as_str()));
                    rep.insert(BString::from("count"), Value::Int(i64::from(v.count)));
                    map.insert(BString::from("repeat"), Value::Object(rep));
                }
                if let Some(v) = exec.interactive_debugging {
                    map.insert(BString::from("interactiveDebugging"), Value::Bool(v));
                }
                if let Some(v) = exec.schedule_random {
                    map.insert(BString::from("scheduleRandom"), Value::Bool(v));
                }
                if let Some(v) = exec.timeout {
                    map.insert(BString::from("timeout"), Value::Int(i64::from(v)));
                }
                if let Some(v) = exec.no_tests_action {
                    map.insert(BString::from("noTestsAction"), Value::string(v.as_str()));
                }
                if let Some(v) = &exec.test_passthrough_arguments {
                    map.insert(BString::from("testPassthroughArguments"), string_array(v));
                }
                obj.insert(BString::from("execution"), Value::Object(map));
            }
        }
        PresetSettings::Package(pkg) => {
            if let Some(v) = &pkg.generators {
                obj.insert(BString::from("generators"), string_array(v));
            }
            if let Some(v) = &pkg.configurations {
                obj.insert(BString::from("configurations"), string_array(v));
            }
            if let Some(vars) = &pkg.variables {
                let map = vars
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            Value::string(redacted_value(k.as_slice(), v, show_secrets)),
                        )
                    })
                    .collect();
                obj.insert(BString::from("variables"), Value::Object(map));
            }
            if let Some(v) = &pkg.config_file {
                obj.insert(BString::from("configFile"), Value::string(v.clone()));
            }
            if let Some(output) = &pkg.output {
                let mut map = BTreeMap::new();
                if let Some(v) = output.debug {
                    map.insert(BString::from("debug"), Value::Bool(v));
                }
                if let Some(v) = output.verbose {
                    map.insert(BString::from("verbose"), Value::Bool(v));
                }
                obj.insert(BString::from("output"), Value::Object(map));
            }
            if let Some(v) = &pkg.package_name {
                obj.insert(BString::from("packageName"), Value::string(v.clone()));
            }
            if let Some(v) = &pkg.package_version {
                obj.insert(BString::from("packageVersion"), Value::string(v.clone()));
            }
            if let Some(v) = &pkg.package_directory {
                obj.insert(BString::from("packageDirectory"), Value::string(v.clone()));
            }
            if let Some(v) = &pkg.vendor_name {
                obj.insert(BString::from("vendorName"), Value::string(v.clone()));
            }
        }
        PresetSettings::Workflow(workflow) => {
            let steps = workflow
                .steps
                .iter()
                .map(|step| {
                    let mut entry = BTreeMap::new();
                    entry.insert(BString::from("type"), Value::string(step.kind.label()));
                    entry.insert(BString::from("name"), Value::string(step.name.clone()));
                    Value::Object(entry)
                })
                .collect();
            obj.insert(BString::from("steps"), Value::Array(steps));
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rcmake_host::PureHost;

    use super::*;

    fn invoke(host: &PureHost, args: &[&str]) -> (i32, String, String) {
        let argv: Vec<BString> = args.iter().copied().map(BString::from).collect();
        let code = run(&argv, host);
        (
            code,
            host.stream_bytes(Stream::Stdout).to_string(),
            host.stream_bytes(Stream::Stderr).to_string(),
        )
    }

    #[test]
    fn resolves_includes_inherits_macros_env_and_disabled_conditions() {
        let host = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host.env_set(b"EXTRA_TAG", Some(b"nightly"));
        host.seed_file(
            Path::new("/work/base.json"),
            br#"{
  "version": 6,
  "configurePresets": [
    {
      "name": "base",
      "hidden": true,
      "generator": "Ninja",
      "binaryDir": "${sourceDir}/build/${presetName}",
      "cacheVariables": {
        "CMAKE_EXPORT_COMPILE_COMMANDS": "ON",
        "BASE_TAG": "$env{EXTRA_TAG}"
      },
      "environment": {
        "MY_MODE": "base"
      }
    }
  ]
}"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CMakePresets.json"),
            br#"{
  "version": 6,
  "include": ["base.json"],
  "configurePresets": [
    {
      "name": "dev",
      "displayName": "Dev Build",
      "inherits": "base",
      "cacheVariables": {
        "CMAKE_BUILD_TYPE": "Debug"
      },
      "environment": {
        "MY_MODE": "dev"
      }
    },
    {
      "name": "win-only",
      "inherits": "base",
      "condition": {
        "type": "equals",
        "lhs": "${hostSystemName}",
        "rhs": "NonExistentOS"
      }
    }
  ],
  "buildPresets": [
    {
      "name": "dev",
      "configurePreset": "dev",
      "jobs": 4,
      "targets": ["app"]
    }
  ]
}"#,
        )
        .unwrap();

        // Multi-kind resolution when `--kind` is omitted shows configure then build.
        let (code, stdout, stderr) = invoke(&host, &["show", "dev"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stderr.is_empty(), "{stderr}");
        let cfg_pos = stdout
            .find("Preset: dev (configure) [enabled]")
            .expect("configure preset");
        let build_pos = stdout
            .find("Preset: dev (build) [enabled]")
            .expect("build preset");
        assert!(cfg_pos < build_pos, "{stdout}");
        assert!(stdout.contains("Binary dir: /work/build/dev"), "{stdout}");
        assert!(stdout.contains("BASE_TAG=nightly"), "{stdout}");
        assert!(stdout.contains("CMAKE_BUILD_TYPE=Debug"), "{stdout}");
        assert!(stdout.contains("MY_MODE=dev"), "{stdout}");
        assert!(stdout.contains("Jobs: 4"), "{stdout}");
        assert!(stdout.contains("Targets: app"), "{stdout}");

        // `--kind` narrows to one category and `--json` produces valid JSON.
        let host2 = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host2.env_set(b"EXTRA_TAG", Some(b"nightly"));
        host2
            .seed_file(
                Path::new("/work/base.json"),
                &host
                    .read(
                        Path::new("/work/base.json"),
                        rcmake_host::ReadRange::All { limit: 1 << 20 },
                    )
                    .unwrap(),
            )
            .unwrap();
        host2
            .seed_file(
                Path::new("/work/CMakePresets.json"),
                &host
                    .read(
                        Path::new("/work/CMakePresets.json"),
                        rcmake_host::ReadRange::All { limit: 1 << 20 },
                    )
                    .unwrap(),
            )
            .unwrap();

        let (code, stdout, stderr) = invoke(&host2, &["show", "win-only", "--json"]);
        assert_eq!(code, 0, "{stderr}");
        let parsed = rcmake_json::parse(stdout.as_bytes(), &rcmake_json::ReadOptions::default())
            .expect("valid json");
        assert_eq!(parsed.get("schema").and_then(Value::as_str), Some(SCHEMA));
        let presets = parsed
            .get("presets")
            .and_then(Value::as_array)
            .expect("presets array");
        assert_eq!(presets.len(), 1);
        assert_eq!(
            presets[0].get("status").and_then(Value::as_str),
            Some("disabled")
        );
        assert_eq!(
            presets[0].get("enabled").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            presets[0].get("binaryDir").and_then(Value::as_str),
            Some("/work/build/win-only")
        );
    }

    #[test]
    fn missing_preset_exits_one_with_fuzzy_suggestion() {
        let host = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host.seed_file(
            Path::new("/work/CMakePresets.json"),
            br#"{
  "version": 6,
  "configurePresets": [
    { "name": "release", "generator": "Ninja", "binaryDir": "${sourceDir}/build" }
  ]
}"#,
        )
        .unwrap();

        let (code, stdout, stderr) = invoke(&host, &["show", "relase", "--kind", "configure"]);
        assert_eq!(code, 1);
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains("No such configure preset"), "{stderr}");
        assert!(stderr.contains("Did you mean \"release\"?"), "{stderr}");
    }

    #[test]
    fn invalid_presets_json_exits_one_with_diagnostic() {
        let host = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host.seed_file(Path::new("/work/CMakePresets.json"), b"{ invalid json")
            .unwrap();
        let (code, stdout, stderr) = invoke(&host, &["show", "dev"]);
        assert_eq!(code, 1);
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains("rcmake preset:"), "{stderr}");
    }

    #[test]
    fn source_flag_reads_presets_from_explicit_directory_without_writes() {
        let host = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host.seed_file(
            Path::new("/proj/CMakePresets.json"),
            br#"{
  "version": 6,
  "configurePresets": [
    { "name": "ci", "generator": "Ninja", "binaryDir": "${sourceDir}/out" }
  ]
}"#,
        )
        .unwrap();

        let (code, stdout, stderr) = invoke(&host, &["show", "ci", "-S", "/proj"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("Binary dir: /proj/out"), "{stdout}");
        assert!(
            host.metadata(Path::new("/proj/out"), rcmake_host::Follow::Yes)
                .is_err(),
            "preset show must not create build directories"
        );
    }

    #[test]
    fn redacts_sensitive_env_and_cache_values_by_default_in_text_and_json() {
        let presets_json = br#"{
  "version": 6,
  "configurePresets": [
    {
      "name": "sec",
      "generator": "Ninja",
      "binaryDir": "${sourceDir}/build",
      "cacheVariables": {
        "CMAKE_BUILD_TYPE": "Release",
        "MY_API_KEY": "super-secret-api-key",
        "dbPassword": "super-secret-password"
      },
      "environment": {
        "CI_TOKEN": "ghp_secret_ci_token",
        "accessToken": "tok_camel_secret",
        "MY_MODE": "prod"
      }
    }
  ]
}"#;

        let host_text = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host_text
            .seed_file(Path::new("/work/CMakePresets.json"), presets_json)
            .unwrap();
        let (code, stdout, stderr) = invoke(&host_text, &["show", "sec"]);
        assert_eq!(code, 0, "{stderr}");
        for leaked in [
            "super-secret-api-key",
            "super-secret-password",
            "ghp_secret_ci_token",
            "tok_camel_secret",
        ] {
            assert!(!stdout.contains(leaked), "leaked secret in text: {stdout}");
        }
        assert!(stdout.contains("MY_API_KEY=<redacted>"), "{stdout}");
        assert!(stdout.contains("dbPassword=<redacted>"), "{stdout}");
        assert!(stdout.contains("CI_TOKEN=<redacted>"), "{stdout}");
        assert!(stdout.contains("accessToken=<redacted>"), "{stdout}");
        assert!(stdout.contains("CMAKE_BUILD_TYPE=Release"), "{stdout}");
        assert!(stdout.contains("MY_MODE=prod"), "{stdout}");

        let host_json = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host_json
            .seed_file(Path::new("/work/CMakePresets.json"), presets_json)
            .unwrap();
        let (code, stdout, stderr) = invoke(&host_json, &["show", "sec", "--json"]);
        assert_eq!(code, 0, "{stderr}");
        for leaked in [
            "super-secret-api-key",
            "super-secret-password",
            "ghp_secret_ci_token",
            "tok_camel_secret",
        ] {
            assert!(!stdout.contains(leaked), "leaked secret in json: {stdout}");
        }
        let parsed = rcmake_json::parse(stdout.as_bytes(), &rcmake_json::ReadOptions::default())
            .expect("valid json");
        let preset = &parsed
            .get("presets")
            .and_then(Value::as_array)
            .expect("presets")[0];
        let env = preset.get("environment").expect("env");
        assert_eq!(
            env.get("CI_TOKEN").and_then(Value::as_str),
            Some("<redacted>")
        );
        assert_eq!(
            env.get("accessToken").and_then(Value::as_str),
            Some("<redacted>")
        );
        assert_eq!(env.get("MY_MODE").and_then(Value::as_str), Some("prod"));
        let cache = preset
            .get("settings")
            .and_then(|s| s.get("cacheVariables"))
            .expect("cacheVariables");
        assert_eq!(
            cache
                .get("MY_API_KEY")
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str),
            Some("<redacted>")
        );

        // `--show-secrets` reveals raw values when explicitly requested.
        let host_raw = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host_raw
            .seed_file(Path::new("/work/CMakePresets.json"), presets_json)
            .unwrap();
        let (code, stdout, stderr) = invoke(&host_raw, &["show", "sec", "--show-secrets"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("CI_TOKEN=ghp_secret_ci_token"), "{stdout}");
        assert!(
            stdout.contains("MY_API_KEY=super-secret-api-key"),
            "{stdout}"
        );
    }

    #[test]
    fn multi_kind_lookup_renders_resolved_kinds_when_another_kind_errors() {
        let presets_json = br#"{
  "version": 6,
  "configurePresets": [
    {
      "name": "dev",
      "generator": "Ninja",
      "binaryDir": "${sourceDir}/build/dev"
    }
  ],
  "buildPresets": [
    {
      "name": "dev",
      "configurePreset": "dev",
      "environment": {
        "BROKEN": "$vendor{unsupported}"
      }
    }
  ]
}"#;

        let host_text = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host_text
            .seed_file(Path::new("/work/CMakePresets.json"), presets_json)
            .unwrap();
        let (code, stdout, stderr) = invoke(&host_text, &["show", "dev"]);
        assert_eq!(code, 1);
        assert!(
            stdout.contains("Preset: dev (configure) [enabled]"),
            "valid configure preset must still render: {stdout}"
        );
        assert!(
            stderr.contains("Could not evaluate build preset \"dev\""),
            "failing build preset error must be reported: {stderr}"
        );

        let host_json = PureHost::new(PathBuf::from("/work"), PathBuf::from("/bin/rcmake"));
        host_json
            .seed_file(Path::new("/work/CMakePresets.json"), presets_json)
            .unwrap();
        let (code, stdout, stderr) = invoke(&host_json, &["show", "dev", "--json"]);
        assert_eq!(code, 1);
        assert!(!stderr.is_empty());
        let parsed = rcmake_json::parse(stdout.as_bytes(), &rcmake_json::ReadOptions::default())
            .expect("valid json");
        assert_eq!(
            parsed
                .get("presets")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(1)
        );
        let errors = parsed
            .get("errors")
            .and_then(Value::as_array)
            .expect("errors array");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].get("kind").and_then(Value::as_str), Some("build"));
    }
}
