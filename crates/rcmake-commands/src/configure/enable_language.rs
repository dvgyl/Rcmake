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

use rcmake_base::list::{self, Empty};
use rcmake_base::truth::is_off;
use rcmake_base::{BString, CMAKE_VERSION_STR};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, IncludeOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, Stream, path};
use rcmake_model::{CacheEntry, CacheType, GeneratorKind};

const CMP0132: PolicyId = PolicyId::new(132);
const CMP0165: PolicyId = PolicyId::new(165);

const ENABLED_LANGUAGES: &[u8] = b"ENABLED_LANGUAGES";
const IN_TRY_COMPILE: &[u8] = b"IN_TRY_COMPILE";
const PLATFORM_INFO_DIR: &[u8] = b"CMAKE_PLATFORM_INFO_DIR";
const PLATFORM_INFO_INITIALIZED: &[u8] = b"CMAKE_PLATFORM_INFO_INITIALIZED";

/// `CMAKE_<LANG>_KNOWN_FEATURES` global properties of CMake 4.5.1.
const KNOWN_FEATURES: [(&str, &str); 4] = [
    (
        "CMAKE_C_KNOWN_FEATURES",
        "c_std_90;c_std_99;c_std_11;c_std_17;c_std_23;c_function_prototypes;c_restrict;\
         c_variadic_macros;c_static_assert",
    ),
    (
        "CMAKE_CXX_KNOWN_FEATURES",
        "cxx_std_98;cxx_std_11;cxx_std_14;cxx_std_17;cxx_std_20;cxx_std_23;cxx_std_26;\
         cxx_template_template_parameters;cxx_alias_templates;cxx_alignas;cxx_alignof;\
         cxx_attributes;cxx_auto_type;cxx_constexpr;cxx_decltype;\
         cxx_decltype_incomplete_return_types;cxx_default_function_template_args;\
         cxx_defaulted_functions;cxx_defaulted_move_initializers;cxx_delegating_constructors;\
         cxx_deleted_functions;cxx_enum_forward_declarations;cxx_explicit_conversions;\
         cxx_extended_friend_declarations;cxx_extern_templates;cxx_final;\
         cxx_func_identifier;cxx_generalized_initializers;cxx_inheriting_constructors;\
         cxx_inline_namespaces;cxx_lambdas;cxx_local_type_template_args;\
         cxx_long_long_type;cxx_noexcept;cxx_nonstatic_member_init;cxx_nullptr;\
         cxx_override;cxx_range_for;cxx_raw_string_literals;\
         cxx_reference_qualified_functions;cxx_right_angle_brackets;\
         cxx_rvalue_references;cxx_sizeof_member;cxx_static_assert;cxx_strong_enums;\
         cxx_thread_local;cxx_trailing_return_types;cxx_unicode_literals;\
         cxx_uniform_initialization;cxx_unrestricted_unions;cxx_user_literals;\
         cxx_variadic_macros;cxx_variadic_templates;cxx_aggregate_default_initializers;\
         cxx_attribute_deprecated;cxx_binary_literals;cxx_contextual_conversions;\
         cxx_decltype_auto;cxx_digit_separators;cxx_generic_lambdas;\
         cxx_lambda_init_captures;cxx_relaxed_constexpr;cxx_return_type_deduction;\
         cxx_variable_templates",
    ),
    (
        "CMAKE_CUDA_KNOWN_FEATURES",
        "cuda_std_03;cuda_std_11;cuda_std_14;cuda_std_17;cuda_std_20;cuda_std_23;cuda_std_26",
    ),
    (
        "CMAKE_HIP_KNOWN_FEATURES",
        "hip_std_98;hip_std_11;hip_std_14;hip_std_17;hip_std_20;hip_std_23;hip_std_26",
    ),
];

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
    let generator = generator(cx);
    super::generator_env::initialize(cx, generator)?;
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

    // RC is enabled after the other languages so that an implicit RC enable does not
    // look like a recursive enable_language(RC).
    let (rc, other): (Vec<BString>, Vec<BString>) =
        unique.into_iter().partition(|lang| lang == b"RC");
    for group in [other, rc] {
        if !group.is_empty() {
            Bootstrap::new(cx, generator, optional).run(cx, group)?;
        }
    }
    Ok(())
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

    fn run(&self, cx: &mut Cx<'_>, mut languages: Vec<BString>) -> Result<(), CmdError> {
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
        let _ = cx.host().create_dir_all(&path::to_native(&self.root_bin));

        self.determine_system(cx)?;

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
        }
        if !cx.var_defined(b"CMAKE_SYSTEM") {
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
        Ok(())
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
            if !read_list_file(cx, &compiler_file)? {
                return Err(missing_module(cx, &compiler_file));
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
            let determine = cat(&[b"CMakeDetermine", lang, b"Compiler.cmake"]);
            self.require_module(cx, &String::from_utf8_lossy(&determine))?;
            self.export_compiler_env(cx, lang);
            if !read_list_file(cx, &compiler_file)? {
                return Err(missing_module(cx, &compiler_file));
            }
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
        if let Some(problem) = self.compiler_problem(cx, &lang) {
            state.need_test = false;
            if !self.optional {
                let _ = cx.host().remove_file(&path::to_native(&compiler_file));
                if !self.in_try_compile {
                    let env_var = cx.var(&lang_var(&lang, b"_COMPILER_ENV_VAR"));
                    let body = cat(&[
                        &problem,
                        &compiler_advice(&lang, env_var.as_deref().map(Vec::as_slice)),
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
    let props = &mut cx.global().props;
    if props.get(IN_TRY_COMPILE).is_none() {
        props.set(IN_TRY_COMPILE.into(), "0".into());
    }
    for (name, features) in KNOWN_FEATURES {
        if props.get(name.as_bytes()).is_none() {
            props.set(name.into(), features.into());
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

/// `cmGlobalGenerator::PrintCompilerAdvice`.
fn compiler_advice(lang: &[u8], env_var: Option<&[u8]>) -> BString {
    let either = env_var.map_or_else(Vec::new, |var| {
        cat(&[b"either the environment variable \"", var, b"\" or "]).to_vec()
    });
    cat(&[
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
    cx.cache().insert(
        name.into(),
        CacheEntry {
            value,
            ty,
            help: help.into(),
            advanced,
            props: Default::default(),
        },
    );
    if cx.var_normal_only(name).is_some() && cx.policy(PolicyId::new(126)) != PolicyStatus::New {
        cx.unset_var(name);
    }
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
