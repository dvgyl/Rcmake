//! Native per-configuration `compile_commands.json` extraction for Xcode.
//!
//! Shares option resolution with [`crate::settings`] and [`crate::settings::compile`]
//! so SDK, architectures, deployment target, defines, includes, frameworks, PCH, and
//! per-source flags match the generated Xcode project without scraping `project.pbxproj`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use rcmake_base::{BString, cmdline::split_unix, list as cmake_list, path_lex::Lexical, truth};
use rcmake_gen_common::GenError;
use rcmake_gen_common::compile_commands_native::{
    CommandEncoding, CompileRow, ConfigDatabase, IncompleteExport, MultiConfigCompileDatabase,
    absolute_path, contains_unresolved_ide_macro,
};
use rcmake_host::{Host, path::from_native_cmake};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{
    GenerationResult, ResolvedTarget,
    ide::{BundleKind, IdeCompileSettings, IdeFacts, IdeLinkItem, IdeSourceKind},
};

use crate::project::XcodeIndices;
use crate::settings::{self, Product};
use crate::text;

pub(crate) fn collect(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
    indices: &XcodeIndices<'_>,
    selected: &HashSet<TargetId>,
) -> Result<MultiConfigCompileDatabase, GenError> {
    let configs: Vec<String> = if result.configs.is_empty() {
        GeneratorKind::Xcode
            .default_configuration_types()
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        result.configs.iter().map(|cfg| text(&cfg.0)).collect()
    };
    let default_index = configs.iter().position(|name| name == "Debug").unwrap_or(0);
    let build_root = from_native_cmake(bin);
    let mut databases = Vec::with_capacity(configs.len());
    let mut diagnostics = Vec::new();

    for config in &configs {
        let project_attrs: BTreeMap<BString, BString> =
            rcmake_resolve::ide::xcode_project_attributes(model, DirId::ROOT, config.as_bytes())
                .map_err(|error| GenError::Invalid(error.to_string().into()))?
                .into_iter()
                .collect();

        let mut rows = Vec::new();
        for (target_id, raw) in model.targets() {
            if !selected.contains(&target_id)
                || !model.is_in_build_system(raw)
                || matches!(
                    raw.kind,
                    TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary
                )
            {
                continue;
            }
            let facts = indices
                .resolved
                .get(&(target_id, config.as_bytes()))
                .copied();
            collect_target(
                result,
                model,
                host,
                bin,
                &build_root,
                indices,
                config,
                &project_attrs,
                target_id,
                raw,
                facts,
                &mut rows,
                &mut diagnostics,
            );
        }

        databases.push(ConfigDatabase {
            config: BString::from(config.as_str()),
            rows,
        });
    }

    Ok(MultiConfigCompileDatabase {
        configs: databases,
        default_index,
        diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_target(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
    build_root: &[u8],
    indices: &XcodeIndices<'_>,
    config: &str,
    project_attrs: &BTreeMap<BString, BString>,
    target_id: TargetId,
    raw: &Target,
    facts: Option<&ResolvedTarget>,
    rows: &mut Vec<CompileRow>,
    diagnostics: &mut Vec<IncompleteExport>,
) {
    let ide = facts.and_then(|f| f.ide.as_deref());
    let product = Product::new(model, raw, facts, config, bin);
    let macro_env = MacroEnv::new(model, host, bin, raw, &product, config);

    let target_attrs = collect_target_xcode_attrs(ide, config);
    // Diagnose external xcconfig files if attached to the target or project.
    let xcconfig_attached = target_attrs.contains_key("XCODE_XCCONFIG")
        || ide
            .and_then(|f| f.attributes.get(b"XCODE_XCCONFIG".as_slice()))
            .is_some_and(|v| !v.is_empty())
        || model
            .definition(raw.dir, b"CMAKE_XCODE_XCCONFIG")
            .is_some_and(|v| !v.is_empty());
    if xcconfig_attached {
        diagnostics.push(IncompleteExport {
            config: BString::from(config),
            target: raw.name.clone(),
            reason: "Target uses an external .xcconfig file whose build settings cannot be expanded deterministically.".into(),
        });
        return;
    }

    // Check compile-affecting XCODE_ATTRIBUTE_* overrides for unresolvable macros.
    let extra_c_flags =
        match resolve_xcode_attr("OTHER_CFLAGS", &target_attrs, project_attrs, &macro_env) {
            Ok(val) => val,
            Err(reason) => {
                diagnostics.push(IncompleteExport {
                    config: BString::from(config),
                    target: raw.name.clone(),
                    reason,
                });
                return;
            }
        };
    let extra_cxx_flags = match resolve_xcode_attr(
        "OTHER_CPLUSPLUSFLAGS",
        &target_attrs,
        project_attrs,
        &macro_env,
    ) {
        Ok(val) => val,
        Err(reason) => {
            diagnostics.push(IncompleteExport {
                config: BString::from(config),
                target: raw.name.clone(),
                reason,
            });
            return;
        }
    };
    let extra_defines = match resolve_xcode_attr(
        "GCC_PREPROCESSOR_DEFINITIONS",
        &target_attrs,
        project_attrs,
        &macro_env,
    ) {
        Ok(val) => val,
        Err(reason) => {
            diagnostics.push(IncompleteExport {
                config: BString::from(config),
                target: raw.name.clone(),
                reason,
            });
            return;
        }
    };
    let extra_headers = match resolve_xcode_attr(
        "HEADER_SEARCH_PATHS",
        &target_attrs,
        project_attrs,
        &macro_env,
    ) {
        Ok(val) => val,
        Err(reason) => {
            diagnostics.push(IncompleteExport {
                config: BString::from(config),
                target: raw.name.clone(),
                reason,
            });
            return;
        }
    };

    // Resolve SDKROOT -> -isysroot.
    let sdk = match resolve_sdk(model, raw.dir, &target_attrs, project_attrs, &macro_env) {
        Ok(sdk) => sdk,
        Err(reason) => {
            diagnostics.push(IncompleteExport {
                config: BString::from(config),
                target: raw.name.clone(),
                reason,
            });
            return;
        }
    };

    // Resolve architectures (supporting universal binaries with multiple -arch rows).
    let archs = match resolve_archs(
        model,
        host,
        raw,
        config,
        &target_attrs,
        project_attrs,
        &macro_env,
    ) {
        Ok(archs) => archs,
        Err(reason) => {
            diagnostics.push(IncompleteExport {
                config: BString::from(config),
                target: raw.name.clone(),
                reason,
            });
            return;
        }
    };

    // Resolve deployment target flag.
    let deployment_flag = deployment_target_flag(model, raw.dir);

    // Collect framework search paths from linked framework files / framework targets.
    let linked_frameworks = linked_framework_search_paths(model, indices, bin, config, ide);

    let mut missing_compilers: BTreeSet<BString> = BTreeSet::new();

    if let Some(ide) = ide {
        for source in &ide.sources {
            let IdeSourceKind::Compile { language } = &source.kind else {
                continue;
            };
            let Some(compiler) = resolve_compiler(result, model, raw.dir, language) else {
                if missing_compilers.insert(language.clone()) {
                    diagnostics.push(IncompleteExport {
                        config: BString::from(config),
                        target: raw.name.clone(),
                        reason: format!(
                            "No compiler executable recorded for language {}.",
                            text(language)
                        )
                        .into(),
                    });
                }
                continue;
            };
            let launchers = resolve_launcher(model, raw, language);
            let group = ide.compile.iter().find(|g| g.language == *language);
            let implicit = settings::compile::implicit_includes(model, raw.dir, language);
            let pch = ide.pch.iter().find(|p| p.language == *language);
            let has_any_pch = !ide.pch.is_empty();
            let extra_lang_flags = match language.as_slice() {
                b"C" | b"OBJC" => extra_c_flags.as_deref(),
                b"CXX" | b"OBJCXX" => extra_cxx_flags.as_deref(),
                _ => None,
            };

            let emit_archs: Vec<Option<&BString>> = if matches!(
                language.as_slice(),
                b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM"
            ) && !archs.is_empty()
            {
                archs.iter().map(Some).collect()
            } else {
                vec![None]
            };

            let single_arch = emit_archs.len() <= 1;
            let resolved_unit_obj = if single_arch {
                facts.and_then(|f| {
                    f.compile_units
                        .iter()
                        .find(|u| u.source.value == source.path)
                        .map(|u| absolute_path(build_root, &u.object))
                })
            } else {
                None
            };

            for arch in emit_archs {
                let mut argv = Vec::new();
                argv.extend(launchers.iter().cloned());
                argv.push(compiler.clone());
                if let Some(arch) = arch {
                    argv.push(BString::from("-arch"));
                    argv.push(arch.clone());
                }
                if let Some(sysroot) = &sdk {
                    argv.push(BString::from("-isysroot"));
                    argv.push(sysroot.clone());
                }
                if let Some(deploy) = &deployment_flag
                    && language.as_slice() != b"Swift"
                {
                    argv.push(deploy.clone());
                }

                // Defines: CMAKE_INTDIR + group defines + XCODE_ATTRIBUTE defines + per-source defines.
                if language.as_slice() == b"Swift" {
                    if let Some(group) = group {
                        for define in &group.defines {
                            let raw_def = text(&define.value);
                            let name = raw_def.split('=').next().unwrap_or(&raw_def);
                            if !name.is_empty() {
                                argv.push(BString::from("-D"));
                                argv.push(BString::from(name));
                            }
                        }
                    }
                } else {
                    argv.push(BString::from(format!("-DCMAKE_INTDIR=\"{config}\"")));
                    if let Some(group) = group {
                        for define in &group.defines {
                            argv.push(define_arg(&define.value));
                        }
                    }
                    if let Some(extra) = &extra_defines {
                        for token in split_unix(extra.as_bytes()) {
                            if !token.is_empty() && token.as_slice() != b"$(inherited)" {
                                argv.push(define_arg(&token));
                            }
                        }
                    }
                }

                // Target includes (filtering implicit includes, keeping exact order).
                if let Some(group) = group {
                    for include in &group.includes {
                        if settings::compile::is_implicit_include(&include.path, &implicit) {
                            continue;
                        }
                        push_include_arg(
                            &mut argv,
                            &include.path,
                            include.system,
                            include.framework,
                        );
                    }
                }
                if let Some(extra) = &extra_headers {
                    for token in split_unix(extra.as_bytes()) {
                        if !token.is_empty() && token.as_slice() != b"$(inherited)" {
                            argv.push(BString::from([b"-I".as_slice(), token.as_slice()].concat()));
                        }
                    }
                }
                for fw in &linked_frameworks {
                    argv.push(BString::from([b"-F".as_slice(), fw.as_slice()].concat()));
                }

                // Target compile flags.
                if let Some(group) = group {
                    push_ide_flags(&mut argv, group);
                }
                if let Some(extra) = extra_lang_flags {
                    for token in split_unix(extra.as_bytes()) {
                        if !token.is_empty() && token.as_slice() != b"$(inherited)" {
                            argv.push(token);
                        }
                    }
                }

                // Precompiled header flags matching project/sources.rs.
                if settings::compile::supported(language) && has_any_pch {
                    if let Some(pch) = pch
                        && !source.skip_precompile_headers
                    {
                        argv.push(BString::from("-include"));
                        argv.push(pch.header.clone());
                    } else if source.skip_precompile_headers || pch.is_none() {
                        argv.push(BString::from("-DCMAKE_SKIP_PRECOMPILE_HEADERS"));
                    }
                }

                // Per-source overrides (defines, includes, flags).
                if let Some(src_compile) = &source.compile {
                    for define in &src_compile.defines {
                        argv.push(define_arg(&define.value));
                    }
                    for include in &src_compile.includes {
                        push_include_arg(
                            &mut argv,
                            &include.path,
                            include.system,
                            include.framework,
                        );
                    }
                    push_ide_flags(&mut argv, src_compile);
                }

                let object = resolved_unit_obj.clone().unwrap_or_else(|| {
                    xcode_object_path(
                        build_root,
                        &raw.name,
                        config,
                        arch.map(|a| a.as_slice()),
                        &source.path,
                    )
                });

                argv.push(BString::from("-c"));
                argv.push(source.path.clone());
                argv.push(BString::from("-o"));
                argv.push(object.clone());

                if let Some(bad) = argv.iter().find(|arg| contains_unresolved_ide_macro(arg)) {
                    diagnostics.push(IncompleteExport {
                        config: BString::from(config),
                        target: raw.name.clone(),
                        reason: format!(
                            "Unresolved Xcode macro in compile argument {:?} for source {}.",
                            text(bad),
                            text(&source.path)
                        )
                        .into(),
                    });
                    continue;
                }

                rows.push(CompileRow {
                    config: BString::from(config),
                    target: target_id,
                    directory: BString::from(build_root),
                    file: source.path.clone(),
                    output: object,
                    invocation: CommandEncoding::Arguments(argv),
                });
            }
        }
    } else if let Some(facts) = facts {
        // Fallback when called without IdeFlavor::Xcode (e.g. direct unit test calls).
        for unit in &facts.compile_units {
            let Some(group) = facts.compile_groups.get(unit.group) else {
                continue;
            };
            let Some(compiler) = resolve_compiler(result, model, raw.dir, &group.language) else {
                if missing_compilers.insert(group.language.clone()) {
                    diagnostics.push(IncompleteExport {
                        config: BString::from(config),
                        target: raw.name.clone(),
                        reason: format!(
                            "No compiler executable recorded for language {}.",
                            text(&group.language)
                        )
                        .into(),
                    });
                }
                continue;
            };
            let launchers = resolve_launcher(model, raw, &group.language);
            let mut argv = Vec::new();
            argv.extend(launchers);
            argv.push(compiler);
            if let Some(sysroot) = &sdk {
                argv.push(BString::from("-isysroot"));
                argv.push(sysroot.clone());
            }
            if let Some(deploy) = &deployment_flag {
                argv.push(deploy.clone());
            }
            argv.push(BString::from(format!("-DCMAKE_INTDIR=\"{config}\"")));
            for define in &group.defines {
                argv.push(define_arg(&define.value));
            }
            for include in &group.includes {
                push_include_arg(&mut argv, &include.path.value, include.system, false);
            }
            for flag in &group.flags {
                push_flag_tokens(&mut argv, &flag.value);
            }
            let object = absolute_path(build_root, &unit.object);
            argv.push(BString::from("-c"));
            argv.push(unit.source.value.clone());
            argv.push(BString::from("-o"));
            argv.push(object.clone());
            if let Some(bad) = argv.iter().find(|arg| contains_unresolved_ide_macro(arg)) {
                diagnostics.push(IncompleteExport {
                    config: BString::from(config),
                    target: raw.name.clone(),
                    reason: format!(
                        "Unresolved Xcode macro in compile argument {:?}.",
                        text(bad)
                    )
                    .into(),
                });
                continue;
            }
            rows.push(CompileRow {
                config: BString::from(config),
                target: target_id,
                directory: BString::from(build_root),
                file: unit.source.value.clone(),
                output: object,
                invocation: CommandEncoding::Arguments(argv),
            });
        }
    }
}

struct MacroEnv {
    config: String,
    project_dir: String,
    symroot: String,
    target_temp_dir: String,
    config_build_dir: String,
    native_arch: String,
}

impl MacroEnv {
    fn new(
        model: &ProjectModel,
        host: &dyn Host,
        bin: &Path,
        raw: &Target,
        product: &Product,
        config: &str,
    ) -> Self {
        let project_dir = text(&model.dir(DirId::ROOT).source_dir);
        let symroot = from_native_cmake(&bin.join("build")).to_string();
        let target_temp_dir =
            from_native_cmake(&bin.join(format!("build/{}.build/{config}", text(&raw.name))))
                .to_string();
        let native_arch = text(&host.system().machine);
        Self {
            config: config.to_owned(),
            project_dir,
            symroot,
            target_temp_dir,
            config_build_dir: product.dir.clone(),
            native_arch,
        }
    }

    fn expand(&self, raw: &str) -> String {
        raw.replace("$(CONFIGURATION)", &self.config)
            .replace("$(EFFECTIVE_PLATFORM_NAME)", "")
            .replace("$(PROJECT_DIR)", &self.project_dir)
            .replace("$(SRCROOT)", &self.project_dir)
            .replace("$(SYMROOT)", &self.symroot)
            .replace("$(TARGET_TEMP_DIR)", &self.target_temp_dir)
            .replace("$(CONFIGURATION_BUILD_DIR)", &self.config_build_dir)
            .replace("$(NATIVE_ARCH_ACTUAL)", &self.native_arch)
            .replace("$(inherited)", "")
    }
}

fn collect_target_xcode_attrs(ide: Option<&IdeFacts>, config: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(ide) = ide else {
        return out;
    };
    for (key, value) in &ide.attributes {
        let Ok(key) = std::str::from_utf8(key) else {
            continue;
        };
        let Some(rest) = key.strip_prefix("XCODE_ATTRIBUTE_") else {
            continue;
        };
        let name = if let Some((head, tail)) = rest.split_once("[variant=") {
            let Some((variant, after)) = tail.split_once(']') else {
                continue;
            };
            if variant != config {
                continue;
            }
            format!("{head}{after}")
        } else {
            rest.to_owned()
        };
        out.insert(name, text(value));
    }
    out
}

fn resolve_xcode_attr(
    name: &str,
    target_attrs: &BTreeMap<String, String>,
    project_attrs: &BTreeMap<BString, BString>,
    env: &MacroEnv,
) -> Result<Option<String>, BString> {
    let raw = target_attrs
        .get(name)
        .cloned()
        .or_else(|| project_attrs.get(name.as_bytes()).map(|v| text(v)));
    let Some(raw) = raw else {
        return Ok(None);
    };
    let expanded = env.expand(&raw);
    if contains_unresolved_ide_macro(expanded.as_bytes()) {
        return Err(format!("Unresolved Xcode macro in {name}: {expanded}").into());
    }
    let trimmed = expanded.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

fn resolve_sdk(
    model: &ProjectModel,
    dir: DirId,
    target_attrs: &BTreeMap<String, String>,
    project_attrs: &BTreeMap<BString, BString>,
    env: &MacroEnv,
) -> Result<Option<BString>, BString> {
    if let Some(attr) = resolve_xcode_attr("SDKROOT", target_attrs, project_attrs, env)? {
        if attr.starts_with('/') {
            return Ok(Some(BString::from(attr)));
        }
        // Short SDK name (for example `macosx`): use CMake's resolved sysroot path if available.
        if let Some(path) = model
            .definition(dir, b"_CMAKE_OSX_SYSROOT_PATH")
            .filter(|v| v.starts_with(b"/"))
        {
            return Ok(Some(path.clone()));
        }
        return Ok(None);
    }
    let sdk = model
        .definition(dir, b"_CMAKE_OSX_SYSROOT_PATH")
        .filter(|v| v.starts_with(b"/"))
        .or_else(|| {
            model
                .definition(dir, b"CMAKE_OSX_SYSROOT")
                .filter(|v| v.starts_with(b"/"))
        });
    Ok(sdk.cloned())
}

fn resolve_archs(
    model: &ProjectModel,
    host: &dyn Host,
    raw: &Target,
    config: &str,
    target_attrs: &BTreeMap<String, String>,
    project_attrs: &BTreeMap<BString, BString>,
    env: &MacroEnv,
) -> Result<Vec<BString>, BString> {
    if let Some(attr) = resolve_xcode_attr("ARCHS", target_attrs, project_attrs, env)? {
        let parsed: Vec<BString> = attr
            .split(|c: char| c.is_ascii_whitespace() || c == ';')
            .filter(|s| !s.is_empty())
            .map(BString::from)
            .collect();
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }
    let arch_key = format!("OSX_ARCHITECTURES_{}", config.to_ascii_uppercase());
    let raw_list = raw
        .props
        .get(arch_key.as_bytes())
        .or_else(|| raw.props.get(b"OSX_ARCHITECTURES"))
        .or_else(|| model.definition(raw.dir, b"CMAKE_OSX_ARCHITECTURES"))
        .or_else(|| model.definition(raw.dir, b"_CMAKE_APPLE_ARCHS_DEFAULT"))
        .filter(|v| !v.is_empty());
    if let Some(list) = raw_list {
        let mut out = Vec::new();
        for item in cmake_list::split(list, cmake_list::Empty::Drop) {
            let expanded = env.expand(&text(&item));
            if contains_unresolved_ide_macro(expanded.as_bytes()) {
                return Err(format!("Unresolved Xcode macro in ARCHS: {expanded}").into());
            }
            for part in expanded.split_whitespace() {
                if !part.is_empty() {
                    out.push(BString::from(part));
                }
            }
        }
        return Ok(out);
    }
    let system = model
        .definition(raw.dir, b"CMAKE_SYSTEM_NAME")
        .map(|v| v.as_slice())
        .unwrap_or_default();
    if system == b"Darwin" && !host.system().machine.is_empty() {
        return Ok(vec![host.system().machine]);
    }
    Ok(Vec::new())
}

fn deployment_target_flag(model: &ProjectModel, dir: DirId) -> Option<BString> {
    let deployment = model
        .definition(dir, b"CMAKE_OSX_DEPLOYMENT_TARGET")
        .filter(|v| !v.is_empty())?;
    let flag_prefix = match model
        .definition(dir, b"CMAKE_SYSTEM_NAME")
        .map(|v| v.as_slice())
    {
        Some(b"iOS") => b"-miphoneos-version-min=".as_slice(),
        Some(b"tvOS") => b"-mtvos-version-min=".as_slice(),
        Some(b"watchOS") => b"-mwatchos-version-min=".as_slice(),
        Some(b"visionOS") => b"-mxros-version-min=".as_slice(),
        _ => b"-mmacosx-version-min=".as_slice(),
    };
    Some([flag_prefix, deployment.as_slice()].concat().into())
}

fn linked_framework_search_paths(
    model: &ProjectModel,
    indices: &XcodeIndices<'_>,
    bin: &Path,
    config: &str,
    ide: Option<&IdeFacts>,
) -> Vec<BString> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let Some(ide) = ide else {
        return out;
    };
    for item in &ide.link_items {
        match item {
            IdeLinkItem::File(path) | IdeLinkItem::Framework(path) => {
                let lex = Lexical::new(path);
                if lex.absolute() && lex.filename().ends_with(b".framework") {
                    let parent = BString::from(lex.parent());
                    if !parent.is_empty() && seen.insert(parent.clone()) {
                        out.push(parent);
                    }
                }
            }
            IdeLinkItem::Target(id) => {
                if let Some(dep) = model.target(*id) {
                    let resolved = indices.resolved.get(&(*id, config.as_bytes())).copied();
                    let product = Product::new(model, dep, resolved, config, bin);
                    if matches!(product.bundle, BundleKind::Framework { .. }) {
                        let dir = BString::from(product.dir);
                        if !dir.is_empty() && seen.insert(dir.clone()) {
                            out.push(dir);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn resolve_compiler(
    result: &GenerationResult,
    model: &ProjectModel,
    dir: DirId,
    language: &[u8],
) -> Option<BString> {
    if let Some(tc) = result.metadata.toolchains.get(language)
        && !tc.compiler_path.is_empty()
        && !tc.compiler_path.ends_with(b"-NOTFOUND")
    {
        return Some(tc.compiler_path.clone());
    }
    let key = [b"CMAKE_", language, b"_COMPILER"].concat();
    model
        .definition(dir, &key)
        .filter(|v| !v.is_empty() && !v.ends_with(b"-NOTFOUND") && !truth::is_off(v))
        .cloned()
}

fn resolve_launcher(model: &ProjectModel, raw: &Target, language: &[u8]) -> Vec<BString> {
    let prop = [language, b"_COMPILER_LAUNCHER"].concat();
    let var = [b"CMAKE_", language, b"_COMPILER_LAUNCHER"].concat();
    raw.props
        .get(&prop)
        .or_else(|| model.definition(raw.dir, &var))
        .map_or_else(Vec::new, |val| {
            cmake_list::split(val, cmake_list::Empty::Drop)
        })
}

fn define_arg(define: &[u8]) -> BString {
    if define.starts_with(b"-D") {
        BString::from(define)
    } else {
        [b"-D".as_slice(), define].concat().into()
    }
}

fn push_include_arg(argv: &mut Vec<BString>, path: &[u8], system: bool, framework: bool) {
    match (system, framework) {
        (false, false) => argv.push([b"-I".as_slice(), path].concat().into()),
        (true, false) => {
            argv.push(BString::from("-isystem"));
            argv.push(BString::from(path));
        }
        (false, true) => argv.push([b"-F".as_slice(), path].concat().into()),
        (true, true) => {
            argv.push(BString::from("-iframework"));
            argv.push(BString::from(path));
        }
    }
}

fn push_ide_flags(argv: &mut Vec<BString>, settings: &IdeCompileSettings) {
    for flag in &settings.flags {
        push_flag_tokens(argv, &flag.value);
    }
}

fn push_flag_tokens(argv: &mut Vec<BString>, raw: &[u8]) {
    if raw.is_empty() {
        return;
    }
    if let Some(shell) = raw.strip_prefix(b"SHELL:") {
        for token in split_unix(shell) {
            if !token.is_empty() {
                argv.push(token);
            }
        }
    } else {
        argv.push(BString::from(raw));
    }
}

fn xcode_object_path(
    build_root: &[u8],
    target_name: &[u8],
    config: &str,
    arch: Option<&[u8]>,
    source: &[u8],
) -> BString {
    let arch_dir = arch.map_or_else(|| "normal".to_owned(), text);
    let stem = Path::new(&text(source))
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unit".to_owned());
    let rel = format!(
        "build/{}.build/{config}/Objects-normal/{arch_dir}/{stem}.o",
        text(target_name)
    );
    absolute_path(build_root, rel.as_bytes())
}
