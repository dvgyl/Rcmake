use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{Backtrace, DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{
    BT, FileSet, FileSetId, FileSetKind, ModelError, ProjectModel, TargetId, TargetKind,
    UsageField, UsageScope,
};

use super::{absolute, bytes, model_error, properties, scope, source_dir, usage, usage_target};

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, mut rest)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    if rest.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    let id = usage_target(cx, &name.value, "sources")?;
    let dir = source_dir(cx);
    let bt = cx.backtrace();
    while let Some((first, tail)) = rest.split_first() {
        match (scope(bytes(first)), tail) {
            (Some(scope), [key, args @ ..]) if bytes(key) == b"FILE_SET" => {
                rest = add_file_set(cx, id, args, scope, &dir)?;
                while let Some((_, args)) = rest
                    .split_first()
                    .filter(|(key, _)| bytes(key) == b"FILE_SET")
                {
                    rest = add_file_set(cx, id, args, scope, &dir)?;
                }
            }
            (Some(scope), _) => {
                let end = tail
                    .iter()
                    .position(|arg| super::scope(bytes(arg)).is_some())
                    .unwrap_or(tail.len());
                let (items, next) = tail.split_at(end);
                let model = cx.project();
                let target_dir = model
                    .target(id)
                    .map(|target| model.dir(target.dir).source_dir.clone())
                    .ok_or_else(|| model_error(ModelError::UnknownObject))?;
                let different_dir = target_dir != dir;
                let policy = PolicyId::new(76);
                let status = cx.policy(policy);
                let items = || items.iter().map(bytes).filter(|item| !item.is_empty());
                let needs_conversion = different_dir || scope != UsageScope::Private;
                if needs_conversion
                    && status == PolicyStatus::Warn
                    && items()
                        .any(|item| !item.starts_with(b"$<") && !Lexical::new(item).absolute())
                {
                    cx.issue_policy_warning(policy, b"", b"");
                }
                let target = cx
                    .project()
                    .target_mut(id)
                    .ok_or_else(|| model_error(ModelError::UnknownObject))?;
                usage::validate_scope(target, scope, true)?;
                let value = |convert| {
                    list::join(items().map(|item| {
                        if convert && status == PolicyStatus::New {
                            absolute(item, &dir)
                        } else {
                            BString::from(item)
                        }
                    }))
                };
                if scope != UsageScope::Interface {
                    let direct = value(different_dir);
                    if !direct.is_empty() {
                        target.sources.push(BT::new(direct, bt.clone()));
                    }
                }
                if scope != UsageScope::Private {
                    let interface = value(true);
                    if !interface.is_empty() {
                        target
                            .usage
                            .push(UsageField::InterfaceSources, BT::new(interface, bt.clone()));
                    }
                }
                rest = next;
            }
            _ => return Err(CmdError::status("called with invalid arguments")),
        }
    }
    crate::dirs::source_groups::apply_to_target(cx, id)?;
    Ok(Flow::Normal)
}

#[derive(Default)]
struct FileSetArgs<'a> {
    kind: Option<&'a [u8]>,
    bases: Vec<BT<BString>>,
    files: Vec<BT<BString>>,
}

const SYCL_GATE: &[u8] = b"c0d1fb10-2ece-420e-9d29-7d7f2b300f25";
const CMP0211: PolicyId = PolicyId::new(211);

fn add_file_set<'a>(
    cx: &mut Cx<'_>,
    id: TargetId,
    args: &'a [ExpandedArg],
    scope: UsageScope,
    dir: &[u8],
) -> Result<&'a [ExpandedArg], CmdError> {
    let Some((name, args)) = args
        .split_first()
        .filter(|(name, _)| !name.value.is_empty())
    else {
        return Err(CmdError::status("FILE_SET must not be empty."));
    };
    let bt = cx.backtrace();
    let (args, rest) = parse_file_set(args, dir, &bt)?;
    let directory = cx.current_dir_id();
    let model = cx.project();
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    usage::validate_scope(target, scope, true)?;
    let existing = find_file_set(model, id, &name.value);
    let previous = existing.and_then(|id| model.file_set(id));
    let custom = args
        .kind
        .filter(|value| model.custom_rule(directory, value).is_some())
        .map(BString::from)
        .or_else(|| {
            args.kind
                .is_none()
                .then(|| previous?.props.get(b"_RCMAKE_RULE").cloned())
                .flatten()
        });
    if target.kind == TargetKind::Utility && custom.is_none() {
        return Err(CmdError::status(
            "FILE_SETs of type \"HEADERS\", \"SOURCES\", \"SYCL\", \"SYCL_HEADERS\", \"CXX_MODULES\" may not be added to custom targets.",
        ));
    }
    let (kind, sycl) = file_set_kind(
        &name.value,
        custom.as_ref().map(|_| b"SOURCES".as_slice()).or(args.kind),
        previous,
    )?;
    if kind == FileSetKind::Headers
        && target
            .props
            .get(b"FRAMEWORK")
            .is_some_and(|value| rcmake_base::truth::is_on(value))
    {
        return Err(CmdError::status(
            "FILE_SETs, of type \"HEADERS\", may not be added to FRAMEWORK targets.",
        ));
    }
    if kind == FileSetKind::CxxModules && scope == UsageScope::Interface && !target.is_imported() {
        return Err(CmdError::status(
            "File set TYPE \"CXX_MODULES\" may not have \"INTERFACE\" scope.",
        ));
    }
    if kind != FileSetKind::Headers
        && scope != UsageScope::Interface
        && target.kind == TargetKind::InterfaceLibrary
    {
        return Err(CmdError::status(format!(
            "File set TYPE \"{}\" may not have \"PUBLIC\" or \"PRIVATE\" scope on INTERFACE libraries.",
            kind.as_str()
        )));
    }
    let mut set = if let Some(previous) = previous {
        if previous.scope != scope {
            return Err(CmdError::status(format!(
                "Scope {} for file set \"{}\" does not match original scope {}.",
                scope_name(scope),
                previous.name,
                scope_name(previous.scope)
            )));
        }
        previous.clone()
    } else {
        FileSet {
            target: id,
            name: name.value.clone(),
            kind,
            scope,
            base_dirs: Vec::new(),
            files: Vec::new(),
            props: Default::default(),
        }
    };
    let selects_rule = custom.is_some() && (args.kind.is_some() || existing.is_none());
    if let Some(rule) = custom {
        set.props.set("_RCMAKE_RULE".into(), rule);
    }
    if let Some(alias) = sycl {
        require_sycl(cx, format!("File set TYPE \"{}\"", BString::from(alias)))?;
        set.props.set("LANGUAGE".into(), "SYCL".into());
    }
    let first_new_base = set.base_dirs.len();
    let mut bases = args.bases;
    if existing.is_none() && bases.is_empty() {
        bases.push(BT::new(dir.into(), bt.clone()));
    }
    set.base_dirs.extend(bases);
    let first_new_file = set.files.len();
    set.files.extend(args.files);
    validate_file_set_paths(cx.project(), &set)?;
    validate_file_set_membership(cx, &set, first_new_file)?;
    let model = cx.project();
    let target = model
        .target_mut(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if kind == FileSetKind::Headers {
        let include = list::join(
            set.base_dirs
                .iter()
                .skip(first_new_base)
                .map(|base| [b"$<BUILD_INTERFACE:", base.value.as_slice(), b">"].concat()),
        );
        if !include.is_empty() {
            if scope != UsageScope::Interface {
                target.usage.push(
                    UsageField::IncludeDirectories,
                    BT::new(include.clone(), bt.clone()),
                );
            }
            if scope != UsageScope::Private {
                target.usage.push(
                    UsageField::InterfaceIncludeDirectories,
                    BT::new(include, bt),
                );
            }
        }
    }
    let set_id = if let Some(id) = existing {
        *model
            .file_set_mut(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))? = set;
        id
    } else {
        model.add_file_set(set).map_err(model_error)?
    };
    if selects_rule {
        model.set_file_set_rule_dir(set_id, directory);
    }
    properties::store_file_set_properties(model, set_id);
    Ok(rest)
}

fn parse_file_set<'a>(
    mut rest: &'a [ExpandedArg],
    dir: &[u8],
    bt: &Backtrace,
) -> Result<(FileSetArgs<'a>, &'a [ExpandedArg]), CmdError> {
    let mut result = FileSetArgs::default();
    let is_keyword = |arg: &ExpandedArg| {
        matches!(bytes(arg), b"TYPE" | b"BASE_DIRS" | b"FILES" | b"FILE_SET")
            || scope(bytes(arg)).is_some()
    };
    while let Some((key, tail)) = rest.split_first() {
        rest = match bytes(key) {
            b"TYPE" => {
                if let Some((value, next)) = tail.split_first().filter(|(arg, _)| !is_keyword(arg))
                {
                    result.kind = Some(bytes(value)).filter(|value| !value.is_empty());
                    next
                } else {
                    tail
                }
            }
            b"BASE_DIRS" | b"FILES" => {
                let end = tail.iter().position(is_keyword).unwrap_or(tail.len());
                let (values, next) = tail.split_at(end);
                let entries = values
                    .iter()
                    .map(bytes)
                    .filter(|value| !value.is_empty())
                    .map(|value| BT::new(absolute(value, dir), bt.clone()));
                if bytes(key) == b"BASE_DIRS" {
                    result.bases.extend(entries);
                } else {
                    result.files.extend(entries);
                }
                next
            }
            b"FILE_SET" => break,
            keyword if scope(keyword).is_some() => break,
            _ => {
                return Err(CmdError::status(format!(
                    "Unrecognized keyword: \"{}\".",
                    key.value
                )));
            }
        };
    }
    Ok((result, rest))
}

fn sycl_kind(name: &[u8]) -> Option<FileSetKind> {
    match name {
        b"SYCL" => Some(FileSetKind::Sources),
        b"SYCL_HEADERS" => Some(FileSetKind::Headers),
        _ => None,
    }
}

fn file_set_kind<'a>(
    name: &'a BString,
    kind: Option<&'a [u8]>,
    existing: Option<&FileSet>,
) -> Result<(FileSetKind, Option<&'a [u8]>), CmdError> {
    let resolve = |name: &[u8]| FileSetKind::from_name(name).or_else(|| sycl_kind(name));
    let kind_value = match kind {
        Some(value) => resolve(value).ok_or_else(|| {
            CmdError::status("File set TYPE may only be one of the built-in types \"HEADERS\", \"SOURCES\", \"SYCL\", \"SYCL_HEADERS\", \"CXX_MODULES\" or a RULE visible from the current directory.")
        })?,
        None => existing.map(|set| set.kind).or_else(|| resolve(name))
            .ok_or_else(|| CmdError::status("Must specify a TYPE when creating file set."))?,
    };
    if let Some(set) = existing.filter(|set| set.kind != kind_value) {
        return Err(CmdError::status(format!(
            "Type \"{}\" for file set \"{name}\" does not match original type \"{}\".",
            kind_value.as_str(),
            set.kind.as_str()
        )));
    }
    let spelling = kind.unwrap_or_else(|| {
        if sycl_kind(name).is_some() {
            name
        } else {
            kind_value.as_str().as_bytes()
        }
    });
    if name.first().is_some_and(u8::is_ascii_uppercase)
        && name.as_slice() != spelling
        && sycl_kind(name) != Some(kind_value)
    {
        return Err(CmdError::status(format!(
            "FILE_SET name starting with a capital letter must match the TYPE name \"{}\".",
            BString::from(spelling)
        )));
    }
    if name.starts_with(b"_") || !name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        return Err(CmdError::status(
            "Non-default file set name must contain only letters, numbers, and underscores, and must not start with a capital letter or underscore.",
        ));
    }
    let alias = if sycl_kind(spelling).is_some() {
        Some(spelling)
    } else {
        sycl_kind(name).map(|_| name.as_slice())
    };
    Ok((kind_value, alias))
}

/// Check the CMake 4.5 SYCL feature gate.
pub(crate) fn require_sycl(
    cx: &mut Cx<'_>,
    subject: impl std::fmt::Display,
) -> Result<(), CmdError> {
    let gate = cx.var(b"CMAKE_EXPERIMENTAL_SYCL");
    if gate.as_ref().is_some_and(|value| value == SYCL_GATE) {
        if cx
            .global()
            .props
            .get(b"\0CMAKE_EXPERIMENTAL_SYCL")
            .is_none()
        {
            cx.global()
                .props
                .set(b"\0CMAKE_EXPERIMENTAL_SYCL".as_slice().into(), "1".into());
            cx.issue(
                MessageKind::AuthorWarning,
                Effect::None,
                Some(DiagCategory::Experimental),
                "CMake's support for the SYCL programming language is experimental.  It is meant only for experimentation and feedback to CMake developers.".into(),
            );
        }
        return Ok(());
    }
    Err(crate::command_error(
        cx,
        format!(
            "{subject} is experimental and must be enabled by the CMAKE_EXPERIMENTAL_SYCL gate."
        ),
    ))
}

/// Find a file set on a target by name.
pub(crate) fn find_file_set(
    model: &ProjectModel,
    target: TargetId,
    name: &[u8],
) -> Option<FileSetId> {
    model
        .target(target)?
        .file_sets
        .iter()
        .copied()
        .find(|&id| model.file_set(id).is_some_and(|set| set.name == name))
}

/// Return the CMake spelling of a file set scope.
pub(crate) fn scope_name(scope: UsageScope) -> &'static str {
    match scope {
        UsageScope::Private => "PRIVATE",
        UsageScope::Public => "PUBLIC",
        UsageScope::Interface => "INTERFACE",
    }
}

fn contains_genex(value: &[u8]) -> bool {
    value.windows(2).any(|pair| pair == b"$<")
}

fn literal_paths<'a>(
    entries: impl Iterator<Item = &'a BT<BString>> + 'a,
    dir: &'a [u8],
) -> impl Iterator<Item = BString> + 'a {
    entries
        .filter(|entry| !contains_genex(&entry.value))
        .flat_map(|entry| list::split(&entry.value, list::Empty::Drop))
        .map(move |path| absolute(&path, dir))
}

fn path_key(path: &[u8]) -> BString {
    let mut key = Lexical::new(path).normalize();
    if !key.ends_with(b"/") {
        key.push(b'/');
    }
    key
}

/// Validate literal paths. Generation resolves paths that contain generator expressions.
fn validate_file_set_paths(model: &ProjectModel, set: &FileSet) -> Result<(), CmdError> {
    let target = model
        .target(set.target)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    let dir = model.dir(target.dir).source_dir.as_slice();
    let bases = literal_paths(set.base_dirs.iter(), dir).collect::<Vec<_>>();
    let mut keys: Vec<_> = bases.iter().map(|base| (path_key(base), base)).collect();
    keys.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    keys.dedup_by(|a, b| a.0 == b.0);
    for pair in keys.windows(2) {
        if let [(parent, first), (child, second)] = pair
            && child.starts_with(parent)
        {
            return Err(CmdError::status(format!(
                "Base directories in file set cannot be subdirectories of each other:\n  {first}\n  {second}"
            )));
        }
    }
    if !set.base_dirs.iter().any(|base| contains_genex(&base.value)) {
        for file in literal_paths(set.files.iter(), dir) {
            let path = path_key(&file);
            if !keys
                .iter()
                .any(|(base, _)| path.len() > base.len() && path.starts_with(base))
            {
                let bases = bases
                    .iter()
                    .map(|base| format!("\n  {base}"))
                    .collect::<String>();
                return Err(CmdError::status(format!(
                    "File:\n  {file}\nmust be in one of the file set's base directories:{bases}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_file_set_membership(
    cx: &mut Cx<'_>,
    set: &FileSet,
    first_new_file: usize,
) -> Result<(), CmdError> {
    let model = cx.project();
    let target = model
        .target(set.target)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    let dir = model.dir(target.dir).source_dir.as_slice();
    let policy = target.policies.policies.get(CMP0211);
    if set.kind == FileSetKind::Headers || policy == PolicyStatus::Old {
        return Ok(());
    }
    let mut owners = BTreeMap::new();
    for other in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
        if other.kind != FileSetKind::Headers
            && other.name != set.name
            && (other.scope != UsageScope::Interface && set.scope != UsageScope::Interface
                || other.scope != UsageScope::Private && set.scope != UsageScope::Private)
        {
            for file in literal_paths(other.files.iter(), dir) {
                owners.insert(path_key(&file), &other.name);
            }
        }
    }
    for file in literal_paths(set.files.iter().take(first_new_file), dir) {
        owners.insert(path_key(&file), &set.name);
    }
    let mut warnings = BTreeSet::new();
    for file in literal_paths(set.files.iter().skip(first_new_file), dir) {
        if let Some(other) = owners.insert(path_key(&file), &set.name) {
            let file = Lexical::new(&file).normalize();
            let message = format!(
                "In target \"{}\" the file\n  {file}\nalready belongs to file set \"{other}\".",
                target.name
            );
            if policy == PolicyStatus::New {
                return Err(CmdError::status(message));
            }
            warnings.insert(message);
        }
    }
    for warning in warnings {
        cx.issue_policy_warning(CMP0211, b"", warning.as_bytes());
    }
    Ok(())
}

/// Change the stored paths of a file set and its build-side sources.
pub(crate) fn replace_file_set_paths(
    cx: &mut Cx<'_>,
    id: FileSetId,
    bases: bool,
    value: Option<BString>,
    append: bool,
) -> Result<(), CmdError> {
    let bt = cx.backtrace();
    let mut set = cx
        .project()
        .file_set(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?
        .clone();
    let file_count = set.files.len();
    let entries = if bases {
        &mut set.base_dirs
    } else {
        &mut set.files
    };
    if !append {
        entries.clear();
    }
    let first_new_file = if bases { file_count } else { entries.len() };
    if let Some(value) = value {
        entries.push(BT::new(value, bt));
    }
    // A project can change the base directories and the sources in successive calls.
    if !bases {
        validate_file_set_membership(cx, &set, first_new_file)?;
    }
    let model = cx.project();
    *model
        .file_set_mut(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))? = set;
    properties::store_file_set_properties(model, id);
    Ok(())
}
