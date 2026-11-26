//! Make entry points for project build subdirectories.

use std::collections::{BTreeMap, HashMap};

use rcmake_base::{path_lex::Lexical, truth};
use rcmake_model::{DirId, ProjectModel, TargetId};
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

use super::{
    BString, GenError, Host, MAKEFILE2, Path, Target, check_make_path, make_recipe_text, makefile,
    path, phony, quote_always, recursive_make, write_makefile,
};

pub(super) fn write(
    host: &dyn Host,
    build: &Path,
    model: &ProjectModel,
    result: &GenerationResult,
    targets: &[Target<'_>],
    (top, dispatch): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), GenError> {
    let root = path::from_native(build);
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
        let mut wrapper = makefile();
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
            check_make_path(&goal)?;
            let prereq = match name {
                b"all" => Some(BString::from("cmake_check_build_system")),
                b"preinstall" => Some(Lexical::new(relative).append(b"all")),
                _ => None,
            };
            phony(top, &goal, prereq.as_ref().map(|value| value.as_slice()));
            recursive_make(top, MAKEFILE2, &goal);
            phony(&mut wrapper, name, []);
            from_root(&mut wrapper, &root, b"Makefile", &goal)?;

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
            phony(dispatch, &goal, needs.iter().map(|value| value.as_slice()));
            goals.push(BString::from(name));
        }
        for &(_, _, target) in dir_targets {
            phony(&mut wrapper, target.name, []);
            from_root(&mut wrapper, &root, b"Makefile", target.name)?;
            let fast = [target.name, b"/fast"].concat();
            phony(&mut wrapper, &fast, []);
            from_root(&mut wrapper, &root, &target.file, &target.goal)?;
            goals.push(target.name.into());
        }
        phony(&mut wrapper, b"help", []);
        for goal in goals {
            wrapper.extend_from_slice(b"\t@printf '%s\\n' ");
            make_recipe_text(
                &mut wrapper,
                &quote_always(&[b"... ", goal.as_slice()].concat()),
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

fn from_root(out: &mut Vec<u8>, root: &[u8], file: &[u8], goal: &[u8]) -> Result<(), GenError> {
    out.extend_from_slice(b"\t$(SILENT)$(MAKE) -C ");
    make_recipe_text(out, &quote_always(root))?;
    out.extend_from_slice(b" -f ");
    make_recipe_text(out, &quote_always(file))?;
    out.push(b' ');
    make_recipe_text(out, &quote_always(goal))?;
    out.push(b'\n');
    Ok(())
}
