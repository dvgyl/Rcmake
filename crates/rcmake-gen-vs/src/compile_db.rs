//! Native per-configuration `compile_commands.json` extraction for Visual Studio.
//!
//! Uses the exact project/configuration/platform plan from [`crate::plan`] and
//! [`crate::vcxproj`] without scraping generated XML or inventing invocations
//! for unresolvable MSBuild macros.

use std::collections::BTreeSet;

use rcmake_base::{BString, list, truth};
use rcmake_gen_common::compile_commands_native::{
    CommandEncoding, CompileRow, ConfigDatabase, IncompleteExport, contains_unresolved_ide_macro,
};
use rcmake_model::{DirId, ProjectModel, Target, TargetId};
use rcmake_resolve::{
    GenerationResult,
    ide::{IdeCompileSettings, IdeSource},
};

use crate::vcxproj::{self, Kind, Plan};
use crate::{flags_table, string, windows};

pub(crate) fn collect_project(
    ctx: &vcxproj::Context<'_>,
    plan: &Plan,
    result: &GenerationResult,
    databases: &mut [ConfigDatabase],
    diagnostics: &mut Vec<IncompleteExport>,
) {
    let Some(target_id) = ctx.project.target else {
        return;
    };
    let Some(raw) = ctx.model.target(target_id) else {
        return;
    };

    for (config_index, config_name) in ctx.settings.configs.iter().enumerate() {
        let Some(db) = databases.get_mut(config_index) else {
            continue;
        };
        let Some(cfg) = plan.configs.get(config_name) else {
            continue;
        };
        let fact = ctx.facts.and_then(|facts| facts.get(config_name)).copied();
        let ide = fact.and_then(|f| f.ide.as_deref());
        let env = VsMacroEnv {
            config: config_name,
            platform: &ctx.settings.platform,
            int_dir: format!("{}\\", windows(&cfg.int_dir)),
            out_dir: format!("{}\\", windows(&cfg.dir)),
            project_dir: format!("{}\\", windows(&ctx.project.dir)),
            solution_dir: format!("{}\\", windows(&ctx.settings.bin)),
        };

        let mut missing_compilers: BTreeSet<BString> = BTreeSet::new();

        for item in plan.items.values() {
            if !item.kind.compiles() {
                continue;
            }
            let Some(src_cfg) = item.configs.get(config_name).filter(|s| !s.excluded) else {
                continue;
            };
            match item.kind {
                Kind::Compile => collect_c_cpp_item(
                    ctx,
                    result,
                    raw,
                    target_id,
                    config_name,
                    &env,
                    ide,
                    item,
                    src_cfg,
                    &mut missing_compilers,
                    &mut db.rows,
                    diagnostics,
                ),
                Kind::Cuda => collect_cuda_item(
                    ctx,
                    result,
                    raw,
                    target_id,
                    config_name,
                    &env,
                    cfg.cuda.as_ref(),
                    item,
                    src_cfg,
                    &mut missing_compilers,
                    &mut db.rows,
                    diagnostics,
                ),
                Kind::Masm | Kind::Nasm => collect_asm_item(
                    ctx,
                    result,
                    raw,
                    target_id,
                    config_name,
                    &env,
                    item,
                    src_cfg,
                    &mut missing_compilers,
                    &mut db.rows,
                    diagnostics,
                ),
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_c_cpp_item(
    ctx: &vcxproj::Context<'_>,
    result: &GenerationResult,
    raw: &Target,
    target_id: TargetId,
    config_name: &str,
    env: &VsMacroEnv<'_>,
    ide: Option<&rcmake_resolve::ide::IdeFacts>,
    item: &vcxproj::Item,
    src_cfg: &vcxproj::SourceConfig,
    missing_compilers: &mut BTreeSet<BString>,
    rows: &mut Vec<CompileRow>,
    diagnostics: &mut Vec<IncompleteExport>,
) {
    let language = if src_cfg.language.is_empty() {
        vcxproj::source_language(&item.path)
    } else {
        src_cfg.language.as_str()
    };
    let lang_bytes = language.as_bytes();
    let Some(compiler) = resolve_compiler(result, ctx.model, raw.dir, lang_bytes) else {
        if missing_compilers.insert(BString::from(lang_bytes)) {
            diagnostics.push(IncompleteExport {
                config: BString::from(config_name),
                target: raw.name.clone(),
                reason: format!("No compiler executable recorded for language {language}.").into(),
            });
        }
        return;
    };
    let gnu_frontend = is_gnu_frontend(ctx.model, raw.dir, lang_bytes);
    let launchers = resolve_launcher(ctx.model, raw, lang_bytes);
    let mut argv = Vec::new();
    argv.extend(launchers);
    argv.push(compiler);

    let define_prefix = if gnu_frontend { "-D" } else { "/D" };
    let include_prefix = if gnu_frontend { "-I" } else { "/I" };

    // Always emit CMAKE_INTDIR matching vcxproj.rs ItemDefinitionGroup ClCompile.
    argv.push(BString::from(format!(
        "{define_prefix}CMAKE_INTDIR=\"{config_name}\""
    )));

    let base_ide = ide.and_then(|facts| {
        facts
            .compile
            .iter()
            .find(|settings| settings.language.as_slice() == lang_bytes)
    });
    let source_ide = ide.and_then(|facts| {
        facts
            .sources
            .iter()
            .find(|src| crate::absolute(&ctx.project.source, &string(&src.path)) == item.path)
    });

    if let Some(base) = base_ide {
        push_ide_compile_args(&mut argv, base, define_prefix, include_prefix, env);
        if let Some(overrides) = source_ide.and_then(|src| src.compile.as_ref()) {
            push_ide_compile_args(&mut argv, overrides, define_prefix, include_prefix, env);
        }
    } else if let Some(fact) = ctx.facts.and_then(|facts| facts.get(config_name)).copied()
        && let Some(unit) = fact.compile_units.iter().find(|unit| {
            crate::absolute(&ctx.project.source, &string(&unit.source.value)) == item.path
        })
        && let Some(group) = fact.compile_groups.get(unit.group)
    {
        let key = format!("CMAKE_{language}_FLAGS");
        let specific = format!("{key}_{}", config_name.to_ascii_uppercase());
        for var in [key, specific] {
            if let Some(value) = ctx.model.definition(raw.dir, var.as_bytes()) {
                for tok in flags_table::split(&string(value)) {
                    argv.push(BString::from(env.expand(&tok)));
                }
            }
        }
        for flag in &group.flags {
            push_raw_flag(&mut argv, &string(&flag.value), env);
        }
        for define in &group.defines {
            argv.push(BString::from(format!(
                "{define_prefix}{}",
                env.expand(&string(&define.value))
            )));
        }
        for include in &group.includes {
            argv.push(BString::from(format!(
                "{include_prefix}{}",
                env.expand(&string(&include.path.value))
            )));
        }
    } else if let Some(settings) = &src_cfg.compile {
        push_flags_table_fallback(&mut argv, settings, define_prefix, include_prefix, env);
    }

    // Explicit CompileAs override matching vcxproj/render.rs:337-350.
    if !gnu_frontend
        && matches!(language, "C" | "CXX")
        && language != vcxproj::source_language(&item.path)
        && !src_cfg
            .compile
            .as_ref()
            .is_some_and(|settings| settings.has("CompileAs"))
    {
        argv.push(BString::from(if language == "C" { "/TC" } else { "/TP" }));
    }

    // Precompiled header flags matching vcxproj/pch.rs.
    if let Some(ide) = ide
        && let Some(src) = source_ide
    {
        push_pch_args(&mut argv, ide, src, lang_bytes, gnu_frontend);
    }

    let object = env.expand(&src_cfg.object);
    let source = env.expand(&item.path);
    if gnu_frontend {
        argv.push(BString::from("-c"));
        argv.push(BString::from(source.as_str()));
        argv.push(BString::from("-o"));
        argv.push(BString::from(object.as_str()));
    } else {
        argv.push(BString::from("/c"));
        argv.push(BString::from(format!("/Fo{}", windows(&object))));
        argv.push(BString::from(windows(&source)));
    }

    if let Some(bad) = argv.iter().find(|arg| contains_unresolved_ide_macro(arg)) {
        diagnostics.push(IncompleteExport {
            config: BString::from(config_name),
            target: raw.name.clone(),
            reason: format!(
                "Unresolved Visual Studio MSBuild macro in compile argument {:?} for source {}.",
                string(bad),
                item.path
            )
            .into(),
        });
        return;
    }

    rows.push(CompileRow {
        config: BString::from(config_name),
        target: target_id,
        directory: BString::from(ctx.project.dir.as_str()),
        file: BString::from(source),
        output: BString::from(object),
        invocation: CommandEncoding::Arguments(argv),
    });
}

#[allow(clippy::too_many_arguments)]
fn collect_cuda_item(
    ctx: &vcxproj::Context<'_>,
    result: &GenerationResult,
    raw: &Target,
    target_id: TargetId,
    config_name: &str,
    env: &VsMacroEnv<'_>,
    cuda: Option<&vcxproj::CudaConfig>,
    item: &vcxproj::Item,
    src_cfg: &vcxproj::SourceConfig,
    missing_compilers: &mut BTreeSet<BString>,
    rows: &mut Vec<CompileRow>,
    diagnostics: &mut Vec<IncompleteExport>,
) {
    let Some(compiler) = resolve_compiler(result, ctx.model, raw.dir, b"CUDA") else {
        if missing_compilers.insert(BString::from("CUDA")) {
            diagnostics.push(IncompleteExport {
                config: BString::from(config_name),
                target: raw.name.clone(),
                reason: "No compiler executable recorded for language CUDA.".into(),
            });
        }
        return;
    };
    let mut argv = resolve_launcher(ctx.model, raw, b"CUDA");
    argv.push(compiler);
    argv.push(BString::from(format!("-DCMAKE_INTDIR=\"{config_name}\"")));

    if let Some(cuda) = cuda {
        for define in &cuda.defines {
            argv.push(BString::from(format!("-D{}", env.expand(define))));
        }
        for include in &cuda.includes {
            argv.push(BString::from(format!("-I{}", env.expand(include))));
        }
        for arch in &cuda.architectures {
            argv.push(BString::from(env.expand(arch)));
        }
        for opt in flags_table::split(&cuda.options) {
            argv.push(BString::from(env.expand(&opt)));
        }
        if let Some(mode) = cuda.mode {
            argv.push(BString::from(format!("--{}", mode.nvcc)));
        } else if cuda.separable {
            argv.push(BString::from("-dc"));
        } else {
            argv.push(BString::from("-c"));
        }
    } else {
        argv.push(BString::from("-c"));
    }

    if let Some(overrides) = &src_cfg.tool_compile {
        push_ide_compile_args(&mut argv, overrides, "-D", "-I", env);
    }

    let object = env.expand(&src_cfg.object);
    let source = env.expand(&item.path);
    argv.push(BString::from("-o"));
    argv.push(BString::from(object.as_str()));
    argv.push(BString::from(source.as_str()));

    if let Some(bad) = argv.iter().find(|arg| contains_unresolved_ide_macro(arg)) {
        diagnostics.push(IncompleteExport {
            config: BString::from(config_name),
            target: raw.name.clone(),
            reason: format!(
                "Unresolved Visual Studio MSBuild macro in CUDA compile argument {:?} for source {}.",
                string(bad),
                item.path
            )
            .into(),
        });
        return;
    }

    rows.push(CompileRow {
        config: BString::from(config_name),
        target: target_id,
        directory: BString::from(ctx.project.dir.as_str()),
        file: BString::from(source),
        output: BString::from(object),
        invocation: CommandEncoding::Arguments(argv),
    });
}

#[allow(clippy::too_many_arguments)]
fn collect_asm_item(
    ctx: &vcxproj::Context<'_>,
    result: &GenerationResult,
    raw: &Target,
    target_id: TargetId,
    config_name: &str,
    env: &VsMacroEnv<'_>,
    item: &vcxproj::Item,
    src_cfg: &vcxproj::SourceConfig,
    missing_compilers: &mut BTreeSet<BString>,
    rows: &mut Vec<CompileRow>,
    diagnostics: &mut Vec<IncompleteExport>,
) {
    let lang: &[u8] = if item.kind == Kind::Masm {
        b"ASM_MASM"
    } else {
        b"ASM_NASM"
    };
    let Some(compiler) = resolve_compiler(result, ctx.model, raw.dir, lang) else {
        if missing_compilers.insert(BString::from(lang)) {
            diagnostics.push(IncompleteExport {
                config: BString::from(config_name),
                target: raw.name.clone(),
                reason: format!(
                    "No compiler executable recorded for language {}.",
                    string(lang)
                )
                .into(),
            });
        }
        return;
    };
    let mut argv = vec![compiler];
    let (def_prefix, inc_prefix) = if item.kind == Kind::Masm {
        ("/D", "/I")
    } else {
        ("-D", "-I")
    };
    if let Some(overrides) = &src_cfg.tool_compile {
        push_ide_compile_args(&mut argv, overrides, def_prefix, inc_prefix, env);
    }
    let object = env.expand(&src_cfg.object);
    let source = env.expand(&item.path);
    if item.kind == Kind::Masm {
        argv.push(BString::from("/c"));
        argv.push(BString::from(format!("/Fo{}", windows(&object))));
        argv.push(BString::from(windows(&source)));
    } else {
        argv.push(BString::from("-o"));
        argv.push(BString::from(object.as_str()));
        argv.push(BString::from(source.as_str()));
    }
    if let Some(bad) = argv.iter().find(|arg| contains_unresolved_ide_macro(arg)) {
        diagnostics.push(IncompleteExport {
            config: BString::from(config_name),
            target: raw.name.clone(),
            reason: format!(
                "Unresolved Visual Studio MSBuild macro in assembler argument {:?} for source {}.",
                string(bad),
                item.path
            )
            .into(),
        });
        return;
    }
    rows.push(CompileRow {
        config: BString::from(config_name),
        target: target_id,
        directory: BString::from(ctx.project.dir.as_str()),
        file: BString::from(source),
        output: BString::from(object),
        invocation: CommandEncoding::Arguments(argv),
    });
}

struct VsMacroEnv<'a> {
    config: &'a str,
    platform: &'a str,
    int_dir: String,
    out_dir: String,
    project_dir: String,
    solution_dir: String,
}

impl VsMacroEnv<'_> {
    fn expand(&self, raw: &str) -> String {
        raw.replace("$(Configuration)", self.config)
            .replace("$(Platform)", self.platform)
            .replace("$(IntDir)", &self.int_dir)
            .replace("$(OutDir)", &self.out_dir)
            .replace("$(ProjectDir)", &self.project_dir)
            .replace("$(SolutionDir)", &self.solution_dir)
            .replace("%3B", ";")
    }
}

fn push_ide_compile_args(
    argv: &mut Vec<BString>,
    settings: &IdeCompileSettings,
    define_prefix: &str,
    include_prefix: &str,
    env: &VsMacroEnv<'_>,
) {
    for define in &settings.defines {
        let expanded = env.expand(&string(&define.value));
        if expanded.starts_with("/D") || expanded.starts_with("-D") {
            argv.push(BString::from(expanded));
        } else {
            argv.push(BString::from(format!("{define_prefix}{expanded}")));
        }
    }
    for include in &settings.includes {
        let expanded = env.expand(&string(&include.path));
        argv.push(BString::from(format!("{include_prefix}{expanded}")));
    }
    for flag in &settings.flags {
        push_raw_flag(argv, &string(&flag.value), env);
    }
}

fn push_raw_flag(argv: &mut Vec<BString>, raw: &str, env: &VsMacroEnv<'_>) {
    if raw.is_empty() {
        return;
    }
    if let Some(shell) = raw.strip_prefix("SHELL:") {
        for token in flags_table::split(shell) {
            if !token.is_empty() {
                argv.push(BString::from(env.expand(&token)));
            }
        }
    } else {
        argv.push(BString::from(env.expand(raw)));
    }
}

fn push_flags_table_fallback(
    argv: &mut Vec<BString>,
    settings: &flags_table::Settings,
    define_prefix: &str,
    include_prefix: &str,
    env: &VsMacroEnv<'_>,
) {
    for def in settings.list_values("PreprocessorDefinitions") {
        let expanded = env.expand(def);
        if !expanded.is_empty() {
            argv.push(BString::from(format!("{define_prefix}{expanded}")));
        }
    }
    for inc in settings.list_values("AdditionalIncludeDirectories") {
        let expanded = env.expand(inc);
        if !expanded.is_empty() {
            argv.push(BString::from(format!("{include_prefix}{expanded}")));
        }
    }
    for opt in settings.additional_options() {
        let expanded = env.expand(opt);
        if !expanded.is_empty() {
            argv.push(BString::from(expanded));
        }
    }
}

fn push_pch_args(
    argv: &mut Vec<BString>,
    ide: &rcmake_resolve::ide::IdeFacts,
    source: &IdeSource,
    language: &[u8],
    gnu_frontend: bool,
) {
    let Some(pch) = ide
        .pch
        .iter()
        .find(|p| p.language.as_slice() == language && p.companion.is_some())
    else {
        return;
    };
    if source.skip_precompile_headers {
        return;
    }
    let header = string(&pch.header);
    let artifact = string(&pch.artifact);
    if gnu_frontend {
        argv.push(BString::from("-include"));
        argv.push(BString::from(header));
    } else {
        let create = pch.reuse_from.is_none() && source.path == pch.source;
        argv.push(BString::from(format!(
            "{}{}",
            if create { "/Yc" } else { "/Yu" },
            windows(&header)
        )));
        argv.push(BString::from(format!("/Fp{}", windows(&artifact))));
        argv.push(BString::from(format!("/FI{}", windows(&header))));
    }
}

fn is_gnu_frontend(model: &ProjectModel, dir: DirId, language: &[u8]) -> bool {
    let key = [b"CMAKE_", language, b"_COMPILER_FRONTEND_VARIANT"].concat();
    model
        .definition(dir, &key)
        .is_some_and(|val| val.eq_ignore_ascii_case(b"GNU"))
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
        .map_or_else(Vec::new, |val| list::split(val, list::Empty::Drop))
}
