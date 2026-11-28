use rcmake_base::{BString, hash::HashAlgo, path_lex::Lexical, truth};
use rcmake_model::{DirId, ProjectModel, Target, TargetId};

use crate::lower::{
    artifacts::compile_pdb_path,
    definition, join, language_flags, msvc_debug_options, object_dir,
    shell::{direct, simple_args},
};
use crate::{CompileGroup, CustomStep, ResolveError, ResolvedTarget, usage::UsageResolver};

use super::{Plan, write_source};

pub(in crate::lower) struct Copy {
    pub(in crate::lower) destination: BString,
    pub(in crate::lower) step: Option<CustomStep>,
}

pub(in crate::lower) fn prepare(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    owner_id: TargetId,
    plan: &Plan,
    group: &CompileGroup,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<Copy>, ResolveError> {
    let Some(host) = usage.source_paths().map(|paths| paths.host) else {
        return Ok(None);
    };
    if !definition(model, target.dir, b"CMAKE_PCH_COPY_COMPILE_PDB")
        .is_some_and(|value| truth::is_on(value))
    {
        return Ok(None);
    }
    let Some(owner) = model.target(owner_id) else {
        return Ok(None);
    };
    let config = &resolved.config;
    let id = resolved.id;
    let root = &model.dir(DirId::ROOT).binary_dir;
    let from = compile_pdb_path(model, owner, Some(owner_id), true, config, usage)?;
    if owner_id == id {
        let full_from = join(root, &from);
        if let Some(parent) = rcmake_host::path::to_native(&full_from).parent() {
            let _ = host.create_dir_all(parent);
        }
        return Ok(Some(Copy {
            destination: from,
            step: None,
        }));
    }

    let extensions = debug_extensions(model, target, resolved, group)?;
    if extensions.is_empty() {
        return Ok(None);
    }
    let local = compile_pdb_path(model, target, Some(id), false, config, usage)?;
    let directory = if local.ends_with(b"/") {
        &local
    } else {
        Lexical::new(&local).parent()
    };
    let destination = join(directory, Lexical::new(&from).filename());
    if Lexical::new(&destination).equal(Lexical::new(&from)) {
        return Ok(Some(Copy {
            destination,
            step: None,
        }));
    }
    let id = HashAlgo::Sha256.hex_digest(&group.language);
    let dir = object_dir(model, target, config);
    let script = join(&dir, format!("copy-pch-{id}.cmake").as_bytes());
    let stamp = join(&dir, format!("copy-pch-{id}.stamp").as_bytes());
    let from = join(root, &from);
    let to = join(root, &destination);
    let to_dir = Lexical::new(&to).parent();
    let mut text = BString::from("cmake_minimum_required(VERSION 4.5)\nfile(MAKE_DIRECTORY ");
    quote(&mut text, to_dir);
    text.extend_from_slice(b")\nfile(LOCK ");
    quote(&mut text, &[to.as_slice(), b".lock"].concat());
    text.extend_from_slice(b" GUARD PROCESS TIMEOUT 30)\n");
    for extension in extensions {
        text.extend_from_slice(b"set(from ");
        quote(&mut text, &with_extension(&from, extension));
        text.extend_from_slice(b")\nset(to ");
        quote(&mut text, &with_extension(&to, extension));
        text.extend_from_slice(b")\nforeach(attempt RANGE 1 30)\n  file(COPY_FILE \"${from}\" \"${to}\" ONLY_IF_DIFFERENT RESULT result)\n  if(result STREQUAL \"0\")\n    break()\n  endif()\n  execute_process(COMMAND \"${CMAKE_COMMAND}\" -E sleep 1)\nendforeach()\nif(NOT result STREQUAL \"0\")\n  message(FATAL_ERROR \"Cannot copy ${from} to ${to}. ${result}\")\nendif()\n");
    }
    text.extend_from_slice(b"file(TOUCH ");
    quote(&mut text, &join(root, &stamp));
    text.extend_from_slice(b")\n");
    let absolute_script = join(root, &script);
    write_source(host, &absolute_script, &text)?;
    let program = definition(model, target.dir, b"CMAKE_COMMAND")
        .cloned()
        .unwrap_or_else(|| "cmake".into());
    let step = CustomStep {
        outputs: vec![stamp],
        depends: vec![plan.artifact.clone(), script],
        invocations: vec![direct(vec![program, "-P".into(), absolute_script], None)],
        comment: Some("Copy the compiler program database".into()),
        ..Default::default()
    };
    Ok(Some(Copy {
        destination,
        step: Some(step),
    }))
}

fn debug_extensions(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    group: &CompileGroup,
) -> Result<&'static [&'static [u8]], ResolveError> {
    const PDB_ONLY: &[&[u8]] = &[b".pdb"];
    const PDB_AND_IDB: &[&[u8]] = &[b".pdb", b".idb"];
    let flags = language_flags(model, target, &resolved.config, &group.language)
        .into_iter()
        .flat_map(|flags| simple_args(&flags).unwrap_or_default())
        .chain(msvc_debug_options(
            model,
            target,
            &resolved.config,
            &group.language,
            None,
        )?)
        .chain(group.flags.iter().flat_map(|option| {
            if let Some(shell) = option.value.strip_prefix(b"SHELL:") {
                simple_args(shell).unwrap_or_default()
            } else {
                vec![option.value.clone()]
            }
        }));
    Ok(flags
        .filter_map(|flag| match flag.as_slice() {
            b"/ZI" | b"-ZI" => Some(PDB_AND_IDB),
            b"/Zi" | b"-Zi" => Some(PDB_ONLY),
            b"/Z7" | b"-Z7" => Some(&[][..]),
            _ => None,
        })
        .next_back()
        .unwrap_or_default())
}

fn with_extension(path: &[u8], extension: &[u8]) -> BString {
    [path.strip_suffix(b".pdb").unwrap_or(path), extension]
        .concat()
        .into()
}

fn quote(text: &mut BString, value: &[u8]) {
    crate::lower::install::cmake_quote(text, value);
}
