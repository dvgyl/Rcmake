//! Make entry points for project build subdirectories.

use std::collections::{BTreeMap, HashMap};

use rcmake_base::{path_lex::Lexical, truth};
use rcmake_model::{DirId, ProjectModel, TargetId};
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

use super::{
    BString, GenError, Host, MAKEFILE2, MakeDialect, Path, Target, check_make_path_for,
    make_recipe_text_for, makefile_header, path, phony_for, quote_arg, recursive_make_for,
    write_makefile,
};

pub(super) struct Context<'a, 't> {
    pub(super) host: &'a dyn Host,
    pub(super) build: &'a Path,
    pub(super) model: &'a ProjectModel,
    pub(super) result: &'a GenerationResult,
    pub(super) targets: &'a [Target<'t>],
    pub(super) cmake: &'a [u8],
    pub(super) dialect: MakeDialect,
}

pub(super) fn write(
    ctx: Context<'_, '_>,
    top: &mut Vec<u8>,
    dispatch: &mut Vec<u8>,
) -> Result<(), GenError> {
    let Context {
        host,
        build,
        model,
        result,
        targets,
        cmake,
        dialect,
    } = ctx;
    let root = path::from_native_cmake(build);
    let mut directories = BTreeMap::new();
    let mut children_by_dir: HashMap<DirId, Vec<DirId>> = HashMap::new();
    for (id, dir) in model.dirs().skip(1) {
        let full = Lexical::new(&dir.binary_dir);
        if let Some(relative) = full
            .relative(Lexical::new(&root))
            .filter(|path| path != b".")
        {
            directories.insert(id, relative);
            if let Some(parent) = dir.parent {
                children_by_dir.entry(parent).or_default().push(id);
            }
        }
    }
    if directories.is_empty() {
        return Ok(());
    }

    let mut raw_by_name: HashMap<&[u8], (TargetId, &rcmake_model::Target)> = HashMap::new();
    for (raw_id, raw) in model.targets() {
        if !raw.is_imported() {
            raw_by_name
                .entry(raw.name.as_slice())
                .or_insert((raw_id, raw));
        }
    }
    let mut targets_by_dir: HashMap<DirId, Vec<(TargetId, &rcmake_model::Target, &Target<'_>)>> =
        HashMap::new();
    for target in targets {
        if let Some(&(raw_id, raw)) = raw_by_name.get(target.name)
            && directories.contains_key(&raw.dir)
        {
            targets_by_dir
                .entry(raw.dir)
                .or_default()
                .push((raw_id, raw, target));
        }
    }

    let mut usage: Option<UsageResolver<'_>> = None;
    let first_config = result
        .graphs
        .first()
        .map(|graph| graph.config.clone())
        .unwrap_or_default();
    for (&id, relative) in &directories {
        let mut wrapper = makefile_header(dialect, Some(cmake))?;
        let mut goals = Vec::new();
        let dir_children = children_by_dir
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let dir_targets = targets_by_dir
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for name in [b"all".as_slice(), b"clean", b"preinstall"] {
            let goal = Lexical::new(relative).append(name);
            check_make_path_for(dialect, &goal)?;
            let prereq = match name {
                b"all" => Some(BString::from("cmake_check_build_system")),
                b"preinstall" => Some(Lexical::new(relative).append(b"all")),
                _ => None,
            };
            phony_for(
                dialect,
                top,
                &goal,
                prereq.as_ref().map(|value| value.as_slice()),
            );
            recursive_make_for(dialect, top, MAKEFILE2, &goal);
            phony_for(dialect, &mut wrapper, name, []);
            from_root(dialect, &mut wrapper, &root, b"Makefile", &goal)?;

            let mut needs = Vec::new();
            if name != b"preinstall" {
                for &child in dir_children {
                    let dir = model.dir(child);
                    if let Some(path) = directories.get(&child)
                        && (name == b"clean" || !dir.exclude_from_all)
                    {
                        needs.push(Lexical::new(path).append(name));
                    }
                }
                for &(raw_id, raw, target) in dir_targets {
                    if name == b"clean" {
                        needs.push(target.clean_goal.clone());
                    } else {
                        let excluded = raw
                            .props
                            .get(b"EXCLUDE_FROM_ALL")
                            .map(|value| {
                                if rcmake_genex::find_genex(value).is_none() {
                                    return Ok(truth::is_on(value));
                                }
                                let handle = rcmake_genex::TargetHandle(raw_id.index() as u32);
                                let context = rcmake_genex::EvalContext {
                                    config: first_config.clone(),
                                    head: Some(handle),
                                    current: Some(handle),
                                    ..Default::default()
                                };
                                let resolver =
                                    usage.get_or_insert_with(|| UsageResolver::new(model));
                                rcmake_genex::parse(value)
                                    .and_then(|ast| {
                                        rcmake_genex::evaluate(
                                            &ast,
                                            &context,
                                            &mut resolver.host(id),
                                        )
                                    })
                                    .map(|(value, _)| truth::is_on(&value))
                                    .map_err(|error| GenError::Invalid(error.to_string().into()))
                            })
                            .transpose()?
                            .unwrap_or(false);
                        if !excluded {
                            needs.push(target.goal.clone());
                        }
                    }
                }
            }
            phony_for(
                dialect,
                dispatch,
                &goal,
                needs.iter().map(|value| value.as_slice()),
            );
            goals.push(BString::from(name));
        }
        for &(_, _, target) in dir_targets {
            phony_for(dialect, &mut wrapper, target.name, []);
            from_root(dialect, &mut wrapper, &root, b"Makefile", target.name)?;
            let fast = [target.name, b"/fast"].concat();
            phony_for(dialect, &mut wrapper, &fast, []);
            from_root(dialect, &mut wrapper, &root, &target.file, &target.goal)?;
            goals.push(target.name.into());
        }
        phony_for(dialect, &mut wrapper, b"help", []);
        for goal in goals {
            if dialect.is_nmake() {
                wrapper.extend_from_slice(b"\t@$(CMAKE_COMMAND) -E echo ");
            } else {
                wrapper.extend_from_slice(b"\t@printf '%s\\n' ");
            }
            make_recipe_text_for(
                dialect,
                &mut wrapper,
                &quote_arg(dialect, &[b"... ", goal.as_slice()].concat()),
            )?;
            wrapper.push(b'\n');
        }
        write_makefile(
            host,
            build,
            &Lexical::new(relative).append(b"Makefile"),
            &wrapper,
        )?;
    }
    Ok(())
}

fn from_root(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    root: &[u8],
    file: &[u8],
    goal: &[u8],
) -> Result<(), GenError> {
    if dialect.is_nmake() {
        out.extend_from_slice(b"\t$(SILENT)cd /d ");
        make_recipe_text_for(dialect, out, &quote_arg(dialect, root))?;
        out.extend_from_slice(b" && $(MAKE) $(MAKE_SILENT) -f ");
        make_recipe_text_for(dialect, out, &quote_arg(dialect, file))?;
        out.extend_from_slice(b" -$(MAKEFLAGS) VERBOSE=$(VERBOSE) ");
    } else {
        out.extend_from_slice(b"\t$(SILENT)$(MAKE) -C ");
        make_recipe_text_for(dialect, out, &quote_arg(dialect, root))?;
        out.extend_from_slice(b" -f ");
        make_recipe_text_for(dialect, out, &quote_arg(dialect, file))?;
        out.push(b' ');
    }
    make_recipe_text_for(dialect, out, &quote_arg(dialect, goal))?;
    out.push(b'\n');
    Ok(())
}
