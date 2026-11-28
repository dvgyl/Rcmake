//! `enable_language()` and the language bootstrap that `project()` runs (ADR-008).
//!
//! The bootstrap runs the vendored `Modules/` files in the CMake 4.5.1 order:
//!
//! 1. `CMakeSystem.cmake` (warm) or `CMakeDetermineSystem.cmake` (cold), the generator's
//!    find-make module, `CMAKE_PROJECT_TOP_LEVEL_INCLUDES`, and
//!    `CMakeSystemSpecificInitialize.cmake`.
//! 2. For each new language: `CMake<LANG>Compiler.cmake` (warm) or
//!    `CMakeDetermine<LANG>Compiler.cmake` followed by `CMake<LANG>Compiler.cmake` (cold).
//! 3. `CMakeSystemSpecificInformation.cmake`.
//! 4. For each language: `CMake<LANG>Information.cmake`, `CMakeTest<LANG>Compiler.cmake`
//!    (cold only), and the linker information modules when the language uses them.
//! 5. `${CMAKE_ROOT}/Modules/<PROJECT_NAME>Compatibility.cmake` if it exists.

use std::path::{Path, PathBuf};

use rcmake_base::hash::HashAlgo;
use rcmake_base::list::{self, Empty};
use rcmake_base::truth::is_off;
use rcmake_base::{BString, CMAKE_VERSION_STR};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Input, Output, ProcessResult, ReadRange, RunRequest, Stream,
    WriteMode, path,
};
use rcmake_model::{CacheEntry, CacheType, GeneratorKind, VsVersion};

use crate::policy_misc::properties::known_features;

const CMP0132: PolicyId = PolicyId::new(132);
const CMP0165: PolicyId = PolicyId::new(165);

pub(super) const REQUIRED_NINJA_VERSION: &str = "1.3";

/// Parse the version token from `ninja --version` output.
///
/// Accepts 2+ dot-separated unsigned numeric components (`major.minor[.patch[.tweak]]`),
/// optionally followed by `-<non-empty>`, `+<non-empty>`, ` <non-empty>`, or (after at least
/// `major.minor.patch`) a known Ninja dot-suffix (`.git...` or `.dyndep-<digits>`). Rejects
/// trailing dots and malformed numeric components such as `1.11.` or `1.11.foo`.
pub(super) fn parse_ninja_version(stdout: &[u8]) -> Option<BString> {
    let trimmed = stdout.trim_ascii();
    if trimmed.is_empty() || trimmed.contains(&b'\n') || trimmed.contains(&b'\r') {
        return None;
    }
    let mut pos = 0usize;
    let mut parts = 0usize;
    loop {
        let start = pos;
        while trimmed.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == start {
            return None;
        }
        parts += 1;
        if trimmed.get(pos) == Some(&b'.') {
            if trimmed.get(pos + 1).is_some_and(u8::is_ascii_digit) {
                pos += 1;
                continue;
            }
            break;
        }
        break;
    }
    if parts < 2 {
        return None;
    }
    let rest = trimmed.get(pos..).unwrap_or_default();
    if !rest.is_empty() && !valid_ninja_version_suffix(rest, parts) {
        return None;
    }
    Some(BString::from(trimmed))
}

fn valid_ninja_version_suffix(rest: &[u8], parts: usize) -> bool {
    match rest.first() {
        Some(b'-' | b'+' | b' ') => {
            let tail = rest.get(1..).unwrap_or_default();
            !tail.is_empty() && !tail.starts_with(b".")
        }
        Some(b'.') if parts >= 3 => {
            if let Some(after) = rest.strip_prefix(b".git") {
                after.is_empty()
                    || (matches!(after.first(), Some(b'.' | b'-' | b'+'))
                        && !after.ends_with(b".")
                        && !after.windows(2).any(|w| w == b"..")
                        && after.get(1..).is_some_and(|s| !s.is_empty()))
            } else if let Some(digits) = rest.strip_prefix(b".dyndep-") {
                !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Return whether a parsed Ninja version meets CMake's minimum requirement (`1.3`).
pub(super) fn ninja_version_supported(version: &[u8]) -> bool {
    rcmake_base::version::compare(version, REQUIRED_NINJA_VERSION.as_bytes())
        != std::cmp::Ordering::Less
}

const ENABLED_LANGUAGES: &[u8] = b"ENABLED_LANGUAGES";
const IN_TRY_COMPILE: &[u8] = b"IN_TRY_COMPILE";
const PLATFORM_INFO_DIR: &[u8] = b"CMAKE_PLATFORM_INFO_DIR";
const PLATFORM_INFO_INITIALIZED: &[u8] = b"CMAKE_PLATFORM_INFO_INITIALIZED";
/// Private global property with the languages that an active bootstrap enables.
const LANGUAGES_IN_PROGRESS: &[u8] = b"\0RCMAKE_LANGUAGES_IN_PROGRESS";

/// `enable_language(<lang>... [OPTIONAL])`.
pub(super) fn enable_language(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.is_empty() {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    }
    if cx.var_normal_only(b"PROJECT_NAME").is_none() {
        match cx.policy(CMP0165) {
            PolicyStatus::Warn => cx.issue(
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Author),
                "project() should be called prior to this enable_language() call.".into(),
            ),
            PolicyStatus::New => {
                cx.issue(
                    MessageKind::Error,
                    Effect::Fatal,
                    None,
                    "project() must be called prior to this enable_language() call.".into(),
                );
                return Err(CmdError::Reported);
            }
            PolicyStatus::Old => {}
        }
    }
    let optional = args.iter().any(|arg| arg.value == b"OPTIONAL");
    let languages: Vec<BString> = args
        .iter()
        .filter(|arg| arg.value != b"OPTIONAL")
        .map(|arg| arg.value.clone())
        .collect();
    enable_languages(cx, &languages, optional)?;
    Ok(Flow::Normal)
}

/// Enable `languages` in the current directory (`cmMakefile::EnableLanguage`).
///
/// `project()` calls this with its language list (`NONE` enables no compiler but still
/// determines the target system).
///
/// # Errors
///
/// Returns [`CmdError::Reported`] after a fatal error in a bootstrap module or a fatal
/// configuration error (for example, a missing compiler for a required language).
pub(crate) fn enable_languages(
    cx: &mut Cx<'_>,
    languages: &[BString],
    optional: bool,
) -> Result<(), CmdError> {
    let nested = cx
        .global()
        .props
        .get(LANGUAGES_IN_PROGRESS)
        .is_some_and(|value| !value.is_empty());
    #[cfg(all(feature = "native", any(unix, windows)))]
    if !nested {
        super::try_compile::begin_probe_prelude(cx);
    }
    cx.begin_replay_setup();
    let result = if nested {
        setup_languages(cx, languages, optional)
    } else {
        cx.with_language_scope(|cx| setup_languages(cx, languages, optional))
    };
    cx.end_replay_setup(result.is_ok());
    result
}

fn setup_languages(cx: &mut Cx<'_>, languages: &[BString], optional: bool) -> Result<(), CmdError> {
    let generator = generator(cx);
    super::generator_env::initialize(cx, generator)?;
    if let GeneratorKind::VisualStudio(version) = generator {
        init_vs_ide_vars(cx, version);
    }
    if generator == GeneratorKind::NMakeMakefiles {
        cx.set_var(b"CMAKE_GENERATOR_CC", "cl".into());
        cx.set_var(b"CMAKE_GENERATOR_CXX", "cl".into());
    }
    cx.set_var(b"CMAKE_CFG_INTDIR", generator.cfg_int_dir().into());

    let mut unique: Vec<BString> = Vec::new();
    let mut duplicates: Vec<BString> = Vec::new();
    for language in languages {
        if !unique.contains(language) {
            unique.push(language.clone());
        } else if !duplicates.contains(language) {
            duplicates.push(language.clone());
        }
    }
    if !duplicates.is_empty() {
        let quantity: &[u8] = if duplicates.len() == 1 {
            b" has"
        } else {
            b"s have"
        };
        let body = [
            b"Languages to be enabled may not be specified more than once at the same time. \
              The following language"
                .as_slice(),
            quantity,
            b" been specified multiple times: ",
            &duplicates.join(b", ".as_slice()),
        ]
        .concat();
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            body.into(),
        );
    }

    // `cmGlobalGenerator::LanguagesInProgress`: a toolchain file or bootstrap module
    // must not enable a language that is still being enabled.
    let in_progress = cx
        .global()
        .props
        .get(LANGUAGES_IN_PROGRESS)
        .map(|value| list::split(value, Empty::Drop))
        .unwrap_or_default();
    if let Some(lang) = unique.iter().find(|lang| in_progress.contains(lang)) {
        let body = format!(
            "Language '{}' is currently being enabled.  Recursive call not allowed.",
            String::from_utf8_lossy(lang)
        );
        cx.issue(MessageKind::Error, Effect::Fatal, None, body.into());
        return Err(CmdError::Reported);
    }
    // Mark all requested languages active before reading the toolchain file so a toolchain
    // calling `enable_language(RC)` during `project(P C RC)` is caught as recursive.
    // Once system/toolchain determination completes, `RC` is enabled in a second pass after
    // non-RC languages so platform modules (e.g. `Windows-MSVC.cmake`) can implicitly call
    // `enable_language(RC)` cleanly.
    let (rc, other): (Vec<BString>, Vec<BString>) =
        unique.into_iter().partition(|lang| lang == b"RC");
    let mut result = Ok(());
    if !other.is_empty() && !rc.is_empty() {
        let all_active = list::join(in_progress.iter().chain(&other).chain(&rc));
        let other_active = list::join(in_progress.iter().chain(&other));
        cx.global()
            .props
            .set(LANGUAGES_IN_PROGRESS.into(), all_active);
        result = Bootstrap::new(cx, generator, optional).run(cx, other, Some(&other_active));
        if result.is_ok() {
            let rc_active = list::join(in_progress.iter().chain(&rc));
            cx.global()
                .props
                .set(LANGUAGES_IN_PROGRESS.into(), rc_active);
            result = Bootstrap::new(cx, generator, optional).run(cx, rc, None);
        }
    } else {
        let group = if !other.is_empty() { other } else { rc };
        if !group.is_empty() {
            let active = list::join(in_progress.iter().chain(&group));
            cx.global().props.set(LANGUAGES_IN_PROGRESS.into(), active);
            result = Bootstrap::new(cx, generator, optional).run(cx, group, None);
        }
    }
    cx.global()
        .props
        .set(LANGUAGES_IN_PROGRESS.into(), list::join(in_progress));
    result
}

/// The generator of this configure run: the project model's generator, then
/// `CMAKE_GENERATOR`, then `Unix Makefiles`.
fn generator(cx: &mut Cx<'_>) -> GeneratorKind {
    let from_model = cx.project().generator;
    from_model
        .or_else(|| GeneratorKind::from_name(&cx.var_or_empty(b"CMAKE_GENERATOR")))
        .unwrap_or(GeneratorKind::UnixMakefiles)
}

/// One `cmGlobalGenerator::EnableLanguage` call.
struct Bootstrap {
    generator: GeneratorKind,
    optional: bool,
    in_try_compile: bool,
    root_bin: BString,
}

/// Per-language state between the compiler-determination and information passes.
struct LanguageState {
    name: BString,
    need_test: bool,
}

impl Bootstrap {
    fn new(cx: &mut Cx<'_>, generator: GeneratorKind, optional: bool) -> Self {
        seed_global_properties(cx);
        let in_try_compile = cx
            .global()
            .props
            .get(IN_TRY_COMPILE)
            .is_some_and(|value| rcmake_base::truth::is_on(value));
        let root_bin = match cx.var(PLATFORM_INFO_DIR) {
            Some(dir) if in_try_compile => dir,
            _ => cat(&[
                &cx.var_or_empty(b"CMAKE_BINARY_DIR"),
                b"/CMakeFiles/",
                CMAKE_VERSION_STR.as_bytes(),
            ]),
        };
        Self {
            generator,
            optional,
            in_try_compile,
            root_bin,
        }
    }

    fn run(
        &self,
        cx: &mut Cx<'_>,
        mut languages: Vec<BString>,
        post_system_in_progress: Option<&BString>,
    ) -> Result<(), CmdError> {
        if !self.generator.multi_config()
            && !self.in_try_compile
            && !cx.var_defined(b"CMAKE_BUILD_TYPE")
            && let Some(build_type) = cx.host().env_get(b"CMAKE_BUILD_TYPE")
        {
            add_cache(
                cx,
                b"CMAKE_BUILD_TYPE",
                build_type,
                CacheType::String,
                "Choose the type of build.  Options include: empty, Debug, Release, \
                 RelWithDebInfo, MinSizeRel.",
            );
        }
        order_sycl(&mut languages);

        if !self.has_modules(cx) && languages.iter().all(|lang| lang == b"NONE") {
            self.enable_without_modules(cx);
            return Ok(());
        }

        cx.set_var(b"RUN_CONFIGURE", "ON".into());
        cx.set_var(PLATFORM_INFO_DIR, self.root_bin.clone());
        if !self.in_try_compile {
            let dir = path::to_native(&self.root_bin);
            if is_dir(cx, &dir) && !cx.var_is_on(PLATFORM_INFO_INITIALIZED) {
                // A replaced CMakeCache.txt invalidates the saved platform information.
                let _ = cx.host().remove_dir_all(&dir);
            }
            add_cache(
                cx,
                PLATFORM_INFO_INITIALIZED,
                "1".into(),
                CacheType::Internal,
                "Platform information initialized",
            );
        }
        let platform_dir = path::to_native(&self.root_bin);
        if !is_dir(cx, &platform_dir) {
            let _ = cx.host().create_dir_all(&platform_dir);
        }

        self.determine_system(cx)?;
        if let Some(active) = post_system_in_progress {
            cx.global()
                .props
                .set(LANGUAGES_IN_PROGRESS.into(), active.clone());
        }

        let mut states = Vec::with_capacity(languages.len());
        for name in languages {
            let need_test = self.determine_compiler(cx, &name)?;
            states.push(LanguageState { name, need_test });
        }

        if !cx.var_defined(b"CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED") {
            self.require_module(cx, "CMakeSystemSpecificInformation.cmake")?;
        }

        let mut fatal = false;
        for state in &mut states {
            fatal |= self.load_information(cx, state)?;
        }

        let compatibility = cat(&[
            &cx.var_or_empty(b"CMAKE_ROOT"),
            b"/Modules/",
            &cx.var_or_empty(b"PROJECT_NAME"),
            b"Compatibility.cmake",
        ]);
        if exists(cx, &path::to_native(&compatibility)) {
            read_list_file(cx, &compatibility)?;
        }

        if fatal {
            cx.set_fatal();
            return Err(CmdError::Reported);
        }
        Ok(())
    }

    /// `true` when `${CMAKE_ROOT}/Modules` holds the platform bootstrap modules.
    fn has_modules(&self, cx: &mut Cx<'_>) -> bool {
        let file = cat(&[
            &cx.var_or_empty(b"CMAKE_ROOT"),
            b"/Modules/CMakeDetermineSystem.cmake",
        ]);
        exists(cx, &path::to_native(&file))
    }

    /// `project(... NONE)` in a session without a `Modules/` tree (for example, an
    /// in-memory host): record `NONE` and describe the target system from the host.
    fn enable_without_modules(&self, cx: &mut Cx<'_>) {
        set_language_enabled(cx, b"NONE");
        let system = cx.host().system();
        if !cx.var_defined(b"CMAKE_SYSTEM_NAME") {
            cx.set_var(b"CMAKE_SYSTEM_NAME", system.sysname);
        }
        if !cx.var_defined(b"CMAKE_SYSTEM_PROCESSOR") {
            cx.set_var(b"CMAKE_SYSTEM_PROCESSOR", system.machine);
        }
    }

    /// Load or determine the target system, find the build tool, and run the top-level
    /// includes and `CMakeSystemSpecificInitialize.cmake`.
    fn determine_system(&self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        let system_file = cat(&[&self.root_bin, b"/CMakeSystem.cmake"]);
        let read_system = !cx.var_defined(b"CMAKE_SYSTEM_LOADED");
        if read_system && exists(cx, &path::to_native(&system_file)) {
            read_list_file(cx, &system_file)?;
            if !self.in_try_compile && super::selection_change::toolchain_changed(cx) {
                cx.request_exit(0);
                return Err(CmdError::Reported);
            }
        }
        if !cx.var_defined(b"CMAKE_SYSTEM") {
            cx.note_replay_determination();
            if let Some(module) = modules_file(cx, b"CMakeDetermineSystem.cmake") {
                read_list_file(cx, &module)?;
            }
            read_list_file(cx, &system_file)?;
        }
        if read_system {
            if !self.generator.is_ide() {
                for (var, what) in [
                    (b"CMAKE_GENERATOR_INSTANCE".as_slice(), "instance"),
                    (b"CMAKE_GENERATOR_PLATFORM", "platform"),
                    (b"CMAKE_GENERATOR_TOOLSET", "toolset"),
                ] {
                    let value = cx.var_or_empty(var);
                    if !value.is_empty() {
                        let body = cat(&[
                            b"Generator\n  ",
                            self.generator.name().as_bytes(),
                            format!("\ndoes not support {what} specification, but {what}\n  ")
                                .as_bytes(),
                            &value,
                            b"\nwas specified.",
                        ]);
                        cx.issue(MessageKind::Error, Effect::Fatal, None, body);
                        return Err(CmdError::Reported);
                    }
                }
            }
            self.find_make_program(cx)?;
            top_level_includes(cx)?;
        }
        if !cx.var_defined(b"CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED") {
            self.require_module(cx, "CMakeSystemSpecificInitialize.cmake")?;
        }
        Ok(())
    }

    /// `cmGlobalGenerator::FindMakeProgram`.
    fn find_make_program(&self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        if let GeneratorKind::VisualStudio(version) = self.generator {
            super::generator_env::configure_msbuild(cx, version);
        }
        let module = self
            .generator
            .find_make_module()
            .map(|module| format!("{module}.cmake"));
        if is_off(&cx.var_or_empty(b"CMAKE_MAKE_PROGRAM"))
            && let Some(module) = module
            && let Some(file) = modules_file(cx, module.as_bytes())
        {
            read_list_file(cx, &file)?;
        }
        if is_off(&cx.var_or_empty(b"CMAKE_MAKE_PROGRAM")) {
            return Err(system_error(
                cx,
                &format!(
                    "CMake was unable to find a build program corresponding to \"{}\".  \
                     CMAKE_MAKE_PROGRAM is not set.  You probably need to select a \
                     different build tool.",
                    self.generator.name()
                ),
            ));
        }
        if matches!(
            self.generator,
            GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig
        ) {
            let program = cx.var_or_empty(b"CMAKE_MAKE_PROGRAM");
            self.verify_ninja_program(cx, &program)?;
        } else if self.generator == GeneratorKind::NMakeMakefiles {
            let program = cx.var_or_empty(b"CMAKE_MAKE_PROGRAM");
            self.verify_nmake_program(cx, &program)?;
        }
        Ok(())
    }

    /// `cmGlobalNMakeMakefileGenerator::FindMakeProgram`: verify that the selected
    /// `nmake` executable runs `-?`.
    fn verify_nmake_program(&self, cx: &mut Cx<'_>, program: &BString) -> Result<(), CmdError> {
        if !cx.host().supports_process_execution() {
            return Ok(());
        }
        let request = rcmake_host::RunRequest {
            pipeline: vec![rcmake_host::CommandSpec {
                program: program.clone(),
                args: vec!["-?".into()],
            }],
            cwd: cx.current_binary_dir(),
            env: cx.host().env_all(),
            stdin: rcmake_host::Input::Null,
            stdout: rcmake_host::Output::Capture,
            stderr: rcmake_host::Output::Capture,
            timeout: Some(std::time::Duration::from_secs(30)),
            tee: false,
            default_error_mode: false,
        };
        let error = match cx.host().run_bounded(&request, 64 * 1024) {
            Ok(output) if output.results == [rcmake_host::ProcessResult::Exited(0)] => {
                return Ok(());
            }
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stderr);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    trimmed.to_owned()
                } else {
                    let stdout_text = String::from_utf8_lossy(&output.stdout);
                    let stdout_trimmed = stdout_text.trim();
                    if !stdout_trimmed.is_empty() {
                        stdout_trimmed.to_owned()
                    } else {
                        match output.results.as_slice() {
                            [rcmake_host::ProcessResult::SpawnFailed(err)] => err.to_string(),
                            _ => format!("process exited with {:?}", output.results),
                        }
                    }
                }
            }
            Err(err)
                if err.kind == std::io::ErrorKind::Unsupported
                    && err.os_message == "No command handler is registered" =>
            {
                return Ok(());
            }
            Err(err) => err.to_string(),
        };
        let body = format!("Running\n '{program}' '-?'\nfailed with:\n {error}");
        cx.issue(MessageKind::Error, Effect::Fatal, None, body.into());
        Err(CmdError::Reported)
    }

    /// Warm the probe memo in the background while the compilers are identified.
    #[cfg(all(feature = "native", any(unix, windows)))]
    fn prefetch_toolchain(&self, cx: &mut Cx<'_>, lang: &[u8]) {
        if self.in_try_compile || !matches!(lang, b"C" | b"CXX" | b"OBJC" | b"OBJCXX") {
            return;
        }
        let sysroot = [b"_CMAKE_OSX_SYSROOT_PATH".as_slice(), b"CMAKE_OSX_SYSROOT"]
            .into_iter()
            .map(|name| path::to_native(&cx.var_or_empty(name)))
            .find(|path| path.is_absolute());
        if let Some(runtime) =
            cx.session_data::<std::sync::Arc<super::try_compile::active::ProbeRuntime>>()
        {
            runtime.prefetch_toolchain(sysroot);
        }
    }

    /// `cmGlobalNinjaGenerator::FindMakeProgram`: verify that the selected Ninja
    /// executable runs `--version` and meets the minimum required version.
    fn verify_ninja_program(&self, cx: &mut Cx<'_>, program: &BString) -> Result<(), CmdError> {
        if !cx.host().supports_process_execution() {
            return Ok(());
        }
        let cwd = cx.current_binary_dir();
        let raw_env = cx.host().env_all();
        // The session memo checks fresh metadata on every use. Verification
        // mode uses a memo that hashes every time.
        #[cfg(all(feature = "native", any(unix, windows)))]
        let memo = cx
            .session_data::<std::sync::Arc<super::try_compile::active::ProbeRuntime>>()
            .map(|runtime| std::sync::Arc::clone(runtime.trace_memo()));
        #[cfg(all(feature = "native", any(unix, windows)))]
        let program_digest = |host: &dyn rcmake_host::Host, path: &std::path::Path| {
            memo.as_ref().map_or_else(
                || rcmake_store::file_digest(host, path),
                |memo| memo.file_digest(host, path),
            )
        };
        #[cfg(all(feature = "native", any(unix, windows)))]
        let (request_env, cached_key) = {
            let native = path::to_native(program);
            // Check session `BuildToolFacts` first so journaled `Observe`/`Speculate`
            // children (which omit `BuildToolFacts`) go straight to `run_bounded` without
            // reading the Ninja binary through `JournalHost`.
            if let Some(facts) =
                cx.session_data::<std::sync::Arc<super::build_tool_facts::BuildToolFacts>>()
                && native.is_absolute()
                && super::try_compile::active::admit_environment(&raw_env)
                && cx
                    .host()
                    .canonicalize(&native)
                    .ok()
                    .is_some_and(|canonical| {
                        super::build_tool_facts::is_native_ninja(cx.host(), &canonical)
                    })
                && let Ok(digest) = program_digest(cx.host(), &native)
            {
                let mut normalized = raw_env;
                super::try_compile::plan::process_environment(&mut normalized);
                let env_digest =
                    super::try_compile::plan::process_environment_fingerprint(&normalized);
                if facts
                    .get_verified_ninja(&native, digest, env_digest, &cwd)
                    .is_some()
                {
                    return Ok(());
                }
                (normalized, Some((facts, native, digest, env_digest)))
            } else {
                (raw_env, None)
            }
        };
        #[cfg(not(all(feature = "native", any(unix, windows))))]
        let request_env = raw_env;
        let request = rcmake_host::RunRequest {
            pipeline: vec![rcmake_host::CommandSpec {
                program: program.clone(),
                args: vec!["--version".into()],
            }],
            cwd: cwd.clone(),
            env: request_env,
            stdin: rcmake_host::Input::Null,
            stdout: rcmake_host::Output::Capture,
            stderr: rcmake_host::Output::Capture,
            timeout: Some(std::time::Duration::from_secs(5)),
            tee: false,
            default_error_mode: false,
        };
        let error = match cx.host().run_bounded(&request, 4096) {
            Ok(output) if output.results == [rcmake_host::ProcessResult::Exited(0)] => {
                let version = parse_ninja_version(&output.stdout);
                if let Some(version) = version.filter(|version| ninja_version_supported(version)) {
                    #[cfg(all(feature = "native", any(unix, windows)))]
                    if let Some((facts, native, digest, env_digest)) = cached_key
                        && program_digest(cx.host(), &native).ok() == Some(digest)
                    {
                        facts.record_ninja(native, digest, env_digest, cwd, output, version);
                    }
                    #[cfg(not(all(feature = "native", any(unix, windows))))]
                    let _ = version;
                    return Ok(());
                }
                let detected = String::from_utf8_lossy(output.stdout.trim_ascii());
                let body = format!(
                    "The detected version of Ninja ({detected}) is less than the version of Ninja required by CMake ({REQUIRED_NINJA_VERSION})."
                );
                cx.issue(MessageKind::Error, Effect::Fatal, None, body.into());
                return Err(CmdError::Reported);
            }
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stderr);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    format!("process exited with {:?}", output.results)
                } else {
                    trimmed.to_owned()
                }
            }
            Err(err)
                if err.kind == std::io::ErrorKind::Unsupported
                    && err.os_message == "No command handler is registered" =>
            {
                return Ok(());
            }
            Err(err) => err.to_string(),
        };
        let body = format!("Running\n '{program}' '--version'\nfailed with:\n {error}");
        cx.issue(MessageKind::Error, Effect::Fatal, None, body.into());
        Err(CmdError::Reported)
    }

    /// First pass for one language: load the saved compiler file or determine the
    /// compiler. Returns `true` when the compiler still needs its working test.
    fn determine_compiler(&self, cx: &mut Cx<'_>, lang: &[u8]) -> Result<bool, CmdError> {
        if lang == b"NONE" {
            set_language_enabled(cx, b"NONE");
            return Ok(false);
        }
        let compiler_file = cat(&[&self.root_bin, b"/CMake", lang, b"Compiler.cmake"]);
        let mut need_test = false;
        if (!language_enabled(cx, lang) || !cx.var_defined(&lang_var(lang, b"_COMPILER_LOADED")))
            && exists(cx, &path::to_native(&compiler_file))
        {
            let compiler = lang_var(lang, b"_COMPILER");
            let arg_key = lang_var(lang, b"_COMPILER_ARG1");
            let requested = cx.var(&compiler);
            let args = cx.var(&arg_key);
            if !read_list_file(cx, &compiler_file)? {
                return Err(missing_module(cx, &compiler_file));
            }
            if !self.in_try_compile {
                super::selection_change::record_compiler(cx, &compiler, &arg_key, requested, args);
            }
            set_language_enabled(cx, lang);
        }
        if !language_enabled(cx, lang) {
            if self.in_try_compile {
                return Err(system_error(
                    cx,
                    "This should not have happened. If you see this message, you are \
                     probably using a broken CMakeLists.txt file or a problematic release of \
                     CMake",
                ));
            }
            let explicit_compiler = !is_off(&cx.var_or_empty(&lang_var(lang, b"_COMPILER")));
            let determine = cat(&[b"CMakeDetermine", lang, b"Compiler.cmake"]);
            cx.note_replay_determination();
            #[cfg(all(feature = "native", any(unix, windows)))]
            self.prefetch_toolchain(cx, lang);
            self.require_module(cx, &String::from_utf8_lossy(&determine))?;
            self.export_compiler_env(cx, lang);
            if !read_list_file(cx, &compiler_file)? {
                return Err(missing_module(cx, &compiler_file));
            }
            if lang == b"CSharp"
                && !explicit_compiler
                && is_off(&cx.var_or_empty(&lang_var(lang, b"_COMPILER")))
            {
                discover_csharp_compiler_fallback(cx, &self.root_bin);
            }
            record_identification(cx, &self.root_bin, lang);
            set_language_enabled(cx, lang);
            need_test = true;
        }
        Ok(need_test)
    }

    /// With `CMP0132` not `NEW`, put `<CMAKE_<LANG>_COMPILER_ENV_VAR>=<compiler>` into the
    /// environment for user scripts and child processes.
    fn export_compiler_env(&self, cx: &mut Cx<'_>, lang: &[u8]) {
        if cx.policy(CMP0132) == PolicyStatus::New
            || cx.var_defined(b"CMAKE_GENERATOR_NO_COMPILER_ENV")
        {
            return;
        }
        let env_var = cx.var_or_empty(&lang_var(lang, b"_COMPILER_ENV_VAR"));
        let compiler = cx.var(&lang_var(lang, b"_COMPILER"));
        if let (false, Some(compiler)) = (env_var.is_empty(), compiler) {
            cx.host().env_set(&env_var, Some(&compiler));
        }
    }

    /// Second pass for one language: check the compiler, load the language information,
    /// test the compiler, and load the linker information. Returns `true` when a missing
    /// compiler makes the configure step fail.
    fn load_information(
        &self,
        cx: &mut Cx<'_>,
        state: &mut LanguageState,
    ) -> Result<bool, CmdError> {
        let lang = state.name.clone();
        if lang == b"NONE" {
            set_language_enabled(cx, b"NONE");
            return Ok(false);
        }
        let compiler_file = cat(&[&self.root_bin, b"/CMake", &lang, b"Compiler.cmake"]);
        let mut fatal = false;
        let problem = self.compiler_problem(cx, &lang);
        let compiler_available = problem.is_none();
        if let Some(problem) = problem {
            state.need_test = false;
            if !self.optional {
                let _ = cx.host().remove_file(&path::to_native(&compiler_file));
                if !self.in_try_compile {
                    let env_var = cx.var(&lang_var(&lang, b"_COMPILER_ENV_VAR"));
                    let body = cat(&[
                        &problem,
                        &compiler_advice(
                            self.generator,
                            &lang,
                            env_var.as_deref().map(Vec::as_slice),
                        ),
                    ]);
                    cx.issue(MessageKind::Error, Effect::SetError, None, body);
                    fatal = true;
                }
            }
        }

        if !cx.var_defined(&lang_var(&lang, b"_INFORMATION_LOADED")) {
            let name = cat(&[b"CMake", &lang, b"Information.cmake"]);
            match modules_file(cx, &name) {
                None => return Err(missing_module(cx, &name)),
                Some(file) => {
                    if !read_list_file(cx, &file)? {
                        return Err(system_error(
                            cx,
                            &format!(
                                "Could not process cmake module file: {}",
                                String::from_utf8_lossy(&file)
                            ),
                        ));
                    }
                }
            }
        }

        let target_option = lang_var(&lang, b"_COMPILE_OPTIONS_TARGET");
        if cx.var_or_empty(b"CMAKE_HOST_SYSTEM_NAME") == "Darwin"
            && cx.var_or_empty(&lang_var(&lang, b"_COMPILER_ID")) == "AppleClang"
            && !cx
                .var_or_empty(&lang_var(&lang, b"_COMPILER_TARGET"))
                .is_empty()
            && !cx.var_defined(&target_option)
        {
            cx.set_var(&target_option, "--target=".into());
        }

        let works = lang_var(&lang, b"_COMPILER_WORKS");
        if state.need_test && !self.in_try_compile {
            let test = cat(&[b"CMakeTest", &lang, b"Compiler.cmake"]);
            self.require_module(cx, &String::from_utf8_lossy(&test))?;
            if !cx.var_is_on(&works) {
                // Test the compiler again on the next run.
                let _ = cx.host().remove_file(&path::to_native(&compiler_file));
            }
        }

        if cx.var_is_on(&works) && cx.var_is_on(&lang_var(&lang, b"_USE_LINKER_INFORMATION")) {
            ensure_linker_identity(cx, &lang);
            if !cx.var_defined(&lang_var(&lang, b"_LINKER_INFORMATION_LOADED")) {
                let name = cat(&[b"CMake", &lang, b"LinkerInformation.cmake"]);
                let file = modules_file(cx, &name)
                    .or_else(|| modules_file(cx, &cat(&[b"Internal/", &name])));
                match file {
                    None => return Err(missing_module(cx, &name)),
                    Some(file) => {
                        read_list_file(cx, &file)?;
                    }
                }
            }
            if state.need_test && !self.in_try_compile {
                let inspect = cat(&[b"Internal/CMakeInspect", &lang, b"Linker.cmake"]);
                self.require_module(cx, &String::from_utf8_lossy(&inspect))?;
            }
        }
        if compiler_available
            && !cx.fatal_occurred()
            && (!state.need_test || self.in_try_compile || cx.var_is_on(&works))
        {
            let source = cx.var_or_empty(&lang_var(&lang, b"_SOURCE_FILE_EXTENSIONS"));
            let ignored = cx.var_or_empty(&lang_var(&lang, b"_IGNORE_EXTENSIONS"));
            let registry = cx.project().languages_mut();
            registry.register_language(&lang);
            registry.set_extensions(
                &lang,
                list::split(&source, Empty::Drop),
                list::split(&ignored, Empty::Drop),
            );
        }
        Ok(fatal)
    }

    /// The "compiler not found" text for `lang`, or `None` when the compiler exists.
    fn compiler_problem(&self, cx: &mut Cx<'_>, lang: &[u8]) -> Option<BString> {
        let name = lang_var(lang, b"_COMPILER");
        let compiler = cx.var_or_empty(&name);
        if compiler.is_empty() || compiler.ends_with(b"NOTFOUND") {
            return Some(cat(&[b"No ", &name, b" could be found.\n"]));
        }
        if matches!(lang, b"RC" | b"ASM_MARMASM" | b"ASM_MASM") {
            return None;
        }
        if !is_full_path(&compiler) {
            return Some(cat(&[
                b"The ",
                &name,
                b":\n  ",
                &compiler,
                b"\nis not a full path and was not found in the PATH.\n",
            ]));
        }
        if !exists(cx, &path::to_native(&compiler)) {
            return Some(cat(&[
                b"The ",
                &name,
                b":\n  ",
                &compiler,
                b"\nis not a full path to an existing compiler tool.\n",
            ]));
        }
        None
    }

    /// Find and read the module `name`; report `Could not find cmake module file` when it
    /// is missing or can not be read.
    fn require_module(&self, cx: &mut Cx<'_>, name: &str) -> Result<(), CmdError> {
        let found = match modules_file(cx, name.as_bytes()) {
            Some(file) => read_list_file(cx, &file)?,
            None => false,
        };
        if found {
            Ok(())
        } else {
            Err(missing_module(cx, name.as_bytes()))
        }
    }
}

/// Seed the global properties that the bootstrap modules and projects read.
fn seed_global_properties(cx: &mut Cx<'_>) {
    let languages = enabled_languages(cx);
    let props = &mut cx.global().props;
    if props.get(IN_TRY_COMPILE).is_none() {
        props.set(IN_TRY_COMPILE.into(), "0".into());
    }
    let names = known_features::LANGUAGES.iter().copied().chain(
        known_features::STANDARD_FEATURES
            .iter()
            .map(|&(name, _)| name),
    );
    for name in names {
        if props.get(name.as_bytes()).is_none()
            && let Some(features) = known_features::known_features(name.as_bytes())
        {
            props.set(name.into(), features.into());
        }
    }
    // The resolver uses the known feature list until this language is enabled.
    // Keep this fallback out of the interpreter's compiler variables.
    for lang in [b"C".as_slice(), b"CXX", b"CUDA", b"HIP"] {
        if !languages.iter().any(|enabled| enabled.as_slice() == lang)
            && let Some(features) =
                known_features::known_features(&lang_var(lang, b"_KNOWN_FEATURES"))
        {
            props.set(lang_var(lang, b"_COMPILE_FEATURES"), features.into());
        }
    }
}

/// `true` when `lang` is in the `ENABLED_LANGUAGES` global property.
fn language_enabled(cx: &mut Cx<'_>, lang: &[u8]) -> bool {
    enabled_languages(cx).iter().any(|enabled| enabled == lang)
}

fn enabled_languages(cx: &mut Cx<'_>) -> Vec<BString> {
    cx.global()
        .props
        .get(ENABLED_LANGUAGES)
        .map(|value| list::split(value, Empty::Drop))
        .unwrap_or_default()
}

/// Add `lang` to the sorted `ENABLED_LANGUAGES` global property
/// (`cmState::SetLanguageEnabled`).
fn set_language_enabled(cx: &mut Cx<'_>, lang: &[u8]) {
    cx.global()
        .props
        .unset(&lang_var(lang, b"_COMPILE_FEATURES"));
    let mut languages = enabled_languages(cx);
    if let Err(index) = languages.binary_search_by(|probe| probe.as_slice().cmp(lang)) {
        languages.insert(index, lang.into());
        cx.global()
            .props
            .set(ENABLED_LANGUAGES.into(), list::join(languages));
    }
}

/// Move `CXX` in front of `SYCL`, adding it when it is missing: SYCL needs C++.
fn order_sycl(languages: &mut Vec<BString>) {
    let Some(sycl) = languages.iter().position(|lang| lang == b"SYCL") else {
        return;
    };
    match languages.iter().position(|lang| lang == b"CXX") {
        None => languages.insert(sycl, "CXX".into()),
        Some(cxx) if cxx > sycl => {
            let lang = languages.remove(cxx);
            languages.insert(sycl, lang);
        }
        Some(_) => {}
    }
}

/// `CMAKE_PROJECT_TOP_LEVEL_INCLUDES`: read each file once, after the first system
/// determination.
fn top_level_includes(cx: &mut Cx<'_>) -> Result<(), CmdError> {
    let includes = cx.var_or_empty(b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES");
    let items = list::split(&includes, Empty::Drop);
    if items.is_empty() {
        return Ok(());
    }
    cx.global()
        .props
        .set(b"_RCMAKE_IN_TOP_LEVEL_INCLUDES".into(), "ON".into());
    let mut result = Ok(());
    for setup in items {
        let mut file = setup.clone();
        if !is_full_path(&setup) && !setup.ends_with(b".cmake") {
            match modules_file(cx, &cat(&[&setup, b".cmake"])) {
                Some(found) => file = found,
                None => {
                    result = Err(system_error(
                        cx,
                        &format!(
                            "CMAKE_PROJECT_TOP_LEVEL_INCLUDES module:\n  {}",
                            String::from_utf8_lossy(&setup)
                        ),
                    ));
                    break;
                }
            }
        }
        let absolute = absolute_path(&file, &path::from_native(&cx.current_source_dir()));
        let native = path::to_native(&absolute);
        let problem = if !exists(cx, &native) {
            Some("CMAKE_PROJECT_TOP_LEVEL_INCLUDES file does not exist: ")
        } else if is_dir(cx, &native) {
            Some("CMAKE_PROJECT_TOP_LEVEL_INCLUDES file is a directory: ")
        } else {
            match read_list_file(cx, &absolute) {
                Ok(true) => None,
                Ok(false) => Some("Failed reading CMAKE_PROJECT_TOP_LEVEL_INCLUDES file: "),
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        };
        if let Some(problem) = problem {
            result = Err(system_error(
                cx,
                &format!("{problem}{}", String::from_utf8_lossy(&setup)),
            ));
            break;
        }
    }
    cx.global().props.unset(b"_RCMAKE_IN_TOP_LEVEL_INCLUDES");
    result
}

/// `cmMakefile::GetModulesFile`: search `CMAKE_MODULE_PATH`, then `${CMAKE_ROOT}/Modules`.
/// A module that runs from `${CMAKE_ROOT}/Modules` gets the bundled file when both exist.
pub(super) fn modules_file(cx: &mut Cx<'_>, file: &[u8]) -> Option<BString> {
    let cwd = path::from_native(&cx.host().initial_cwd());
    let mut user = None;
    for dir in list::split(&cx.var_or_empty(b"CMAKE_MODULE_PATH"), Empty::Drop) {
        let dir: Vec<u8> = dir
            .iter()
            .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
            .collect();
        let candidate = cat(&[&dir, b"/", file]);
        if exists(cx, &path::to_native(&absolute_path(&candidate, &cwd))) {
            user = Some(candidate);
            break;
        }
    }
    let modules = cat(&[&cx.var_or_empty(b"CMAKE_ROOT"), b"/Modules/"]);
    let bundled = cat(&[&modules, file]);
    let bundled = exists(cx, &path::to_native(&bundled)).then_some(bundled);
    match (user, bundled) {
        (Some(user), Some(bundled)) => {
            if cx
                .var_or_empty(b"CMAKE_CURRENT_LIST_FILE")
                .starts_with(&modules)
            {
                Some(bundled)
            } else {
                Some(user)
            }
        }
        (user, bundled) => user.or(bundled),
    }
}

/// `cmMakefile::ReadListFile`: run `file` in the current variable and policy scope.
/// Returns `Ok(false)` when the file is missing or can not be parsed, and
/// [`CmdError::Reported`] after a fatal error.
fn read_list_file(cx: &mut Cx<'_>, file: &[u8]) -> Result<bool, CmdError> {
    let options = IncludeOptions {
        optional: false,
        no_policy_scope: true,
        no_diagnostic_scope: true,
    };
    let read = cx.include_file(&path::to_native(file), options).is_ok();
    if cx.fatal_occurred() {
        return Err(CmdError::Reported);
    }
    Ok(read)
}

/// `cmSystemTools::Error` followed by a stop: print `CMake Error: <message>`.
fn system_error(cx: &mut Cx<'_>, message: &str) -> CmdError {
    let line = format!("CMake Error: {message}\n");
    cx.host().write_out(Stream::Stderr, line.as_bytes());
    cx.set_fatal();
    CmdError::Reported
}

fn missing_module(cx: &mut Cx<'_>, name: &[u8]) -> CmdError {
    system_error(
        cx,
        &format!(
            "Could not find cmake module file: {}",
            String::from_utf8_lossy(name)
        ),
    )
}

/// `cmGlobalGenerator::PrintCompilerAdvice` (including NMake `cl` advice).
fn compiler_advice(generator: GeneratorKind, lang: &[u8], env_var: Option<&[u8]>) -> BString {
    let nmake_prefix: &[u8] =
        if generator == GeneratorKind::NMakeMakefiles && matches!(lang, b"C" | b"CXX") {
            b"To use the NMake generator with Visual C++, cmake must be run from a shell that can \
              use the compiler cl from the command line. This environment is unable to invoke the \
              cl compiler. To fix this problem, run cmake from the Visual Studio Command Prompt \
              (vcvarsall.bat).\n"
        } else {
            b""
        };
    let either = env_var.map_or_else(Vec::new, |var| {
        cat(&[b"either the environment variable \"", var, b"\" or "]).to_vec()
    });
    cat(&[
        nmake_prefix,
        b"Tell CMake where to find the compiler by setting ",
        &either,
        b"the CMake cache entry CMAKE_",
        lang,
        b"_COMPILER to the full path to the compiler, or to the compiler name if it is in \
          the PATH.",
    ])
}

/// `cmMakefile::AddCacheDefinition` for a value that the bootstrap computes.
pub(super) fn add_cache(cx: &mut Cx<'_>, name: &[u8], value: BString, ty: CacheType, help: &str) {
    let advanced = cx.cache().get(name).is_some_and(|entry| entry.advanced);
    let entry = CacheEntry {
        value,
        ty,
        help: help.into(),
        advanced,
        props: Default::default(),
    };
    if !cx.insert_cache_entry_unrecorded(name.into(), entry) {
        return;
    }
    if cx.var_normal_only(name).is_some() && cx.policy(PolicyId::new(126)) != PolicyStatus::New {
        cx.unset_var(name);
    }
}

/// Largest `CMake<LANG>Compiler.cmake` file that an identification record describes.
const COMPILER_FILE_LIMIT: u64 = 1024 * 1024;

/// Path of the identification record for `lang` in the platform information directory.
///
/// The drift tracker in `rcmake-cli` (`drift.rs`) removes old records before a native
/// configure and reads them after generation. A record whose `evidence_sha256`
/// matches the current compiler file is the only proof that this configure
/// identified the compiler. A changed compiler file alone is not proof.
fn identification_record(root_bin: &[u8], lang: &[u8]) -> BString {
    cat(&[
        root_bin,
        b"/CMake",
        lang,
        b"Compiler.identification-v1.json",
    ])
}

/// Record that `CMakeDetermine<LANG>Compiler.cmake` ran in this configure and wrote
/// the compiler file that is now on disk.
///
/// The record depends only on the compiler file, so equal configures write equal
/// build trees. The previous record is removed first, so a failed write leaves no
/// stale proof.
fn record_identification(cx: &mut Cx<'_>, root_bin: &[u8], lang: &[u8]) {
    if !cx.services().track_compiler_identifications {
        return;
    }
    let host = cx.host();
    let record = path::to_native(&identification_record(root_bin, lang));
    let compiler_file = path::to_native(&cat(&[root_bin, b"/CMake", lang, b"Compiler.cmake"]));
    let _ = host.remove_file(&record);
    let bytes = match host.metadata(&compiler_file, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File && meta.len <= COMPILER_FILE_LIMIT => host
            .read(
                &compiler_file,
                ReadRange::All {
                    limit: COMPILER_FILE_LIMIT,
                },
            )
            .ok(),
        _ => None,
    };
    let Some(bytes) = bytes else {
        return;
    };
    let text = format!(
        "{{\"version\":1,\"evidence_sha256\":\"{}\"}}\n",
        HashAlgo::Sha256.hex_digest(&bytes)
    );
    let _ = host.write(&record, text.as_bytes(), WriteMode::CreateNew);
}

fn lang_var(lang: &[u8], suffix: &[u8]) -> BString {
    cat(&[b"CMAKE_", lang, suffix])
}

fn exists(cx: &mut Cx<'_>, file: &Path) -> bool {
    cx.host().metadata(file, Follow::Yes).is_ok()
}

fn is_dir(cx: &mut Cx<'_>, dir: &Path) -> bool {
    cx.host()
        .metadata(dir, Follow::Yes)
        .is_ok_and(|meta| meta.kind == rcmake_host::FileKind::Dir)
}

fn is_full_path(file: &[u8]) -> bool {
    matches!(file.first(), Some(b'/' | b'\\')) || (file.len() >= 2 && file.get(1) == Some(&b':'))
}

fn absolute_path(file: &[u8], base: &[u8]) -> BString {
    if is_full_path(file) {
        return file.into();
    }
    let joined: PathBuf = path::to_native(base).join(path::to_native(file));
    path::from_native(&joined)
}

fn cat(parts: &[&[u8]]) -> BString {
    parts.concat().into()
}

/// Populate Visual Studio IDE generator variables that CMake's `cmGlobalVisualStudio10Generator`
/// establishes before language determination runs (`CMAKE_MSVCIDE_RUN_PATH`,
/// `CMAKE_VS_MSBUILD_COMMAND`, and `CMAKE_VS_TARGET_FRAMEWORK_VERSION`).
fn init_vs_ide_vars(cx: &mut Cx<'_>, version: VsVersion) {
    if !cx.var_defined(b"CMAKE_MSVCIDE_RUN_PATH")
        && let Some(run_path) = cx.host().env_get(b"CMAKE_MSVCIDE_RUN_PATH")
    {
        add_cache(
            cx,
            b"CMAKE_MSVCIDE_RUN_PATH",
            run_path,
            CacheType::String,
            "Saved environment variable CMAKE_MSVCIDE_RUN_PATH",
        );
    }
    if cx.var_or_empty(b"CMAKE_VS_MSBUILD_COMMAND").is_empty()
        && let Some(make) = cx.var(b"CMAKE_MAKE_PROGRAM").filter(|v| !is_off(v))
    {
        cx.set_var(b"CMAKE_VS_MSBUILD_COMMAND", make);
    }
    if !cx.var_defined(b"CMAKE_VS_TARGET_FRAMEWORK_VERSION")
        && matches!(version, VsVersion::Vs17 | VsVersion::Vs18)
    {
        cx.set_var(b"CMAKE_VS_TARGET_FRAMEWORK_VERSION", "v4.7.2".into());
    }
}

/// Discover `csc.exe` from the selected Visual Studio / MSBuild installation, `.NET Framework`,
/// or `PATH` when MSBuild's `CompilerIdCSharp.csproj` probe did not populate
/// `CMAKE_CSharp_COMPILER`, and persist the compiler identification file.
fn discover_csharp_compiler_fallback(cx: &mut Cx<'_>, root_bin: &[u8]) {
    let Some(compiler_path) = find_csharp_compiler(cx) else {
        return;
    };
    let compiler = path::from_native_cmake(&compiler_path);
    add_cache(
        cx,
        b"CMAKE_CSharp_COMPILER",
        compiler.clone(),
        CacheType::Filepath,
        "C# compiler",
    );
    cx.set_var(b"CMAKE_CSharp_COMPILER", compiler.clone());

    let mut compiler_id = cx.var_or_empty(b"CMAKE_CSharp_COMPILER_ID");
    if compiler_id.is_empty() {
        compiler_id = "Microsoft Visual Studio".into();
        cx.set_var(b"CMAKE_CSharp_COMPILER_ID", compiler_id.clone());
    }
    let mut compiler_version = cx.var_or_empty(b"CMAKE_CSharp_COMPILER_VERSION");
    if compiler_version.is_empty()
        && let Some(version) = probe_csharp_compiler_version(cx, &compiler)
    {
        compiler_version = version;
        cx.set_var(b"CMAKE_CSharp_COMPILER_VERSION", compiler_version.clone());
    }
    cx.set_var(b"CMAKE_CSharp_COMPILER_LOADED", "1".into());
    cx.set_var(b"CMAKE_CSharp_COMPILER_ID_RUN", "1".into());
    cx.set_var(b"CMAKE_CSharp_SOURCE_FILE_EXTENSIONS", "cs".into());
    cx.set_var(
        b"CMAKE_CSharp_IGNORE_EXTENSIONS",
        "inl;h;hpp;HPP;H;o;O;obj;OBJ;def;DEF;rc;RC".into(),
    );

    let compiler_file = path::to_native(&cat(&[root_bin, b"/CMakeCSharpCompiler.cmake"]));
    let works = cx.var_or_empty(b"CMAKE_CSharp_COMPILER_WORKS");
    let content = format!(
        "set(CMAKE_CSharp_COMPILER \"{compiler}\")\n\
         set(CMAKE_CSharp_COMPILER_ID \"{compiler_id}\")\n\
         set(CMAKE_CSharp_COMPILER_VERSION \"{compiler_version}\")\n\n\
         set(CMAKE_CSharp_COMPILER_LOADED 1)\n\
         set(CMAKE_CSharp_COMPILER_WORKS \"{works}\")\n\n\
         set(CMAKE_CSharp_COMPILER_ID_RUN \"1\")\n\
         set(CMAKE_CSharp_IGNORE_EXTENSIONS \"inl;h;hpp;HPP;H;o;O;obj;OBJ;def;DEF;rc;RC\")\n\
         set(CMAKE_CSharp_SOURCE_FILE_EXTENSIONS \"cs\")\n"
    );
    let _ = cx
        .host()
        .write(&compiler_file, content.as_bytes(), WriteMode::Truncate);
}

fn find_csharp_compiler(cx: &mut Cx<'_>) -> Option<PathBuf> {
    let host = cx.host();
    let is_file = |candidate: &Path| {
        host.metadata(candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in [
        b"CMAKE_VS_MSBUILD_COMMAND".as_slice(),
        b"CMAKE_MAKE_PROGRAM",
    ] {
        let msbuild = cx.var_or_empty(var);
        if msbuild.is_empty() || is_off(&msbuild) {
            continue;
        }
        let native = path::to_native(&msbuild);
        if let Some(dir) = native.parent() {
            candidates.push(dir.join("Roslyn/csc.exe"));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("Roslyn/csc.exe"));
            }
            candidates.push(dir.join("csc.exe"));
        }
    }
    let instance = cx.var_or_empty(b"CMAKE_GENERATOR_INSTANCE");
    if let Some(dir) = instance
        .split(|&byte| byte == b',')
        .next()
        .filter(|dir| !dir.is_empty() && !dir.contains(&b'='))
    {
        let base = path::to_native(dir);
        candidates.push(base.join("MSBuild/Current/Bin/Roslyn/csc.exe"));
        candidates.push(base.join("MSBuild/Current/Bin/amd64/Roslyn/csc.exe"));
        candidates.push(base.join("MSBuild/15.0/Bin/Roslyn/csc.exe"));
    }
    if let Some(vs_dir) = host.env_get(b"VSINSTALLDIR").filter(|dir| !dir.is_empty()) {
        let base = path::to_native(&vs_dir);
        candidates.push(base.join("MSBuild/Current/Bin/Roslyn/csc.exe"));
        candidates.push(base.join("MSBuild/Current/Bin/amd64/Roslyn/csc.exe"));
        candidates.push(base.join("MSBuild/15.0/Bin/Roslyn/csc.exe"));
    }
    for env_var in [b"WINDIR".as_slice(), b"SystemRoot"] {
        if let Some(windir) = host.env_get(env_var).filter(|dir| !dir.is_empty()) {
            let base = path::to_native(&windir);
            candidates.push(base.join("Microsoft.NET/Framework64/v4.0.30319/csc.exe"));
            candidates.push(base.join("Microsoft.NET/Framework/v4.0.30319/csc.exe"));
        }
    }
    if let Some(found) = candidates.into_iter().find(|path| is_file(path)) {
        return Some(found);
    }
    let windows = host.system().sysname.starts_with(b"Windows");
    if let Some(paths) = host.env_get(b"PATH") {
        let delimiter = if windows { b';' } else { b':' };
        for dir in paths
            .split(|&byte| byte == delimiter)
            .filter(|dir| !dir.is_empty())
        {
            let base = host.initial_cwd().join(path::to_native(dir));
            for name in ["csc.exe", "csc"] {
                let candidate = base.join(name);
                if is_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn probe_csharp_compiler_version(cx: &Cx<'_>, compiler: &BString) -> Option<BString> {
    if !cx.host().supports_process_execution() {
        return None;
    }
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: compiler.clone(),
            args: vec!["/help /preferreduilang:en-US".into()],
        }],
        cwd: cx.host().initial_cwd(),
        env: cx.host().env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(std::time::Duration::from_secs(5)),
        tee: false,
        default_error_mode: false,
    };
    let output = cx.host().run_bounded(&request, 64 * 1024).ok()?;
    parse_csharp_version(&output.stdout).or_else(|| parse_csharp_version(&output.stderr))
}

fn parse_csharp_version(output: &[u8]) -> Option<BString> {
    for line in output.split(|&byte| byte == b'\n') {
        let upper = line.to_ascii_uppercase();
        let Some(compiler_pos) = upper.windows(8).position(|window| window == b"COMPILER") else {
            continue;
        };
        let after_compiler = upper.get(compiler_pos + 8..)?;
        let Some(version_pos) = after_compiler
            .windows(7)
            .position(|window| window == b"VERSION")
        else {
            continue;
        };
        let rest = after_compiler.get(version_pos + 7..)?;
        let start = rest
            .iter()
            .position(|&byte| byte.is_ascii_digit() || byte == b'.')?;
        let digits = rest.get(start..)?;
        let end = digits
            .iter()
            .position(|&byte| !byte.is_ascii_digit() && byte != b'.')
            .unwrap_or(digits.len());
        let token = digits.get(..end)?;
        if token.iter().any(u8::is_ascii_digit) {
            return Some(BString::from(token));
        }
    }
    None
}

/// Ensure `CMAKE_<LANG>_COMPILER_LINKER` and `CMAKE_<LANG>_COMPILER_LINKER_ID` are populated
/// for MSVC toolchains before `Internal/CMake<LANG>LinkerInformation.cmake` runs so platform
/// linker modules (`Linker/MSVC-*.cmake` and `Platform/Linker/Windows-MSVC-*.cmake`) load cleanly.
fn ensure_linker_identity(cx: &mut Cx<'_>, lang: &[u8]) {
    let id_var = lang_var(lang, b"_COMPILER_LINKER_ID");
    if !cx.var_or_empty(&id_var).is_empty() {
        return;
    }
    let is_msvc = cx.var_or_empty(&lang_var(lang, b"_COMPILER_ID")) == b"MSVC"
        || cx.var_or_empty(&lang_var(lang, b"_SIMULATE_ID")) == b"MSVC";
    if !is_msvc {
        return;
    }
    let linker_var = lang_var(lang, b"_COMPILER_LINKER");
    let current_linker = cx.var_or_empty(&linker_var);
    let linker_missing = current_linker.is_empty() || current_linker.ends_with(b"NOTFOUND");
    let candidate = if !linker_missing {
        Some(current_linker)
    } else {
        cx.var(b"CMAKE_LINKER")
            .filter(|value| !value.is_empty() && !value.ends_with(b"NOTFOUND"))
    };
    let Some(linker) = candidate else {
        return;
    };
    let win32 = cx.var_is_on(b"WIN32") || cx.var_or_empty(b"CMAKE_SYSTEM_NAME") == b"Windows";
    let mut identified = probe_linker_identity(cx, &linker, win32);
    if identified.is_none() {
        let leaf = linker
            .rsplit(|&byte| byte == b'/' || byte == b'\\')
            .next()
            .unwrap_or(linker.as_slice());
        if leaf.eq_ignore_ascii_case(b"link.exe") || leaf.eq_ignore_ascii_case(b"link") {
            identified = Some(("MSVC", Some("MSVC"), None));
        } else if leaf.eq_ignore_ascii_case(b"lld-link.exe")
            || leaf.eq_ignore_ascii_case(b"lld-link")
        {
            identified = Some(("LLD", Some("MSVC"), None));
        }
    }
    if let Some((id, frontend, version)) = identified {
        if linker_missing {
            cx.set_var(&linker_var, linker);
        }
        cx.set_var(&id_var, id.into());
        if let Some(frontend) = frontend {
            cx.set_var(
                &lang_var(lang, b"_COMPILER_LINKER_FRONTEND_VARIANT"),
                frontend.into(),
            );
        }
        if let Some(version) = version {
            cx.set_var(&lang_var(lang, b"_COMPILER_LINKER_VERSION"), version);
        }
    }
}

fn probe_linker_identity(
    cx: &Cx<'_>,
    linker: &BString,
    win32: bool,
) -> Option<(&'static str, Option<&'static str>, Option<BString>)> {
    if !cx.host().supports_process_execution() {
        return None;
    }
    let cwd = cx.host().initial_cwd();
    let env = cx.host().env_all();
    for flag in ["-v", "--version"] {
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: linker.clone(),
                args: vec![flag.into()],
            }],
            cwd: cwd.clone(),
            env: env.clone(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(std::time::Duration::from_secs(5)),
            tee: false,
            default_error_mode: false,
        };
        let Ok(output) = cx.host().run_bounded(&request, 64 * 1024) else {
            return None;
        };
        if matches!(output.results.first(), Some(ProcessResult::SpawnFailed(_))) {
            return None;
        }
        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&output.stderr);
        }
        if let Some(parsed) = parse_linker_identity(&combined, win32) {
            return Some(parsed);
        }
    }
    None
}

fn parse_linker_identity(
    output: &[u8],
    win32: bool,
) -> Option<(&'static str, Option<&'static str>, Option<BString>)> {
    let text = std::str::from_utf8(output).ok()?;
    let version_at = |rest: &str| -> Option<BString> {
        let token: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect();
        token
            .chars()
            .any(|ch| ch.is_ascii_digit())
            .then(|| token.into())
    };
    if let Some(pos) = text.find("Microsoft (R) Incremental Linker Version ") {
        let version = text
            .get(pos + "Microsoft (R) Incremental Linker Version ".len()..)
            .and_then(version_at);
        return Some(("MSVC", Some("MSVC"), version));
    }
    if let Some(pos) = text.find("LLD ")
        && let Some(version) = text.get(pos + "LLD ".len()..).and_then(version_at)
    {
        let frontend = if win32 && !text.contains("compatible with GNU") {
            "MSVC"
        } else {
            "GNU"
        };
        return Some(("LLD", Some(frontend), Some(version)));
    }
    if let Some(pos) = text.find("GNU gold ")
        && let Some(after) = text.get(pos..)
        && let Some(after_paren) = after.find(") ")
        && let Some(version) = after.get(after_paren + 2..).and_then(version_at)
    {
        return Some(("GNUgold", Some("GNU"), Some(version)));
    }
    if let Some(pos) = text.find("GNU ld ")
        && let Some(tail) = text.get(pos + "GNU ld ".len()..)
    {
        let version_start = if let Some(rest) = tail.strip_prefix("version ") {
            Some(rest)
        } else if let Some(close) = tail.find(") ") {
            tail.get(close + 2..)
        } else {
            None
        };
        if let Some(version) = version_start.and_then(version_at) {
            return Some(("GNU", Some("GNU"), Some(version)));
        }
    }
    if let Some(pos) = text.find("mold (sold) ")
        && let Some(version) = text.get(pos + "mold (sold) ".len()..).and_then(version_at)
    {
        return Some(("MOLD", Some("GNU"), Some(version)));
    }
    if let Some(pos) = text.find("mold ")
        && let Some(version) = text.get(pos + "mold ".len()..).and_then(version_at)
    {
        return Some(("MOLD", Some("GNU"), Some(version)));
    }
    if let Some(pos) = text.find("Wild version ")
        && let Some(version) = text.get(pos + "Wild version ".len()..).and_then(version_at)
    {
        return Some(("WILD", Some("GNU"), Some(version)));
    }
    None
}

#[cfg(test)]
mod identification_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcmake_eval::{CacheDefine, Interp, InterpConfig, RunMode};
    use rcmake_host::{Host, PureHost, ReadRange};
    use rcmake_model::CacheType;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const MODULES: &str = "/share/cmake-4.5/Modules";
    const RECORD: &str = "/work/build/CMakeFiles/4.5.1/CMakeCCompiler.identification-v1.json";
    const COMPILER_FILE: &str = "/work/build/CMakeFiles/4.5.1/CMakeCCompiler.cmake";

    fn configure(host: &Arc<PureHost>, defines: Vec<CacheDefine>, track: bool) -> TestResult {
        let services = rcmake_eval::Services {
            track_compiler_identifications: track,
            ..Default::default()
        };
        let mut interp = Interp::new(
            InterpConfig {
                services: Arc::new(services),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines,
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(outcome.exit_code, 0);
        Ok(())
    }

    fn seed(host: &PureHost) -> TestResult {
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Fake)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /bin/make CACHE FILEPATH \"\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake \"set(CMAKE_C_COMPILER /bin/cc)\\nset(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCInformation.cmake",
            "set(CMAKE_C_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCCompiler.cmake",
            "set(CMAKE_C_COMPILER_WORKS TRUE)\n",
        )?;
        host.seed_file(Path::new("/bin/cc"), b"")?;
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(P C)\n")?;
        Ok(())
    }

    fn read(host: &PureHost, file: &str) -> Option<Vec<u8>> {
        host.read(&PathBuf::from(file), ReadRange::All { limit: 1 << 16 })
            .ok()
    }

    /// Only a real compiler identification writes a record. The record names the
    /// digest of the compiler file that the identification wrote.
    #[test]
    fn only_a_compiler_identification_writes_a_record() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        seed(&host)?;
        // Compatibility configures leave tracking disabled and write no record.
        configure(&host, Vec::new(), false)?;
        assert_eq!(read(&host, RECORD), None);

        configure(&host, Vec::new(), true)?;
        let record = read(&host, RECORD).ok_or("record")?;
        let compiler_file = read(&host, COMPILER_FILE).ok_or("compiler file")?;
        let expected = format!(
            "{{\"version\":1,\"evidence_sha256\":\"{}\"}}\n",
            rcmake_base::hash::HashAlgo::Sha256.hex_digest(&compiler_file)
        );
        assert_eq!(String::from_utf8(record.clone())?, expected);

        // A warm configure loads the saved results and writes no record.
        host.remove_file(Path::new(RECORD))?;
        let warm = CacheDefine {
            name: "CMAKE_PLATFORM_INFO_INITIALIZED".into(),
            ty: Some(CacheType::Internal),
            value: "1".into(),
        };
        configure(&host, vec![warm], true)?;
        assert_eq!(read(&host, RECORD), None);

        // A new identification writes the same bytes for the same compiler file.
        configure(&host, Vec::new(), true)?;
        assert_eq!(read(&host, RECORD), Some(record));
        Ok(())
    }

    #[test]
    fn explicit_c_and_rc_allows_implicit_rc_enable_during_c_info() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /bin/make CACHE FILEPATH \"\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake \"set(CMAKE_C_COMPILER /bin/cl)\\nset(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCInformation.cmake",
            "enable_language(RC)\nset(CMAKE_C_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCCompiler.cmake",
            "set(CMAKE_C_COMPILER_WORKS TRUE)\n",
        )?;
        module(
            "CMakeDetermineRCCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeRCCompiler.cmake \"set(CMAKE_RC_COMPILER /bin/rc)\\nset(CMAKE_RC_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeRCInformation.cmake",
            "set(CMAKE_RC_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestRCCompiler.cmake",
            "set(CMAKE_RC_COMPILER_WORKS TRUE)\n",
        )?;
        host.seed_file(Path::new("/bin/cl"), b"")?;
        host.seed_file(Path::new("/bin/rc"), b"")?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"project(ExplicitRC C RC)\n",
        )?;
        configure(&host, Vec::new(), false)?;
        Ok(())
    }

    #[test]
    fn vs_csharp_discovers_csc_target_framework_and_msvcide_run_path() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        host.env_set(b"CMAKE_MSVCIDE_RUN_PATH", Some(b"C:/tools;C:/bin"));
        host.env_set(b"ProgramFiles(x86)", Some(b"/pf86"));
        host.seed_file(
            Path::new(
                "/pf86/Reference Assemblies/Microsoft/Framework/.NETFramework/v4.7.2/mscorlib.dll",
            ),
            b"",
        )?;
        host.seed_file(
            Path::new(
                "/pf86/Reference Assemblies/Microsoft/Framework/.NETFramework/v4.8/mscorlib.dll",
            ),
            b"",
        )?;
        host.seed_file(
            Path::new("/vs2022/MSBuild/Current/Bin/amd64/MSBuild.exe"),
            b"",
        )?;
        host.seed_file(Path::new("/vs2022/MSBuild/Current/Bin/Roslyn/csc.exe"), b"")?;
        host.set_command_handler(|_, req, _| {
            let cmd = req.pipeline.first().map(|s| s.program.as_slice());
            if cmd == Some(b"/vs2022/MSBuild/Current/Bin/Roslyn/csc.exe".as_slice()) {
                return Ok(rcmake_host::RunOutput {
                    results: vec![rcmake_host::ProcessResult::Exited(0)],
                    stdout: b"Microsoft (R) Visual C# Compiler version 4.14.0-3.25279.5\n".to_vec(),
                    ..Default::default()
                });
            }
            Ok(rcmake_host::RunOutput {
                results: vec![rcmake_host::ProcessResult::Exited(0)],
                ..Default::default()
            })
        })?;
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCSharpCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCSharpCompiler.cmake \"set(CMAKE_CSharp_COMPILER CMAKE_CSharp_COMPILER-NOTFOUND)\\nset(CMAKE_CSharp_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCSharpInformation.cmake",
            "set(CMAKE_CSharp_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCSharpCompiler.cmake",
            "set(CMAKE_CSharp_COMPILER_WORKS TRUE)\n",
        )?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"set(CMAKE_DOTNET_TARGET_FRAMEWORK_VERSION net8.0)\nproject(App NONE)\nenable_language(CSharp)\n",
        )?;
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: vec![
                    CacheDefine {
                        name: "CMAKE_GENERATOR".into(),
                        ty: Some(CacheType::Internal),
                        value: "Visual Studio 17 2022".into(),
                    },
                    CacheDefine {
                        name: "CMAKE_GENERATOR_INSTANCE".into(),
                        ty: Some(CacheType::Internal),
                        value: "/vs2022".into(),
                    },
                ],
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        interp.cx().project().generator = Some(rcmake_model::GeneratorKind::VisualStudio(
            rcmake_model::VsVersion::Vs17,
        ));
        interp.cx().project().generator_spec.toolset = "VCTargetsPath=C:/custom/vctargets".into();
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_MSVCIDE_RUN_PATH"),
            "C:/tools;C:/bin"
        );
        // CMake 4.5.1 defaults VS 17+ to v4.7.2 and never auto-picks v4.8 or copies SDK net8.0.
        assert_eq!(
            interp
                .cx()
                .var_or_empty(b"CMAKE_VS_TARGET_FRAMEWORK_VERSION"),
            "v4.7.2"
        );
        assert_eq!(
            interp
                .cx()
                .var_or_empty(b"CMAKE_VS_PLATFORM_TOOLSET_VCTARGETS_CUSTOM_DIR"),
            r"C:\custom\vctargets"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_CSharp_COMPILER"),
            "/vs2022/MSBuild/Current/Bin/Roslyn/csc.exe"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_CSharp_COMPILER_ID"),
            "Microsoft Visual Studio"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_CSharp_COMPILER_VERSION"),
            "4.14.0"
        );
        Ok(())
    }

    #[test]
    fn identifies_msvc_linker_and_loads_linker_modules() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        host.seed_file(Path::new("/bin/cl.exe"), b"")?;
        host.seed_file(Path::new("/bin/link.exe"), b"")?;
        host.set_command_handler(|_, req, _| {
            let cmd = req.pipeline.first().map(|s| s.program.as_slice());
            if cmd == Some(b"/bin/link.exe".as_slice()) {
                return Ok(rcmake_host::RunOutput {
                    results: vec![rcmake_host::ProcessResult::Exited(0)],
                    stdout: b"Microsoft (R) Incremental Linker Version 14.44.35207.0\n".to_vec(),
                    ..Default::default()
                });
            }
            Ok(rcmake_host::RunOutput {
                results: vec![rcmake_host::ProcessResult::Exited(0)],
                ..Default::default()
            })
        })?;
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(WIN32 1)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /bin/make CACHE FILEPATH \"\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake \"set(CMAKE_C_COMPILER /bin/cl.exe)\\nset(CMAKE_C_COMPILER_ID MSVC)\\nset(CMAKE_LINKER /bin/link.exe)\\nset(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCInformation.cmake",
            "set(CMAKE_C_USE_LINKER_INFORMATION TRUE)\nset(CMAKE_C_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCCompiler.cmake",
            "set(CMAKE_C_COMPILER_WORKS TRUE)\n",
        )?;
        module(
            "Internal/CMakeCLinkerInformation.cmake",
            "if(CMAKE_C_COMPILER_LINKER_ID STREQUAL \"MSVC\")\n  set(CMAKE_C_LINK_OPTIONS_WARNING_AS_ERROR \"LINKER:-WX\")\n  set(CMAKE_C_LINK_LIBRARY_USING_WHOLE_ARCHIVE \"LINKER:/WHOLEARCHIVE:<LIBRARY>\")\nendif()\nset(CMAKE_C_LINKER_INFORMATION_LOADED 1)\n",
        )?;
        module("Internal/CMakeInspectCLinker.cmake", "")?;
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(P C)\n")?;

        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_C_COMPILER_LINKER"),
            "/bin/link.exe"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_C_COMPILER_LINKER_ID"),
            "MSVC"
        );
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_C_COMPILER_LINKER_VERSION"),
            "14.44.35207.0"
        );
        assert_eq!(
            interp
                .cx()
                .var_or_empty(b"CMAKE_C_LINK_OPTIONS_WARNING_AS_ERROR"),
            "LINKER:-WX"
        );
        assert_eq!(
            interp
                .cx()
                .var_or_empty(b"CMAKE_C_LINK_LIBRARY_USING_WHOLE_ARCHIVE"),
            "LINKER:/WHOLEARCHIVE:<LIBRARY>"
        );
        Ok(())
    }

    #[test]
    fn toolchain_recursive_enable_language_rc_is_rejected() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "enable_language(RC)\nfile(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"project(ToolchainRC C RC)\n",
        )?;
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_ne!(outcome.exit_code, 0);
        let stderr =
            String::from_utf8_lossy(host.stream_bytes(rcmake_host::Stream::Stderr).as_slice())
                .into_owned();
        assert!(
            stderr
                .contains("Language 'RC' is currently being enabled.  Recursive call not allowed."),
            "expected recursive RC error in stderr:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn explicit_missing_csharp_compiler_is_not_overwritten_by_fallback() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        host.seed_file(
            Path::new("/vs2022/MSBuild/Current/Bin/amd64/MSBuild.exe"),
            b"",
        )?;
        host.seed_file(Path::new("/vs2022/MSBuild/Current/Bin/Roslyn/csc.exe"), b"")?;
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCSharpCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCSharpCompiler.cmake \"set(CMAKE_CSharp_COMPILER \\\"${CMAKE_CSharp_COMPILER}\\\")\\nset(CMAKE_CSharp_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCSharpInformation.cmake",
            "set(CMAKE_CSharp_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCSharpCompiler.cmake",
            "set(CMAKE_CSharp_COMPILER_WORKS TRUE)\n",
        )?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"project(App NONE)\nenable_language(CSharp)\n",
        )?;
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: vec![
                    CacheDefine {
                        name: "CMAKE_GENERATOR".into(),
                        ty: Some(CacheType::Internal),
                        value: "Visual Studio 17 2022".into(),
                    },
                    CacheDefine {
                        name: "CMAKE_GENERATOR_INSTANCE".into(),
                        ty: Some(CacheType::Internal),
                        value: "/vs2022".into(),
                    },
                    CacheDefine {
                        name: "CMAKE_CSharp_COMPILER".into(),
                        ty: Some(CacheType::Filepath),
                        value: "/explicit/missing/csc.exe".into(),
                    },
                ],
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        interp.cx().project().generator = Some(rcmake_model::GeneratorKind::VisualStudio(
            rcmake_model::VsVersion::Vs17,
        ));
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_ne!(outcome.exit_code, 0);
        assert_eq!(
            interp.cx().var_or_empty(b"CMAKE_CSharp_COMPILER"),
            "/explicit/missing/csc.exe"
        );
        Ok(())
    }

    #[test]
    fn non_msvc_toolchain_does_not_publish_global_cmake_linker() -> TestResult {
        let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
        host.seed_file(Path::new("/bin/clang"), b"")?;
        host.seed_file(Path::new("/bin/ld"), b"")?;
        let module = |name: &str, body: &str| {
            host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
        };
        module(
            "CMakeDetermineSystem.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Linux)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /bin/make CACHE FILEPATH \"\")\n",
        )?;
        module("CMakeSystemSpecificInitialize.cmake", "")?;
        module("CMakeSystemSpecificInformation.cmake", "")?;
        module(
            "CMakeDetermineCCompiler.cmake",
            "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake \"set(CMAKE_C_COMPILER /bin/clang)\\nset(CMAKE_C_COMPILER_ID Clang)\\nset(CMAKE_LINKER /bin/ld)\\nset(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
        )?;
        module(
            "CMakeCInformation.cmake",
            "set(CMAKE_C_USE_LINKER_INFORMATION TRUE)\nset(CMAKE_C_INFORMATION_LOADED 1)\n",
        )?;
        module(
            "CMakeTestCCompiler.cmake",
            "set(CMAKE_C_COMPILER_WORKS TRUE)\n",
        )?;
        module(
            "Internal/CMakeCLinkerInformation.cmake",
            "set(CMAKE_C_LINKER_INFORMATION_LOADED 1)\n",
        )?;
        module("Internal/CMakeInspectCLinker.cmake", "")?;
        host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(P C)\n")?;

        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(interp.cx().var_or_empty(b"CMAKE_C_COMPILER_LINKER"), "");
        assert_eq!(interp.cx().var_or_empty(b"CMAKE_C_COMPILER_LINKER_ID"), "");
        Ok(())
    }

    #[test]
    fn nmake_probes_nmake_help_and_prefers_cl_default_compiler() -> TestResult {
        use std::sync::Mutex;

        type RecordedCalls = Arc<Mutex<Vec<(String, Vec<String>)>>>;

        let setup_host = |nmake_exit: i32| -> Result<(Arc<PureHost>, RecordedCalls), Box<dyn std::error::Error>> {
            let host = Arc::new(PureHost::new("/work".into(), "/bin/cmake".into()));
            let calls: RecordedCalls = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            host.set_command_handler(move |_host, req, _out| {
                let spec = req.pipeline.first().unwrap();
                let prog = spec.program.to_string();
                let args: Vec<String> = spec.args.iter().map(ToString::to_string).collect();
                recorded.lock().unwrap().push((prog, args));
                Ok(rcmake_host::RunOutput {
                    results: vec![rcmake_host::ProcessResult::Exited(nmake_exit)],
                    stdout: Vec::new(),
                    stderr: if nmake_exit == 0 {
                        b"Microsoft (R) Program Maintenance Utility Version 14.40.33811.0\n"
                            .to_vec()
                    } else {
                        b"nmake not executable\n".to_vec()
                    },
                })
            })?;
            let module = |name: &str, body: &str| {
                host.seed_file(&Path::new(MODULES).join(name), body.as_bytes())
            };
            host.seed_file(Path::new("/bin/cl"), b"")?;
            module(
                "CMakeDetermineSystem.cmake",
                "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeSystem.cmake \"set(CMAKE_SYSTEM_NAME Windows)\\nset(CMAKE_SYSTEM_LOADED 1)\\n\")\n",
            )?;
            module(
                "CMakeNMakeFindMake.cmake",
                "set(CMAKE_MAKE_PROGRAM nmake CACHE FILEPATH \"\")\n",
            )?;
            module("CMakeSystemSpecificInitialize.cmake", "")?;
            module("CMakeSystemSpecificInformation.cmake", "")?;
            module(
                "CMakeDetermineCCompiler.cmake",
                "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake \"set(CMAKE_C_COMPILER /bin/${CMAKE_GENERATOR_CC})\\nset(CMAKE_C_COMPILER_ID MSVC)\\nset(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
            )?;
            module(
                "CMakeDetermineCXXCompiler.cmake",
                "file(WRITE ${CMAKE_PLATFORM_INFO_DIR}/CMakeCXXCompiler.cmake \"set(CMAKE_CXX_COMPILER /bin/${CMAKE_GENERATOR_CXX})\\nset(CMAKE_CXX_COMPILER_ID MSVC)\\nset(CMAKE_CXX_COMPILER_LOADED 1)\\n\")\n",
            )?;
            module(
                "CMakeCInformation.cmake",
                "set(CMAKE_C_INFORMATION_LOADED 1)\n",
            )?;
            module(
                "CMakeCXXInformation.cmake",
                "set(CMAKE_CXX_INFORMATION_LOADED 1)\n",
            )?;
            module(
                "CMakeTestCCompiler.cmake",
                "set(CMAKE_C_COMPILER_WORKS TRUE)\n",
            )?;
            module(
                "CMakeTestCXXCompiler.cmake",
                "set(CMAKE_CXX_COMPILER_WORKS TRUE)\n",
            )?;
            host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(P C CXX)\n")?;
            Ok((host, calls))
        };

        // Failing `nmake -?` aborts configure with CMake's exact diagnostic.
        let (fail_host, fail_calls) = setup_host(1)?;
        let mut fail_interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: fail_host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        fail_interp.cx().project().generator = Some(rcmake_model::GeneratorKind::NMakeMakefiles);
        let fail_outcome =
            fail_interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_ne!(fail_outcome.exit_code, 0);
        assert!(
            fail_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(prog, args)| prog == "nmake" && args == &["-?"]),
            "expected nmake -? probe: {:?}",
            fail_calls.lock().unwrap()
        );
        let stderr = String::from_utf8_lossy(&fail_host.stream_bytes(rcmake_host::Stream::Stderr))
            .into_owned();
        assert!(
            stderr.contains("'nmake' '-?'") && stderr.contains("nmake not executable"),
            "stderr={stderr}"
        );

        // Passing `nmake -?` succeeds and preserves CMAKE_GENERATOR_CC/CXX = "cl".
        let (ok_host, ok_calls) = setup_host(0)?;
        let mut ok_interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: ok_host.clone(),
                mode: RunMode::Configure,
                cmake_root: "/share/cmake-4.5".into(),
                cmake_command: "/bin/cmake".into(),
                cwd: "/work".into(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        ok_interp.cx().project().generator = Some(rcmake_model::GeneratorKind::NMakeMakefiles);
        let ok_outcome =
            ok_interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        assert_eq!(
            ok_outcome.exit_code,
            0,
            "stderr={}",
            String::from_utf8_lossy(&ok_host.stream_bytes(rcmake_host::Stream::Stderr))
        );
        assert!(
            ok_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(prog, args)| prog == "nmake" && args == &["-?"]),
            "expected nmake -? probe: {:?}",
            ok_calls.lock().unwrap()
        );
        assert_eq!(ok_interp.cx().var_or_empty(b"CMAKE_GENERATOR_CC"), "cl");
        assert_eq!(ok_interp.cx().var_or_empty(b"CMAKE_GENERATOR_CXX"), "cl");
        assert_eq!(ok_interp.cx().var_or_empty(b"CMAKE_C_COMPILER"), "/bin/cl");
        assert_eq!(
            ok_interp.cx().var_or_empty(b"CMAKE_CXX_COMPILER"),
            "/bin/cl"
        );
        Ok(())
    }
}
