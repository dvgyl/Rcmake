mod elf;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{
    Cx, Flow, RunMode,
    args::{KeywordParser, Parsed, Slot},
};
use rcmake_host::{
    CommandSpec, Follow, Input, Output, ProcessResult, RunRequest, Stream,
    path::{executable_on_absolute_path, from_native, to_native},
};
use rcmake_regex::{MatchOptions, Regex};

use super::{Result, error, set, warning};

const INPUTS: [&str; 3] = ["EXECUTABLES", "LIBRARIES", "MODULES"];

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let result = run_command(cx, args);
    if result.is_err() {
        cx.set_fatal();
    }
    result
}

fn run_command(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let platform = cx.var_or_empty(b"CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM");
    let system = cx.var_or_empty(b"CMAKE_HOST_SYSTEM_NAME");
    let host_platform = match system.as_slice() {
        b"Darwin" => b"macos+macho".as_slice(),
        b"Linux" => b"linux+elf",
        b"FreeBSD" => b"freebsd+elf",
        b"Windows" => b"windows+pe",
        _ if !platform.is_empty() => b"",
        _ => {
            return Err(error(
                [
                    b"GET_RUNTIME_DEPENDENCIES is not supported on system \"".as_slice(),
                    &system,
                    b"\"",
                ]
                .concat(),
            ));
        }
    };
    if cx.mode() == RunMode::Configure {
        warning(cx, b"You have used file(GET_RUNTIME_DEPENDENCIES) in project mode. This is probably not what you intended to do. Instead, please consider using it in an install(CODE) or install(SCRIPT) command. For example:\n  install(CODE [[\n    file(GET_RUNTIME_DEPENDENCIES\n      # ...\n      )\n    ]])".to_vec());
    }

    let mut parser = KeywordParser::new();
    for key in [
        "RESOLVED_DEPENDENCIES_VAR",
        "UNRESOLVED_DEPENDENCIES_VAR",
        "CONFLICTING_DEPENDENCIES_PREFIX",
        "RPATH_PREFIX",
        "BUNDLE_EXECUTABLE",
    ] {
        parser = parser.keyword(key, Slot::Single);
    }
    for key in INPUTS.into_iter().chain([
        "DIRECTORIES",
        "PRE_INCLUDE_REGEXES",
        "PRE_EXCLUDE_REGEXES",
        "POST_INCLUDE_REGEXES",
        "POST_EXCLUDE_REGEXES",
        "POST_INCLUDE_FILES",
        "POST_EXCLUDE_FILES",
        "POST_EXCLUDE_FILES_STRICT",
    ]) {
        parser = parser.keyword(key, Slot::Multi);
    }
    let args = parser.parse(args);
    if let Some(arg) = args.unparsed().first() {
        return Err(error(
            [b"Unrecognized argument: \"".as_slice(), arg, b"\""].concat(),
        ));
    }
    if let Some(message) = args.report_error_text() {
        return Err(crate::command_error(cx, message));
    }

    let platform = if platform.is_empty() {
        host_platform
    } else {
        &platform
    };
    let tool = cx.var_or_empty(b"CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL");
    let valid_tool = match platform {
        b"macos+macho" => tool.is_empty() || tool.as_slice() == b"otool",
        b"linux+elf" | b"freebsd+elf" => tool.is_empty() || tool.as_slice() == b"objdump",
        b"windows+pe" => tool.is_empty() || matches!(tool.as_slice(), b"dumpbin" | b"objdump"),
        _ => {
            return Err(error(
                [
                    b"Invalid value for CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM: ".as_slice(),
                    platform,
                ]
                .concat(),
            ));
        }
    };
    if !valid_tool {
        return Err(error(
            [
                b"Invalid value for CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL: ".as_slice(),
                &tool,
            ]
            .concat(),
        ));
    }

    let mut scan = Scan::new(cx, &args);
    if !INPUTS.iter().any(|key| !args.multi(key).is_empty()) {
        return scan.report(&args);
    }
    if matches!(platform, b"linux+elf" | b"freebsd+elf") {
        elf::scan(&mut scan, &args, platform)?;
    } else {
        if platform != b"macos+macho" {
            return Err(error(
                [
                    b"Runtime dependency scanning is not supported for platform ".as_slice(),
                    platform,
                ]
                .concat(),
            ));
        }
        let mut tool = Otool::find(scan.cx)?;
        let bundle = option(&args, "BUNDLE_EXECUTABLE").unwrap_or_default();
        for key in INPUTS {
            for input in args.multi(key) {
                let executable = if key == "EXECUTABLES" {
                    input.as_slice()
                } else {
                    bundle
                };
                let image = tool.image(scan.cx, input)?;
                scan.visit(
                    &mut tool,
                    input,
                    &image,
                    Lexical::new(executable).parent(),
                    &[],
                    0,
                )?;
            }
        }
    }
    scan.report(&args)
}

fn option<'a>(args: &'a Parsed, key: &str) -> Option<&'a [u8]> {
    args.single(key)
        .filter(|value| !value.is_empty())
        .map(|value| value.as_slice())
}

#[derive(Default)]
struct Image {
    libs: Vec<BString>,
    rpaths: Arc<[BString]>,
}

struct Otool {
    command: CommandSpec,
    images: HashMap<BString, Arc<Image>>,
}

impl Otool {
    fn find(cx: &Cx<'_>) -> Result<Self> {
        let command = cx.var_or_empty(b"CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND");
        let spec = if command.is_empty() {
            let path = executable_on_absolute_path(cx.host(), "otool")
                .ok_or_else(|| error("Could not find otool"))?;
            CommandSpec {
                program: from_native(&path),
                args: Vec::new(),
            }
        } else {
            let mut words = list::split(&command, list::Empty::Drop).into_iter();
            let program = words.next().ok_or_else(|| error("Could not find otool"))?;
            CommandSpec {
                program,
                args: words.collect(),
            }
        };
        Ok(Self {
            command: spec,
            images: HashMap::new(),
        })
    }

    fn image(&mut self, cx: &Cx<'_>, path: &[u8]) -> Result<Arc<Image>> {
        if let Some(image) = self.images.get(path) {
            return Ok(Arc::clone(image));
        }
        let image = Arc::new(self.read(cx, path)?);
        self.images.insert(path.into(), Arc::clone(&image));
        Ok(image)
    }

    fn read(&self, cx: &Cx<'_>, file: &[u8]) -> Result<Image> {
        let failure = |message: &[u8]| error([message, b":\n  ", file].concat());
        let host = cx.host();
        let mut command = self.command.clone();
        let operand = if file.starts_with(b"-") {
            from_native(&host.initial_cwd().join(to_native(file)))
        } else {
            BString::from(file)
        };
        command.args.extend(["-l".into(), operand]);
        let output = host
            .run(
                &RunRequest {
                    pipeline: vec![command],
                    cwd: host.initial_cwd(),
                    env: host.env_all(),
                    stdin: Input::Null,
                    stdout: Output::Capture,
                    stderr: Output::Null,
                    timeout: Some(Duration::from_secs(60)),
                    tee: false,
                },
                &mut |_, _| {},
            )
            .map_err(|_| failure(b"Failed to start otool process for"))?;
        match output.results.as_slice() {
            [ProcessResult::Exited(0)] => {}
            [ProcessResult::SpawnFailed(_)] => {
                return Err(failure(b"Failed to start otool process for"));
            }
            _ => return Err(failure(b"Failed to run otool on")),
        }
        if output.stdout.len() > cx.limits().max_value_bytes {
            return Err(error("otool output exceeds the size limit"));
        }
        parse_otool(&output.stdout).ok_or_else(|| error("Invalid output from otool"))
    }
}

fn parse_otool(bytes: &[u8]) -> Option<Image> {
    let mut libs = Vec::new();
    let mut rpaths = Vec::new();
    let mut lines = bytes
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).trim_ascii_start());
    while let Some(line) = lines.next() {
        let (values, prefix) = match line {
            b"cmd LC_RPATH" => (&mut rpaths, b"path ".as_slice()),
            b"cmd LC_LOAD_DYLIB" | b"cmd LC_LOAD_WEAK_DYLIB" => (&mut libs, b"name ".as_slice()),
            _ => continue,
        };
        lines.next()?;
        let value = lines.next()?.strip_prefix(prefix)?.strip_suffix(b")")?;
        let offset = value
            .windows(b" (offset ".len())
            .rposition(|part| part == b" (offset ")?;
        let number = value.get(offset + b" (offset ".len()..)?;
        if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
            return None;
        }
        values.push(value.get(..offset)?.into());
    }
    Some(Image {
        libs,
        rpaths: rpaths.into(),
    })
}

type Regexes = Vec<Arc<Regex>>;

struct Filters<'a> {
    pre_include: Regexes,
    pre_exclude: Regexes,
    post_include: Regexes,
    post_exclude: Regexes,
    include_files: &'a [BString],
    exclude_files: &'a [BString],
    exclude_files_strict: &'a [BString],
    budget: u64,
}

impl<'a> Filters<'a> {
    fn new(cx: &mut Cx<'_>, args: &'a Parsed) -> Self {
        let mut regexes = |key: &str| {
            args.multi(key)
                .iter()
                .filter_map(|pattern| match cx.regex(pattern) {
                    Ok(regex) => Some(regex),
                    Err(error) => {
                        cx.host().write_out(
                            Stream::Stdout,
                            format!(
                                "{}\nRegularExpression::compile(): Error in compile.\n",
                                error.message
                            )
                            .as_bytes(),
                        );
                        None
                    }
                })
                .collect()
        };
        Self {
            pre_include: regexes("PRE_INCLUDE_REGEXES"),
            pre_exclude: regexes("PRE_EXCLUDE_REGEXES"),
            post_include: regexes("POST_INCLUDE_REGEXES"),
            post_exclude: regexes("POST_EXCLUDE_REGEXES"),
            include_files: args.multi("POST_INCLUDE_FILES"),
            exclude_files: args.multi("POST_EXCLUDE_FILES"),
            exclude_files_strict: args.multi("POST_EXCLUDE_FILES_STRICT"),
            budget: cx.limits().regex_steps,
        }
    }

    fn pre_excluded(&mut self, name: &[u8]) -> Result<bool> {
        Ok(!matches_regex(&self.pre_include, name, &mut self.budget)?
            && matches_regex(&self.pre_exclude, name, &mut self.budget)?)
    }

    fn post_excluded(&mut self, cx: &Cx<'_>, path: &[u8]) -> Result<bool> {
        let matches_file = |files: &[BString]| {
            let file = identity(cx, path);
            file.is_some() && files.iter().any(|other| identity(cx, other) == file)
        };
        Ok(matches_file(self.exclude_files_strict)
            || (!(matches_regex(&self.post_include, path, &mut self.budget)?
                || matches_file(self.include_files))
                && (matches_regex(&self.post_exclude, path, &mut self.budget)?
                    || matches_file(self.exclude_files))))
    }
}

fn matches_regex(regexes: &[Arc<Regex>], subject: &[u8], budget: &mut u64) -> Result<bool> {
    for regex in regexes {
        if regex
            .find_at(subject, 0, MatchOptions::default(), budget)
            .map_err(|err| error(err.to_string()))?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

struct Resolved {
    file: Option<(u64, u64)>,
    rpaths: Option<Arc<[BString]>>,
}

struct Scan<'cx, 'eval, 'args> {
    cx: &'cx mut Cx<'eval>,
    filters: Filters<'args>,
    resolved: BTreeMap<BString, BTreeMap<BString, Resolved>>,
    unresolved: BTreeSet<BString>,
}

impl<'cx, 'eval, 'args> Scan<'cx, 'eval, 'args> {
    fn new(cx: &'cx mut Cx<'eval>, args: &'args Parsed) -> Self {
        Self {
            filters: Filters::new(cx, args),
            cx,
            resolved: BTreeMap::new(),
            unresolved: BTreeSet::new(),
        }
    }

    fn visit(
        &mut self,
        tool: &mut Otool,
        path: &[u8],
        image: &Image,
        executable: &[u8],
        inherited: &[BString],
        depth: u32,
    ) -> Result<()> {
        if depth > self.cx.limits().max_recursion.min(256) {
            return Err(error("Runtime dependency scan exceeds the depth limit"));
        }
        let loader = Lexical::new(path).parent();
        let mut rpaths = image.rpaths.to_vec();
        rpaths.extend_from_slice(inherited);
        for name in &image.libs {
            if self.filters.pre_excluded(name)? {
                continue;
            }
            let Some(path) = resolve_macho(self.cx, name, executable, loader, &rpaths)? else {
                self.unresolved.insert(name.clone());
                continue;
            };
            if self.filters.post_excluded(self.cx, &path)? || missing_system_dylib(self.cx, &path) {
                continue;
            }
            let image = tool.image(self.cx, &path)?;
            let name = Lexical::new(&path).filename();
            let paths = self.resolved.entry(name.into()).or_default();
            let file = identity(self.cx, &path);
            let seen = paths.contains_key(&path)
                || file.is_some() && paths.values().any(|entry| entry.file == file);
            paths.insert(
                path.clone(),
                Resolved {
                    file,
                    rpaths: Some(Arc::clone(&image.rpaths)),
                },
            );
            if !seen {
                self.visit(tool, &path, &image, executable, &rpaths, depth + 1)?;
            }
        }
        Ok(())
    }

    fn report(self, args: &Parsed) -> Result {
        let resolved_var = option(args, "RESOLVED_DEPENDENCIES_VAR");
        let unresolved_var = option(args, "UNRESOLVED_DEPENDENCIES_VAR");
        let conflict_prefix = option(args, "CONFLICTING_DEPENDENCIES_PREFIX");
        let rpath_prefix = option(args, "RPATH_PREFIX");
        let mut resolved = Vec::new();
        let mut conflicts = Vec::new();
        for (name, paths) in &self.resolved {
            let mut values = paths.iter();
            let Some((first, entry)) = values.next() else {
                continue;
            };
            if values.all(|(_, other)| entry.file.is_some() && entry.file == other.file) {
                resolved.push(first);
                if let Some(prefix) = rpath_prefix {
                    set(
                        self.cx,
                        &[prefix, b"_", first].concat(),
                        list::join(entry.rpaths.as_deref().unwrap_or_default()),
                    )?;
                }
            } else if let Some(prefix) = conflict_prefix {
                conflicts.push(name);
                set(
                    self.cx,
                    &[prefix, b"_", name].concat(),
                    list::join(paths.keys()),
                )?;
            } else {
                return Err(path_error(
                    [
                        b"Multiple conflicting paths found for ".as_slice(),
                        name,
                        b":",
                    ]
                    .concat(),
                    paths.keys(),
                ));
            }
        }
        if !self.unresolved.is_empty() && unresolved_var.is_none() {
            return Err(path_error(
                b"Could not resolve runtime dependencies:".to_vec(),
                &self.unresolved,
            ));
        }
        if let Some(variable) = resolved_var {
            set(self.cx, variable, list::join(resolved))?;
        }
        if let Some(variable) = unresolved_var {
            set(self.cx, variable, list::join(&self.unresolved))?;
        }
        if let Some(prefix) = conflict_prefix {
            set(
                self.cx,
                &[prefix, b"_FILENAMES"].concat(),
                list::join(conflicts),
            )?;
        }
        Ok(Flow::Normal)
    }
}

fn path_error<'a>(
    mut message: Vec<u8>,
    paths: impl IntoIterator<Item = &'a BString>,
) -> rcmake_eval::CmdError {
    for path in paths {
        message.extend_from_slice(b"\n  ");
        message.extend_from_slice(path);
    }
    error(message)
}

fn native(cx: &Cx<'_>, path: &[u8]) -> PathBuf {
    cx.host().initial_cwd().join(to_native(path))
}

fn identity(cx: &Cx<'_>, path: &[u8]) -> Option<(u64, u64)> {
    let meta = cx.host().metadata(&native(cx, path), Follow::Yes).ok()?;
    (meta.dev != 0 || meta.ino != 0).then_some((meta.dev, meta.ino))
}

fn exists(cx: &Cx<'_>, path: &[u8]) -> bool {
    cx.host().metadata(&native(cx, path), Follow::No).is_ok()
}

fn missing_system_dylib(cx: &Cx<'_>, path: &[u8]) -> bool {
    (path.starts_with(b"/System/Library/") || path.starts_with(b"/usr/lib/")) && !exists(cx, path)
}

fn resolve_macho(
    cx: &mut Cx<'_>,
    name: &[u8],
    executable: &[u8],
    loader: &[u8],
    rpaths: &[BString],
) -> Result<Option<BString>> {
    fn replace_location(path: &[u8], executable: &[u8], loader: &[u8]) -> Option<BString> {
        for (prefix, directory) in [
            (b"@loader_path/".as_slice(), loader),
            (b"@executable_path/", executable),
        ] {
            if let Some(suffix) = path.strip_prefix(prefix) {
                return (!directory.is_empty()).then(|| [directory, b"/", suffix].concat().into());
            }
        }
        Some(path.into())
    }

    let path = if let Some(suffix) = name.strip_prefix(b"@rpath/") {
        rpaths.iter().find_map(|rpath| {
            let candidate = [rpath.as_slice(), b"/", suffix].concat();
            replace_location(&candidate, executable, loader).filter(|path| exists(cx, path))
        })
    } else {
        let requires_existing =
            name.starts_with(b"@loader_path/") || name.starts_with(b"@executable_path/");
        replace_location(name, executable, loader)
            .filter(|path| !requires_existing || exists(cx, path))
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let path = normalize(cx, path);
    if !Lexical::new(&path).absolute() {
        return Err(error("Resolved path is not absolute"));
    }
    Ok(Some(path))
}

fn normalize(cx: &mut Cx<'_>, path: BString) -> BString {
    let generic = rcmake_base::path_lex::cmake_style(&path);
    let lexical = Lexical::new(&generic);
    let mut normalized = Vec::from(lexical.root_path());
    if lexical.components().next().is_some() {
        for (index, part) in lexical.components().enumerate() {
            if index != 0 {
                normalized.push(b'/');
            }
            normalized.extend_from_slice(part);
        }
        if generic.ends_with(b"/") {
            normalized.push(b'/');
        }
    }
    apply_normalization(cx, path, normalized)
}

fn apply_normalization(cx: &mut Cx<'_>, path: BString, normalized: Vec<u8>) -> BString {
    if path.as_slice() == normalized.as_slice() {
        return path;
    }
    let policy = PolicyId::new(207);
    match cx.policy(policy) {
        PolicyStatus::New => normalized.into(),
        PolicyStatus::Old => path,
        PolicyStatus::Warn => {
            cx.issue_policy_warning(
                policy,
                b"",
                &[
                    b"Path\n  \"".as_slice(),
                    &path,
                    b"\"\nwould be converted to\n  \"",
                    &normalized,
                    b"\"\n",
                ]
                .concat(),
            );
            path
        }
    }
}
