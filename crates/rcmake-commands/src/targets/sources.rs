use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::Backtrace;
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{
    BT, FileSet, FileSetKind, ModelError, ProjectModel, TargetId, UsageField, UsageScope,
};

use super::{absolute, bytes, model_error, scope, source_dir, status, usage, usage_target};

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, mut rest)) = args.split_first() else {
        return Err(status("called with incorrect number of arguments"));
    };
    if rest.is_empty() {
        return Err(status("called with incorrect number of arguments"));
    }
    let id = usage_target(cx, &name.value, "sources")?;
    let dir = source_dir(cx);
    let bt = cx.backtrace();
    let model = cx.project();
    while let Some((first, tail)) = rest.split_first() {
        match (scope(bytes(first)), bytes(first), tail) {
            (Some(scope), _, [key, args @ ..]) if bytes(key) == b"FILE_SET" => {
                rest = add_file_set(model, id, args, Some(scope), &dir, &bt)?;
            }
            (_, b"FILE_SET", _) => {
                rest = add_file_set(model, id, tail, None, &dir, &bt)?;
            }
            (Some(scope), _, _) => {
                let end = tail
                    .iter()
                    .position(|arg| super::scope(bytes(arg)).is_some() || bytes(arg) == b"FILE_SET")
                    .unwrap_or(tail.len());
                let (items, next) = tail.split_at(end);
                let target = model
                    .target_mut(id)
                    .ok_or_else(|| model_error(ModelError::UnknownObject))?;
                usage::validate_scope(target, scope, true)?;
                let value = list::join(
                    items
                        .iter()
                        .map(bytes)
                        .filter(|item| !item.is_empty())
                        .map(|item| absolute(item, &dir)),
                );
                if !value.is_empty() {
                    if scope != UsageScope::Interface {
                        target.sources.push(BT::new(value.clone(), bt.clone()));
                    }
                    if scope != UsageScope::Private {
                        target
                            .usage
                            .push(UsageField::InterfaceSources, BT::new(value, bt.clone()));
                    }
                }
                rest = next;
            }
            _ => return Err(status("called with invalid arguments")),
        }
    }
    Ok(Flow::Normal)
}

#[derive(Default)]
struct FileSetArgs<'a> {
    kind: Option<&'a [u8]>,
    scope: Option<UsageScope>,
    bases: Vec<BT<BString>>,
    files: Vec<BT<BString>>,
}

fn add_file_set<'a>(
    model: &mut ProjectModel,
    id: TargetId,
    args: &'a [ExpandedArg],
    scope: Option<UsageScope>,
    dir: &[u8],
    bt: &Backtrace,
) -> Result<&'a [ExpandedArg], CmdError> {
    let Some((name, args)) = args.split_first() else {
        return Err(status("FILE_SET must be followed by a file set name"));
    };
    let (args, rest) = parse_file_set(args, scope, dir, bt)?;
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    let existing = target.file_sets.iter().copied().find(|&set| {
        model
            .file_set(set)
            .is_some_and(|set| set.name == name.value)
    });
    let existing_set = existing.and_then(|set| model.file_set(set));
    let scope = args
        .scope
        .ok_or_else(|| status("FILE_SET requires a scope"))?;
    usage::validate_scope(target, scope, true)?;
    if target.kind == rcmake_model::TargetKind::Utility {
        return Err(status("FILE_SETs may not be added to custom targets"));
    }
    let kind = file_set_kind(&name.value, args.kind, existing_set)?;
    if kind == FileSetKind::CxxModules && scope == UsageScope::Interface && !target.is_imported() {
        return Err(status(
            "File set TYPE CXX_MODULES may not have INTERFACE visibility",
        ));
    }
    let mut bases = args.bases;
    if let Some(set) = existing_set {
        if set.scope != scope {
            return Err(status(format!(
                "Scope {} for file set \"{}\" does not match original scope {}",
                scope_name(scope),
                set.name,
                scope_name(set.scope)
            )));
        }
        validate_bases(set.base_dirs.iter().chain(&bases))?;
    } else {
        if bases.is_empty() {
            bases.push(BT::new(dir.into(), bt.clone()));
        }
        validate_bases(bases.iter())?;
    }
    let include = (kind == FileSetKind::Headers).then(|| {
        list::join(
            bases
                .iter()
                .map(|base| [b"$<BUILD_INTERFACE:", base.value.as_slice(), b">"].concat()),
        )
    });
    if let Some(set) = existing.and_then(|set| model.file_set_mut(set)) {
        set.base_dirs.extend(bases);
        set.files.extend(args.files);
    } else {
        model
            .add_file_set(FileSet {
                target: id,
                name: name.value.clone(),
                kind,
                scope,
                base_dirs: bases,
                files: args.files,
                props: rcmake_model::PropertyMap::default(),
            })
            .map_err(model_error)?;
    }
    if let Some(include) = include.filter(|value| !value.is_empty()) {
        let target = model
            .target_mut(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        if scope != UsageScope::Interface {
            target.usage.push(
                UsageField::IncludeDirectories,
                BT::new(include.clone(), bt.clone()),
            );
        }
        if scope != UsageScope::Private {
            target.usage.push(
                UsageField::InterfaceIncludeDirectories,
                BT::new(include, bt.clone()),
            );
        }
    }
    Ok(rest)
}

fn parse_file_set<'a>(
    mut rest: &'a [ExpandedArg],
    scope: Option<UsageScope>,
    dir: &[u8],
    bt: &Backtrace,
) -> Result<(FileSetArgs<'a>, &'a [ExpandedArg]), CmdError> {
    let mut result = FileSetArgs {
        scope,
        ..FileSetArgs::default()
    };
    while let Some((key, tail)) = rest.split_first() {
        rest = match bytes(key) {
            b"TYPE" | b"SCOPE" => {
                let Some((value, tail)) = tail.split_first() else {
                    return Err(status(format!("{} must be followed by a value", key.value)));
                };
                if bytes(key) == b"TYPE" {
                    result.kind = Some(bytes(value));
                } else {
                    result.scope = Some(super::scope(bytes(value)).ok_or_else(|| {
                        status("FILE_SET SCOPE must be INTERFACE, PUBLIC, or PRIVATE")
                    })?);
                }
                tail
            }
            b"BASE_DIRS" | b"FILES" => {
                let end = tail
                    .iter()
                    .position(|arg| {
                        matches!(
                            bytes(arg),
                            b"TYPE" | b"SCOPE" | b"BASE_DIRS" | b"FILES" | b"FILE_SET"
                        ) || super::scope(bytes(arg)).is_some()
                    })
                    .unwrap_or(tail.len());
                let (values, next) = tail.split_at(end);
                let entries = values
                    .iter()
                    .map(bytes)
                    .flat_map(|value| {
                        if contains_genex(value) {
                            vec![BString::from(value)]
                        } else {
                            list::split(value, list::Empty::Drop)
                        }
                    })
                    .map(|value| BT::new(absolute(&value, dir), bt.clone()));
                if bytes(key) == b"BASE_DIRS" {
                    result.bases.extend(entries);
                } else {
                    result.files.extend(entries);
                }
                next
            }
            b"FILE_SET" => break,
            keyword if super::scope(keyword).is_some() => break,
            _ => return Err(status(format!("Unrecognized argument: \"{}\"", key.value))),
        };
    }
    Ok((result, rest))
}

fn file_set_kind(
    name: &BString,
    kind: Option<&[u8]>,
    existing: Option<&FileSet>,
) -> Result<FileSetKind, CmdError> {
    let default = FileSetKind::from_name(name);
    let kind = match kind {
        Some(value) => FileSetKind::from_name(value).ok_or_else(|| {
            status(format!(
                "File set TYPE \"{}\" is not supported by the project model",
                BString::from(value)
            ))
        })?,
        None => existing
            .map(|set| set.kind)
            .or(default)
            .ok_or_else(|| status("Must specify a TYPE when creating file set"))?,
    };
    if existing.is_some_and(|set| set.kind != kind) {
        return Err(status(format!(
            "Type \"{}\" for file set \"{name}\" does not match original type",
            kind.as_str()
        )));
    }
    if default != Some(kind)
        && !(name
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_'))
    {
        return Err(status(format!("File set name \"{name}\" is not valid")));
    }
    Ok(kind)
}

fn scope_name(scope: UsageScope) -> &'static str {
    match scope {
        UsageScope::Private => "PRIVATE",
        UsageScope::Public => "PUBLIC",
        UsageScope::Interface => "INTERFACE",
    }
}

fn contains_genex(value: &[u8]) -> bool {
    value.windows(2).any(|pair| pair == b"$<")
}

fn validate_bases<'a>(bases: impl Iterator<Item = &'a BT<BString>>) -> Result<(), CmdError> {
    // Evaluate paths with generator expressions when generation resolves their values.
    let mut paths: Vec<_> = bases
        .filter(|base| !contains_genex(&base.value))
        .map(|base| {
            let mut normalized = Lexical::new(&base.value).normalize();
            if !normalized.ends_with(b"/") {
                normalized.push(b'/');
            }
            (normalized, &base.value)
        })
        .collect();
    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    paths.dedup_by(|left, right| left.0 == right.0);
    for pair in paths.windows(2) {
        if let [(parent, first), (child, second)] = pair
            && child.starts_with(parent)
        {
            return Err(status(format!(
                "Base directories in file set cannot be subdirectories of each other:\n  {first}\n  {second}"
            )));
        }
    }
    Ok(())
}
