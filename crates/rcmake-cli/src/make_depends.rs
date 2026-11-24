use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::{BString, Limits, list, truth};
use rcmake_eval::{Interp, InterpConfig, Registry, RunMode};
use rcmake_host::{Follow, Host, Stream, path};
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
    let (generator, source, root, build, info, tail) = match args {
        [generator, _, source, root, build, info, tail @ ..] => {
            (generator, source, root, build, info, tail)
        }
        [generator, root, source, info, ..] => (generator, source, root, root, info, &[][..]),
        _ => return Ok(1),
    };
    let cwd = host.initial_cwd();
    let native = |value: &[u8]| cwd.join(path::to_native(value));
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
    if host
        .env_get(b"VERBOSE")
        .is_some_and(|value| !truth::is_off(&value))
    {
        let name = tail
            .iter()
            .find(|arg| !arg.starts_with(b"--color="))
            .cloned()
            .or_else(|| {
                output_dir
                    .file_stem()
                    .map(|name| path::from_native(std::path::Path::new(name)))
            })
            .unwrap_or_default();
        host.write_out(
            Stream::Stdout,
            format!("Scanning dependencies of target {name}\n").as_bytes(),
        );
    }
    let cx = interp.cx();
    let values = |name: &[u8]| list::split(&cx.var_or_empty(name), list::Empty::Drop);
    let optional = |name: &[u8]| cx.var_defined(name).then(|| cx.var_or_empty(name));
    let mut sources = Vec::new();
    let mut fortran = None;
    for language in values(b"CMAKE_DEPENDS_LANGUAGES") {
        if !matches!(
            language.as_slice(),
            b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"RC" | b"ASM" | b"Fortran"
        ) {
            continue;
        }
        let include_dirs: Vec<_> =
            values(format!("CMAKE_{language}_TARGET_INCLUDE_PATH").as_bytes())
                .iter()
                .map(|name| build.join(path::to_native(name)))
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
        if language == "Fortran" {
            fortran = Some(FortranTarget {
                build_dir: build.clone(),
                module_dir: optional(b"CMAKE_Fortran_TARGET_MODULE_DIR")
                    .filter(|dir| !dir.is_empty())
                    .map(|dir| path::to_native(&dir)),
                compiler_id: cx.var_or_empty(b"CMAKE_Fortran_COMPILER_ID"),
                submodule_sep: optional(b"CMAKE_Fortran_SUBMODULE_SEP"),
                submodule_ext: optional(b"CMAKE_Fortran_SUBMODULE_EXT"),
                defines: values(b"CMAKE_TARGET_DEFINITIONS_Fortran"),
                building_intrinsic_modules: cx
                    .var_is_on(b"CMAKE_Fortran_TARGET_BUILDING_INTRINSIC_MODULES"),
                linked_info_files: values(b"CMAKE_Fortran_TARGET_LINKED_INFO_FILES")
                    .iter()
                    .map(|file| root.join(path::to_native(file)))
                    .collect(),
                sources: entries
                    .map(|source| FortranSource {
                        source,
                        format: None,
                    })
                    .collect(),
            });
        } else {
            sources.extend(entries);
        }
    }
    match &fortran {
        Some(target) => make_depends::write_with_fortran(
            host.as_ref(),
            generator,
            &root,
            output_dir,
            &sources,
            target,
        ),
        None => make_depends::write(host.as_ref(), generator, &root, output_dir, &sources),
    }
    .map_err(|error| error.to_string())?;
    Ok(0)
}

fn register(registry: &mut Registry) {
    let mut commands = Registry::default();
    rcmake_commands::register_all(&mut commands);
    if let Some((set, flags)) = commands.get(b"set") {
        registry.add("set", set, flags);
    }
}
