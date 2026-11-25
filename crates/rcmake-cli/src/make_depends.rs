use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, list, path_lex::Lexical, truth};
use rcmake_eval::{Interp, InterpConfig, Registry, RunMode};
use rcmake_host::{FileKind, Follow, Host, Metadata, ReadRange, Stream, WriteMode, path};
use rcmake_tool_e::make_depends::{self, FortranSource, FortranTarget, Source};

pub(crate) fn run(args: &[BString], host: Arc<dyn Host>) -> i32 {
    match scan(args, &host) {
        Ok(code) => code,
        Err(error) => {
            crate::emit_cli_error(host.as_ref(), error);
            2
        }
    }
}

fn scan(args: &[BString], host: &Arc<dyn Host>) -> Result<i32, String> {
    let (generator, home, source, root, build, info, tail) = match args {
        [generator, home, source, root, build, info, tail @ ..] => {
            (generator, home, source, root, build, info, tail)
        }
        [generator, root, source, info, ..] => {
            (generator, root, source, root, root, info, &[] as &[BString])
        }
        _ => return Ok(1),
    };
    let cwd = host.initial_cwd();
    let native = |value: &[u8]| cwd.join(path::to_native(value));
    let source_root = native(home);
    let root = native(root);
    let build = native(build);
    let source = native(source);
    let info = native(info);
    let Some(output_dir) = info.parent() else {
        return Ok(2);
    };
    if generator != "Unix Makefiles" {
        return Ok(
            if rcmake_model::GeneratorKind::from_name(generator).is_some() {
                0
            } else {
                1
            },
        );
    }
    let directory_info = build.join("CMakeFiles/CMakeDirectoryInformation.cmake");
    let mut interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: Arc::clone(host),
            mode: RunMode::Script,
            cmake_root: PathBuf::new(),
            cmake_command: host.current_exe().unwrap_or_default(),
            cwd,
            limits: Limits::default(),
            diag: rcmake_diag::DiagOptions::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    if interp.run_script(&info).exit_code != 0 {
        return Ok(2);
    }
    if host.metadata(&directory_info, Follow::Yes).is_ok()
        && interp.run_script(&directory_info).exit_code != 0
    {
        return Ok(2);
    }
    let target_name = host
        .env_get(b"VERBOSE")
        .is_some_and(|value| !truth::is_off(&value))
        .then(|| {
            tail.iter()
                .find(|arg| !arg.starts_with(b"--color="))
                .cloned()
                .or_else(|| {
                    output_dir
                        .file_stem()
                        .map(|name| path::from_native(Path::new(name)))
                })
                .unwrap_or_default()
        });
    let cx = interp.cx();
    let values = |name: &[u8]| list::split(&cx.var_or_empty(name), list::Empty::Drop);
    let optional = |name: &[u8]| cx.var_defined(name).then(|| cx.var_or_empty(name));
    let compiler_records = cx.var_or_empty(b"CMAKE_DEPENDS_DEPENDENCY_FILES");
    let languages = values(b"CMAKE_DEPENDS_LANGUAGES");
    let mut sources = Vec::new();
    let mut fortran = None;
    for language in &languages {
        if !matches!(
            language.as_slice(),
            b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"RC" | b"ASM" | b"Fortran"
        ) {
            continue;
        }
        let target_includes = format!("CMAKE_{language}_TARGET_INCLUDE_PATH");
        let (include_key, include_base) = if language.as_slice() == b"Fortran" {
            let key = if cx.var_defined(target_includes.as_bytes()) {
                target_includes.as_bytes()
            } else {
                b"CMAKE_Fortran_INCLUDE_PATH"
            };
            (key, &root)
        } else {
            (target_includes.as_bytes(), &build)
        };
        let include_dirs: Vec<_> = values(include_key)
            .iter()
            .map(|name| include_base.join(path::to_native(name)))
            .collect();
        let pairs = values(format!("CMAKE_DEPENDS_CHECK_{language}").as_bytes());
        let (pairs, remainder) = pairs.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(format!("Invalid dependency pairs for language {language}."));
        }
        let entries = pairs.iter().map(|[input, object]| Source {
            input: source.join(path::to_native(input)),
            object: root.join(path::to_native(object)),
            include_dirs: include_dirs.clone(),
        });
        if language.as_slice() == b"Fortran" {
            let formats = values(b"_RCMAKE_Fortran_SOURCE_FORMATS");
            let (formats, remainder) = formats.as_chunks::<2>();
            if !remainder.is_empty() {
                return Err("Invalid Fortran source format pairs.".into());
            }
            let formats: BTreeMap<_, _> = formats
                .iter()
                .map(|[path, format]| (path.as_slice(), format))
                .collect();
            fortran = Some(FortranTarget {
                build_dir: build.clone(),
                module_dir: optional(b"CMAKE_Fortran_TARGET_MODULE_DIR")
                    .filter(|dir| !dir.is_empty())
                    .map(|dir| path::to_native(&dir)),
                compiler_id: cx.var_or_empty(b"CMAKE_Fortran_COMPILER_ID"),
                submodule_sep: Some(cx.var_or_empty(b"CMAKE_Fortran_SUBMODULE_SEP")),
                submodule_ext: Some(cx.var_or_empty(b"CMAKE_Fortran_SUBMODULE_EXT")),
                defines: values(b"CMAKE_TARGET_DEFINITIONS_Fortran"),
                building_intrinsic_modules: [
                    b"CMAKE_Fortran_TARGET_BUILDING_INTRINSIC_MODULES".as_slice(),
                    b"CMAKE_Fortran_TARGET_BUILDING_INSTRINSIC_MODULES",
                ]
                .iter()
                .any(|key| !cx.var_or_empty(key).is_empty()),
                linked_info_files: values(b"CMAKE_Fortran_TARGET_LINKED_INFO_FILES")
                    .iter()
                    .map(|file| root.join(path::to_native(file)))
                    .collect(),
                sources: entries
                    .zip(pairs)
                    .map(|(source, [input, _])| FortranSource {
                        source,
                        format: formats.get(input.as_slice()).copied().cloned(),
                    })
                    .collect(),
            });
        } else {
            sources.extend(entries);
        }
    }
    let scan_implicit = !languages.is_empty() || compiler_records.is_empty();
    if scan_implicit && let Some(name) = &target_name {
        host.write_out(
            Stream::Stdout,
            format!("Scanning dependencies of target {name}\n").as_bytes(),
        );
    }
    let implicit_result = match &fortran {
        Some(target) => make_depends::write_with_fortran(
            host.as_ref(),
            generator,
            &root,
            output_dir,
            &sources,
            target,
        ),
        None if scan_implicit => {
            make_depends::write(host.as_ref(), generator, &root, output_dir, &sources)
        }
        None => Ok(()),
    };
    let compiler_result = if compiler_records.is_empty() {
        Ok(())
    } else {
        CompilerScan {
            source_root: &source_root,
            build_root: &root,
            build_dir: &build,
            output_dir,
            project_only: cx.var_is_on(b"CMAKE_DEPENDS_IN_PROJECT_ONLY"),
        }
        .update(host.as_ref(), &compiler_records, target_name.as_ref())
    };
    implicit_result.map_err(|error| error.to_string())?;
    compiler_result?;
    Ok(0)
}

fn register(registry: &mut Registry) {
    let mut commands = Registry::default();
    rcmake_commands::register_all(&mut commands);
    if let Some((set, flags)) = commands.get(b"set") {
        registry.add("set", set, flags);
    }
}

type CompilerDependencies = BTreeMap<BString, Arc<Vec<BString>>>;

struct CompilerScan<'a> {
    source_root: &'a Path,
    build_root: &'a Path,
    build_dir: &'a Path,
    output_dir: &'a Path,
    project_only: bool,
}

impl CompilerScan<'_> {
    fn update(
        &self,
        host: &dyn Host,
        records: &[u8],
        target_name: Option<&BString>,
    ) -> Result<(), String> {
        let records = list::split(records, list::Empty::Keep);
        let (records, remainder) = records.as_chunks::<4>();
        if !remainder.is_empty() {
            return Err("Invalid records in CMAKE_DEPENDS_DEPENDENCY_FILES.".into());
        }
        let internal_path = self.output_dir.join("compiler_depend.internal");
        let make_path = self.output_dir.join("compiler_depend.make");
        let internal_meta = regular_file(host, &internal_path)?;
        let make_exists = regular_file(host, &make_path)?.is_some();
        let mut changed = Vec::new();
        for [source, target, format, depfile] in records {
            if depfile.is_empty() {
                continue;
            }
            let path = self.build_root.join(path::to_native(depfile));
            if let Some(meta) = regular_file(host, &path)?
                && internal_meta
                    .as_ref()
                    .is_none_or(|old| meta.mtime >= old.mtime)
            {
                changed.push((source, target, format, path));
            }
        }
        if changed.is_empty() && (internal_meta.is_none() || make_exists) {
            return Ok(());
        }

        let limit = Limits::default().max_file_read;
        let mut remaining = limit;
        let mut dependencies = if internal_meta.is_some() {
            read_compiler_internal(&read_dependency_file(host, &internal_path, &mut remaining)?)
        } else {
            CompilerDependencies::new()
        };
        for (source, target, format, path) in changed {
            let contents = read_dependency_file(host, &path, &mut remaining)?;
            self.collect(&mut dependencies, source, target, format, &contents);
        }
        let (rules, internal) = self.format(&dependencies, limit)?;
        if let Some(name) = target_name {
            host.write_out(
                Stream::Stdout,
                format!("Consolidate compiler generated dependencies of target {name}\n")
                    .as_bytes(),
            );
        }
        rcmake_gen_common::write_if_changed(host, &make_path, &rules)
            .map_err(|error| error.to_string())?;
        host.write(&internal_path, &internal, WriteMode::Truncate)
            .map_err(|error| error.to_string())
    }

    fn collect(
        &self,
        dependencies: &mut CompilerDependencies,
        source: &BString,
        target: &BString,
        format: &[u8],
        contents: &[u8],
    ) {
        let mut inputs = match format {
            b"gcc" | b"custom" => {
                let Some(rules) = parse_compiler_depfile(contents) else {
                    return;
                };
                if format == b"custom" {
                    for rule in rules {
                        let inputs = Arc::new(self.depfile_inputs(rule.inputs));
                        for output in rule.outputs {
                            dependencies
                                .insert(full_path(self.build_dir, &output), Arc::clone(&inputs));
                        }
                    }
                    return;
                }
                let Some(rule) = rules.into_iter().next() else {
                    return;
                };
                if rule.inputs.is_empty() {
                    return;
                }
                let target_path = full_path(self.build_root, target);
                let source_path = full_path(self.build_root, source);
                let mut inputs = self.depfile_inputs(rule.inputs);
                inputs.retain(|input| input != &target_path && input != &source_path);
                inputs
            }
            b"msvc" => lines(contents)
                .filter(|line| !line.is_empty())
                .map(BString::from)
                .filter(|input| self.in_project(input))
                .collect(),
            _ => return,
        };
        if !source.is_empty() {
            inputs.insert(0, source.clone());
        }
        dependencies.insert(target.clone(), Arc::new(inputs));
    }

    fn depfile_inputs(&self, mut inputs: Vec<BString>) -> Vec<BString> {
        inputs.sort_unstable();
        inputs.dedup();
        inputs
            .iter()
            .map(|input| full_path(self.build_dir, input))
            .filter(|input| self.in_project(input))
            .collect()
    }

    fn in_project(&self, input: &[u8]) -> bool {
        if !self.project_only {
            return true;
        }
        let input = path::to_native(input);
        !input.is_absolute()
            || input.starts_with(self.source_root)
            || input.starts_with(self.build_root)
    }

    fn format(
        &self,
        dependencies: &CompilerDependencies,
        limit: u64,
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        let header = b"# CMAKE generated file: DO NOT EDIT!\n# Generated by \"Unix Makefiles\" Generator, CMake Version 4.5\n\n";
        let mut rules = header.to_vec();
        let mut internal = header.to_vec();
        let mut empty_rules = BTreeSet::new();
        for (target, inputs) in dependencies {
            let word = self.make_path(target);
            append_make_word(&mut rules, &word, true)?;
            rules.extend_from_slice(b": ");
            internal.extend_from_slice(target);
            internal.push(b'\n');
            for (index, input) in inputs.iter().enumerate() {
                let word = self.make_path(input);
                if index > 0 {
                    rules.extend_from_slice(b" \\\n  ");
                }
                append_make_word(&mut rules, &word, false)?;
                internal.push(b' ');
                internal.extend_from_slice(input);
                internal.push(b'\n');
                empty_rules.insert(word);
                check_output_size(&rules, &internal, limit)?;
            }
            rules.extend_from_slice(b"\n\n");
            internal.push(b'\n');
            check_output_size(&rules, &internal, limit)?;
        }
        for word in empty_rules {
            rules.push(b'\n');
            append_make_word(&mut rules, &word, true)?;
            rules.extend_from_slice(b":\n");
            check_output_size(&rules, &internal, limit)?;
        }
        Ok((rules, internal))
    }

    fn make_path(&self, name: &[u8]) -> BString {
        let file = path::to_native(name);
        match file.strip_prefix(self.build_root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => path::from_native_cmake(relative),
            Err(_) => path::from_native_cmake(&file),
        }
    }
}

fn full_path(dir: &Path, name: &[u8]) -> BString {
    let dir = path::from_native_cmake(dir);
    let name = path::from_native_cmake(&path::to_native(name));
    Lexical::new(&Lexical::new(&dir).append(&name)).normalize()
}

fn append_make_word(out: &mut Vec<u8>, word: &[u8], target: bool) -> Result<(), String> {
    if word.is_empty() {
        return Err("A compiler dependency path is empty.".into());
    }
    for &byte in word {
        match byte {
            b'\r' | b'\n' | 0 | b';' | b'|' => {
                return Err("Invalid compiler dependency path.".into());
            }
            b'=' => {
                out.extend_from_slice(b"$(EQUALS)");
                continue;
            }
            b' ' | b'\t' | b'#' | b':' | b'\\' => out.push(b'\\'),
            b'%' if target => out.push(b'\\'),
            b'*' | b'?' | b'[' if !target => out.push(b'\\'),
            b'$' => out.push(b'$'),
            _ => {}
        }
        out.push(byte);
    }
    Ok(())
}

fn check_output_size(rules: &[u8], internal: &[u8], limit: u64) -> Result<(), String> {
    if rules.len() as u64 + internal.len() as u64 > limit {
        return Err("The compiler dependency output exceeds the size limit.".into());
    }
    Ok(())
}

fn regular_file(host: &dyn Host, path: &Path) -> Result<Option<Metadata>, String> {
    match host.metadata(path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => Ok(Some(meta)),
        Ok(_) => Err(format!("{} is not a regular file.", path.display())),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot read {}. {error}", path.display())),
    }
}

fn read_dependency_file(
    host: &dyn Host,
    path: &Path,
    remaining: &mut u64,
) -> Result<Vec<u8>, String> {
    let contents = host
        .read(path, ReadRange::All { limit: *remaining })
        .map_err(|error| format!("Cannot read {}. {error}", path.display()))?;
    *remaining -= contents.len() as u64;
    Ok(contents)
}

fn lines(contents: &[u8]) -> impl Iterator<Item = &[u8]> {
    contents
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").map_or(line, |line| line))
}

fn read_compiler_internal(contents: &[u8]) -> CompilerDependencies {
    let mut dependencies = CompilerDependencies::new();
    let mut target = None;
    for line in lines(contents).filter(|line| !line.is_empty() && !line.starts_with(b"#")) {
        if let Some(input) = line.strip_prefix(b" ") {
            if let Some(target) = target
                && let Some(inputs) = dependencies.get_mut(target)
            {
                Arc::make_mut(inputs).push(input.into());
            }
        } else {
            target = Some(line);
            dependencies.entry(line.into()).or_default();
        }
    }
    dependencies
}

struct DepfileRule {
    outputs: Vec<BString>,
    inputs: Vec<BString>,
}

fn parse_compiler_depfile(contents: &[u8]) -> Option<Vec<DepfileRule>> {
    let mut rules = Vec::new();
    let mut words = Vec::new();
    let mut word = BString::default();
    let mut outputs = None;
    let mut bytes = contents.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' if matches!(bytes.peek(), Some(b'\r' | b'\n')) => {
                if bytes.next() == Some(b'\r') {
                    bytes.next_if_eq(&b'\n');
                }
                finish_depfile_word(&mut words, &mut word);
            }
            b'\\' if matches!(bytes.peek(), Some(b' ' | b'\t' | b'#' | b':')) => {
                word.extend(bytes.next());
            }
            b'$' => {
                bytes.next_if_eq(&b'$');
                word.push(b'$');
            }
            b':' if outputs.is_none()
                && bytes.peek().copied().is_none_or(|next| {
                    next.is_ascii_whitespace()
                        || (next == b'\\' && matches!(bytes.clone().nth(1), Some(b'\r' | b'\n')))
                }) =>
            {
                finish_depfile_word(&mut words, &mut word);
                outputs = Some(std::mem::take(&mut words));
            }
            b' ' | b'\t' => finish_depfile_word(&mut words, &mut word),
            b'\n' | b'\r' => {
                finish_depfile_word(&mut words, &mut word);
                finish_depfile_rule(&mut rules, outputs.take(), &mut words)?;
            }
            _ => word.push(byte),
        }
    }
    finish_depfile_word(&mut words, &mut word);
    finish_depfile_rule(&mut rules, outputs, &mut words)?;
    Some(rules)
}

fn finish_depfile_word(words: &mut Vec<BString>, word: &mut BString) {
    if !word.is_empty() {
        words.push(std::mem::take(word));
    }
}

fn finish_depfile_rule(
    rules: &mut Vec<DepfileRule>,
    outputs: Option<Vec<BString>>,
    inputs: &mut Vec<BString>,
) -> Option<()> {
    if let Some(outputs) = outputs.filter(|outputs| !outputs.is_empty()) {
        rules.push(DepfileRule {
            outputs,
            inputs: std::mem::take(inputs),
        });
    } else if !inputs.is_empty() {
        return None;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use rcmake_host::{HostResult, PureHost};

    use super::*;

    #[test]
    fn compiler_scan_escapes_equals_in_make_paths() -> Result<(), String> {
        let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(work.to_path_buf(), work.join("rcmake"));
        let scan = CompilerScan {
            source_root: work,
            build_root: work,
            build_dir: work,
            output_dir: work,
            project_only: false,
        };
        host.seed_file(&work.join("obj.o.d"), b"obj.o: dep=abc.h\n")
            .map_err(|error| error.to_string())?;
        scan.update(&host, b"main.c;obj.o;gcc;obj.o.d", None)?;
        let rules =
            read_output(&host, work, "compiler_depend.make").map_err(|error| error.to_string())?;
        assert!(rules.contains("  dep$(EQUALS)abc.h\n"));
        assert!(rules.contains("\ndep$(EQUALS)abc.h:\n"));
        let internal = read_output(&host, work, "compiler_depend.internal")
            .map_err(|error| error.to_string())?;
        assert!(internal.contains("/dep=abc.h\n"));
        Ok(())
    }

    #[test]
    fn mixed_target_refreshes_both_dependency_outputs() -> Result<(), Box<dyn std::error::Error>> {
        let work = if cfg!(windows) { "C:/work" } else { "/work" };
        let dir = Path::new(work);
        let host = Arc::new(PureHost::new(dir.to_path_buf(), dir.join("rcmake")));
        let output = dir.join("build/sub/CMakeFiles/mixed.dir");
        host.seed_file(
            &dir.join("src/child.f"),
            b"#ifdef USE_BUILTINS\nsubmodule (parent) child\n  include 'defines.inc'\nend submodule child\n#else\nmodule wrong\n#endif\n",
        )?;
        host.seed_file(
            &dir.join("src/builtins.f90"),
            b"module builtins\nend module builtins\n",
        )?;
        host.seed_file(
            &dir.join("build/includes/defines.inc"),
            b"use, intrinsic :: builtins\n",
        )?;
        host.seed_file(
            &dir.join("build/sub/CMakeFiles/CMakeDirectoryInformation.cmake"),
            b"set(CMAKE_Fortran_INCLUDE_PATH includes)\n",
        )?;
        host.seed_file(
            &output.join("DependInfo.cmake"),
            format!(
                r#"set(CMAKE_DEPENDS_LANGUAGES Fortran)
set(CMAKE_DEPENDS_CHECK_Fortran
  "{work}/src/child.f" "sub/CMakeFiles/mixed.dir/child.o"
  "{work}/src/builtins.f90" "sub/CMakeFiles/mixed.dir/builtins.o")
set(_RCMAKE_Fortran_SOURCE_FORMATS "{work}/src/child.f" FREE)
set(CMAKE_TARGET_DEFINITIONS_Fortran USE_BUILTINS=1)
set(CMAKE_Fortran_COMPILER_ID GNU)
set(CMAKE_Fortran_TARGET_BUILDING_INSTRINSIC_MODULES OFF)
set(CMAKE_DEPENDS_DEPENDENCY_FILES
  "{work}/src/main.c" "sub/CMakeFiles/mixed.dir/main.o" "gcc"
  "sub/CMakeFiles/mixed.dir/main.o.d")
"#,
            )
            .as_bytes(),
        )?;
        host.seed_file(&output.join("compiler_depend.internal"), b"old.o\n old.h\n")?;
        host.seed_file(&output.join("compiler_depend.make"), b"old.o: old.h\n")?;
        host.seed_file(
            &output.join("main.o.d"),
            b"ignored.o: generated\\ file\\#$$.h \\\n ../other.h\n",
        )?;
        let args = [
            "Unix Makefiles".into(),
            format!("{work}/src").into(),
            format!("{work}/src").into(),
            format!("{work}/build").into(),
            format!("{work}/build/sub").into(),
            path::from_native_cmake(&output.join("DependInfo.cmake")),
        ];
        let cli_host: Arc<dyn Host> = host.clone();
        assert_eq!(scan(&args, &cli_host)?, 0);
        let compiler_rules = read_output(cli_host.as_ref(), &output, "compiler_depend.make")?;
        assert!(compiler_rules.contains("sub/CMakeFiles/mixed.dir/main.o: "));
        assert!(compiler_rules.contains("sub/generated\\ file\\#$$.h:\n"));
        assert!(compiler_rules.contains("\nother.h:\n"));
        let compiler_internal =
            read_output(cli_host.as_ref(), &output, "compiler_depend.internal")?;
        assert!(compiler_internal.contains(&format!("\n {work}/src/main.c\n")));
        assert!(compiler_internal.contains(&format!("\n {work}/build/sub/generated file#$.h\n")));
        let fortran_rules = read_output(cli_host.as_ref(), &output, "depend.make")?;
        assert!(fortran_rules.contains("child.o: sub/CMakeFiles/mixed.dir/builtins.mod.stamp"));
        let fortran_internal = read_output(cli_host.as_ref(), &output, "depend.internal")?;
        assert!(fortran_internal.contains("build/includes/defines.inc"));
        let modules = read_output(cli_host.as_ref(), &output, "fortran.internal")?;
        assert!(modules.contains("\n child.mod\n"));
        assert!(!modules.contains("wrong.mod"));
        Ok(())
    }

    #[test]
    fn compiler_scan_updates_changed_records_and_keeps_cached_records() -> Result<(), String> {
        let work = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(work.to_path_buf(), work.join("rcmake"));
        let build = work.join("build");
        let output_dir = build.join("CMakeFiles/mixed.dir");
        let scan = CompilerScan {
            source_root: work,
            build_root: &build,
            build_dir: &build,
            output_dir: &output_dir,
            project_only: false,
        };
        let records = b"main.c;main.o;gcc;main.o.d;;one;custom;custom.d";
        host.seed_file(&output_dir.join("DependInfo.cmake"), b"")
            .map_err(|error| error.to_string())?;
        scan.update(&host, records, None)?;
        assert!(regular_file(&host, &output_dir.join("compiler_depend.internal"))?.is_none());
        let write = |name: &str, bytes: &[u8]| {
            host.seed_file(&build.join(name), bytes)
                .map_err(|error| error.to_string())
        };
        write("main.o.d", b"main.o: main.c main.h\n")?;
        write("custom.d", b"one two: old.h\n")?;
        scan.update(&host, records, None)?;
        host.remove_file(&build.join("main.o.d"))
            .map_err(|error| error.to_string())?;
        write("custom.d", b"one two: new.h\n")?;
        scan.update(&host, records, None)?;
        let output = host
            .read(
                &output_dir.join("compiler_depend.make"),
                ReadRange::All { limit: 4096 },
            )
            .map_err(|error| error.to_string())?;
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("one: new.h\n"));
        assert!(output.contains("two: new.h\n"));
        assert!(output.contains("main.o: main.c "));
        assert!(output.contains("main.h:\n"));
        assert!(!output.contains("old.h"));
        Ok(())
    }

    fn read_output(host: &dyn Host, output: &Path, name: &str) -> HostResult<String> {
        let data = host.read(&output.join(name), ReadRange::All { limit: 4096 })?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }
}
