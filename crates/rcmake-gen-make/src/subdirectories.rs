//! Make entry points for project build subdirectories.

use std::collections::BTreeMap;

use rcmake_base::{path_lex::Lexical, truth};
use rcmake_model::ProjectModel;
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
    for (id, dir) in model.dirs().skip(1) {
        let full = Lexical::new(&dir.binary_dir);
        if let Some(relative) = full
            .relative(Lexical::new(&root))
            .filter(|path| path != b".")
        {
            directories.insert(id, relative);
        }
    }

    let mut usage = UsageResolver::new(model);
    for (&id, relative) in &directories {
        let mut wrapper = makefile();
        let mut goals = Vec::new();
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
                for (child, path) in &directories {
                    let dir = model.dir(*child);
                    if dir.parent == Some(id) && (name == b"clean" || !dir.exclude_from_all) {
                        needs.push(Lexical::new(path).append(name));
                    }
                }
                for target in targets {
                    let Some(raw_id) = model.find_target(id, target.name) else {
                        continue;
                    };
                    let Some(raw) = model.target(raw_id).filter(|raw| raw.dir == id) else {
                        continue;
                    };
                    if name == b"clean" {
                        needs.push(target.clean_goal.clone());
                    } else {
                        let excluded = raw
                            .props
                            .get(b"EXCLUDE_FROM_ALL")
                            .map(|value| {
                                let handle = rcmake_genex::TargetHandle(raw_id.index() as u32);
                                let context = rcmake_genex::EvalContext {
                                    config: result
                                        .graphs
                                        .first()
                                        .map(|graph| graph.config.clone())
                                        .unwrap_or_default(),
                                    head: Some(handle),
                                    current: Some(handle),
                                    ..Default::default()
                                };
                                rcmake_genex::parse(value)
                                    .and_then(|ast| {
                                        rcmake_genex::evaluate(&ast, &context, &mut usage.host(id))
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
        for target in targets.iter().filter(|target| {
            model
                .find_target(id, target.name)
                .and_then(|id| model.target(id))
                .is_some_and(|target| target.dir == id)
        }) {
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
