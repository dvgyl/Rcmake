use rcmake_base::{BString, list, truth};
use rcmake_diag::{Backtrace, DiagCategory, Effect, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_genex::{ConfigId, EvalContext, EvalSite, TargetHandle, UsageTraversal};
use rcmake_model::{
    BT, CacheType, DirId, LinkItem, ModelError, ProjectModel, Target, TargetBinding, TargetKind,
    UsageScope,
};
use rcmake_resolve::usage::UsageResolver;

use super::{
    bytes, defer_validation, generation_diagnostic, has_pending_deferred_commands, model_error,
    reported, scope,
};

pub(super) const CHECK_LINKS: &[u8] = b"\0rcmake-check-links";
const PLAIN_CALLS: &[u8] = b"\0target_link_libraries/plain";
const KEYWORD_CALLS: &[u8] = b"\0target_link_libraries/keyword";

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if let [marker] = args
        && bytes(marker) == CHECK_LINKS
        && cx.running_deferred()
    {
        if has_pending_deferred_commands(cx) {
            defer_validation(cx, b"target_link_libraries", &[CHECK_LINKS]);
        } else {
            cx.global().props.unset(CHECK_LINKS);
            validate_links(cx);
        }
        return Ok(Flow::Normal);
    }
    let [name, rest @ ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let dir = cx.current_dir_id();
    let model = cx.project();
    let Some(id) = model.lookup_target(dir, bytes(name)).and_then(|binding| {
        let id = binding.id();
        let target = model.target(id)?;
        match binding {
            TargetBinding::Target(_) if target.is_global() || target.dir == dir => Some(id),
            TargetBinding::Alias(_) if !model.dir(dir).local_targets.contains_key(bytes(name)) => {
                Some(id)
            }
            _ => None,
        }
    }) else {
        return Err(crate::command_error(
            cx,
            format!(
                "Cannot specify link libraries for target \"{}\" which is not built by this project.",
                name.value
            ),
        ));
    };
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if target
        .props
        .get(b"SYMBOLIC")
        .is_some_and(|value| truth::is_on(value))
    {
        return Err(CmdError::status("can not be used on a SYMBOLIC target."));
    }
    if target.kind == TargetKind::Utility {
        let name = target.name.clone();
        return Err(reported(
            cx,
            format!(
                "Utility target \"{name}\" must not be used as the target of a target_link_libraries call."
            ),
        ));
    }
    let Some(first) = rest.first() else {
        return Ok(Flow::Normal);
    };
    let Keyword {
        mut scope,
        signature,
    } = link_scope(bytes(first)).unwrap_or(Keyword {
        scope: UsageScope::Public,
        signature: Signature::Plain,
    });
    let policy = PolicyId::new(79);
    let remote_policy = cx.policy(policy);
    let bt = cx.backtrace();
    let model = cx.project();
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    let owner = target.dir;
    let propagate_private = target.kind.propagates_private_links();
    let legacy_cache = has_legacy_link_cache(target)
        && target.policies.policies.get(PolicyId::new(73)) != PolicyStatus::New;
    let mut legacy_dependencies = BString::default();
    let mut qualifier = None;
    let mut own = Vec::new();
    let mut interface = Vec::new();
    let mut warnings = Vec::new();
    let mut remote_warnings = Vec::new();
    let debug = debug_condition(model.global.props.get(b"DEBUG_CONFIGURATIONS"));
    let mut has_item = false;
    let mut args = rest.iter().enumerate().peekable();
    while let Some((index, arg)) = args.next() {
        let value = bytes(arg);
        if let Some(keyword) = link_scope(value) {
            if index != 0 && (signature == Signature::Plain || signature != keyword.signature) {
                let option = if keyword.signature == Signature::Modern {
                    "INTERFACE, PUBLIC or PRIVATE option"
                } else {
                    "LINK_INTERFACE_LIBRARIES, LINK_PUBLIC or LINK_PRIVATE option"
                };
                return Err(reported(
                    cx,
                    format!(
                        "The {option} must appear as the second argument, just after the target name."
                    ),
                ));
            }
            scope = keyword.scope;
            continue;
        }
        if let Some(next) = Qualifier::parse(value) {
            if let Some(previous) = qualifier.replace(next) {
                warnings.push(format!(
                    "Link library type specifier \"{}\" is followed by specifier \"{}\" instead of a library name.  The first specifier will be ignored.",
                    previous.name(), next.name()
                ));
            }
            continue;
        }
        if let Some(kind) = invalid_link_scope(target, scope, signature) {
            return Err(reported(
                cx,
                format!(
                    "{kind} library can only be used with the INTERFACE keyword of target_link_libraries"
                ),
            ));
        }
        let mut text = arg.value.clone();
        if qualifier.is_none() {
            let mut depth = genex_depth(0, value);
            while depth != 0 {
                let Some((_, next)) = args.next_if(|(_, arg)| !is_keyword(bytes(arg))) else {
                    break;
                };
                text.push(b';');
                text.extend_from_slice(bytes(next));
                depth = genex_depth(depth, bytes(next));
            }
        }
        let value = text.as_slice();
        let qualifier = qualifier.take().unwrap_or(Qualifier::General);
        has_item = true;
        let linked_target = model
            .find_target(dir, value)
            .and_then(|id| model.target(id));
        if legacy_cache
            && scope != UsageScope::Interface
            && !value.is_empty()
            && value != target.name
            && !contains_genex(value)
            && linked_target.is_none_or(|linked| {
                !matches!(
                    linked.kind,
                    TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
                )
            })
        {
            legacy_dependencies.extend_from_slice(qualifier.name().as_bytes());
            legacy_dependencies.push(b';');
            legacy_dependencies.extend_from_slice(value);
            legacy_dependencies.push(b';');
        }
        if value.is_empty() {
            continue;
        }
        if owner != dir && remote_policy != PolicyStatus::New {
            if scope != UsageScope::Interface {
                cx.issue_in_directory(
                    MessageKind::Error,
                    Effect::SetError,
                    None,
                    format!(
                        "Attempt to add link library \"{text}\" to target \"{}\" which is not built in this directory.\nThis is allowed only when policy CMP0079 is set to NEW.",
                        name.value
                    ).into(),
                );
                return Err(CmdError::Reported);
            }
            if remote_policy == PolicyStatus::Warn {
                remote_warnings.push(text.clone());
            }
        }
        let item = |value| LinkItem {
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            scope: Some(scope),
            text: BT::new(value, bt.clone()),
            lookup_dir: dir,
        };
        if scope != UsageScope::Interface {
            if let Some(linked) = linked_target
                && linked.is_global()
                && !linkable(linked)
            {
                let kind = linked.kind.as_str();
                return Err(crate::command_error(
                    cx,
                    format!(
                        "Target \"{}\" of type {kind} may not be linked into another target. One may link only to INTERFACE, OBJECT, STATIC or SHARED libraries, or to executables with the ENABLE_EXPORTS property set.",
                        BString::from(value)
                    ),
                ));
            }
            let stable_target = linked_target.is_some_and(|target| !target.is_imported());
            let text = if qualifier != Qualifier::General && stable_target {
                [b"$<TARGET_NAME:", value, b">"].concat().into()
            } else {
                text.clone()
            };
            own.push(item(qualifier.apply(&debug, text)));
        }
        if scope != UsageScope::Private || propagate_private {
            let mut text = qualifier.apply(&debug, text.clone());
            if scope == UsageScope::Private && maybe_target_name(value) {
                text = [b"$<LINK_ONLY:", text.as_slice(), b">"].concat().into();
            }
            interface.push(item(text));
        }
    }
    if let Some(pending) = qualifier {
        return Err(missing_library(cx, pending));
    }
    if let Some(error) = signature_error(target, signature, has_item) {
        return Err(crate::command_error(cx, error));
    }
    for warning in warnings {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            None,
            warning.into(),
        );
    }
    if owner != dir {
        if remote_policy == PolicyStatus::New {
            for batch in [&mut own, &mut interface] {
                if let Some(first) = batch.first_mut() {
                    first.opens_lookup_scope = true;
                }
                if let Some(last) = batch.last_mut() {
                    last.closes_lookup_scope = true;
                }
            }
        } else {
            for item in &mut interface {
                item.lookup_dir = owner;
            }
        }
    }
    for text in remote_warnings {
        let body = format!(
            "Target\n  {}\nis not created in this directory.  For compatibility with older versions of CMake, link library\n  {text}\nwill be looked up in the directory in which the target was created rather than in this calling directory.",
            name.value,
        );
        cx.issue_policy_warning(policy, b"", body.as_bytes());
    }
    let model = cx.project();
    let root = model.dir(DirId::ROOT).source_dir.clone();
    let check_links = !own.is_empty();
    let fields = [
        check_links.then_some(b"LINK_LIBRARIES".as_slice()),
        (!interface.is_empty()).then_some(b"INTERFACE_LINK_LIBRARIES"),
    ]
    .into_iter()
    .flatten()
    .map(|field| (field, rcmake_model::AppendMode::List));
    let cache_key = model
        .update_target_properties(
            id,
            fields,
            rcmake_model::PropOrigin {
                dir,
                backtrace: &bt,
            },
            |target| {
                if has_item && let Some(frame) = bt.top() {
                    let relative = frame
                        .file
                        .strip_prefix(root.as_slice())
                        .and_then(|file| file.strip_prefix(b"/"));
                    let file = BString::from(relative.unwrap_or(&frame.file));
                    let name = frame
                        .name
                        .as_deref()
                        .map_or_else(|| "target_link_libraries".into(), BString::from);
                    let call = format!("\n * {file}:{} ({name})", frame.line);
                    target
                        .props
                        .append_string(signature.property(), call.as_bytes());
                }
                let cache_key =
                    (!legacy_dependencies.is_empty()).then(|| legacy_link_cache_key(&target.name));
                if check_links {
                    target
                        .usage
                        .link_libraries
                        .get_or_insert_default()
                        .extend(own);
                }
                if !interface.is_empty() {
                    target
                        .usage
                        .interface_link_libraries
                        .get_or_insert_default()
                        .extend(interface);
                }
                cache_key
            },
        )
        .map_err(model_error)?;
    if let Some(key) = cache_key {
        let mut value = cx
            .cache()
            .get(&key)
            .map_or_else(BString::default, |entry| entry.value.clone());
        value.extend_from_slice(&legacy_dependencies);
        cx.write_cache(
            &key,
            value,
            CacheType::Static,
            b"Dependencies for the target",
        );
    }
    if check_links {
        schedule_validation(cx);
    }
    Ok(Flow::Normal)
}

pub(super) fn has_legacy_link_cache(target: &Target) -> bool {
    !target.is_imported()
        && matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        )
}

pub(super) fn legacy_link_cache_key(name: &[u8]) -> BString {
    [name, b"_LIB_DEPENDS"].concat().into()
}

fn linkable(target: &Target) -> bool {
    match target.kind {
        TargetKind::ModuleLibrary | TargetKind::Utility | TargetKind::GlobalTarget => false,
        TargetKind::Executable => target
            .props
            .get(b"ENABLE_EXPORTS")
            .is_some_and(|value| truth::is_on(value)),
        _ => true,
    }
}

pub(super) fn schedule_validation(cx: &mut Cx<'_>) {
    if cx.global().props.get(CHECK_LINKS).is_none()
        && defer_validation(cx, b"target_link_libraries", &[CHECK_LINKS])
    {
        cx.global().props.set(CHECK_LINKS.into(), "1".into());
    }
}

fn validate_links(cx: &mut Cx<'_>) {
    let mut configs = if cx
        .project()
        .generator
        .is_some_and(|generator| generator.multi_config())
    {
        list::split(
            &cx.var_or_empty(b"CMAKE_CONFIGURATION_TYPES"),
            list::Empty::Drop,
        )
    } else {
        vec![cx.var_or_empty(b"CMAKE_BUILD_TYPE")]
    };
    if configs.is_empty() {
        configs.push(BString::default());
    }
    let enabled = cx
        .global()
        .props
        .get(b"ENABLED_LANGUAGES")
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    let in_probe = cx
        .global()
        .props
        .get(b"IN_TRY_COMPILE")
        .is_some_and(|value| truth::is_on(value));
    let ipo_variables = enabled
        .into_iter()
        .map(|lang| {
            let extensions = [b"CMAKE_", lang.as_slice(), b"_SOURCE_FILE_EXTENSIONS"].concat();
            let support = [b"_CMAKE_", lang.as_slice(), b"_IPO_SUPPORTED_BY_CMAKE"].concat();
            let compiler = [
                b"_CMAKE_",
                lang.as_slice(),
                b"_IPO_MAY_BE_SUPPORTED_BY_COMPILER",
            ]
            .concat();
            [extensions, support, compiler].map(|name| {
                let value = cx.var_or_empty(&name);
                (name, value)
            })
        })
        .collect::<Vec<_>>();
    let model = cx.project();
    let root = model.dir(DirId::ROOT).source_dir.clone();
    let mut diagnostics = Vec::new();
    let mut resolver = UsageResolver::new(model);
    let mut seen_imports = std::collections::HashSet::new();
    let mut seen_deprecations = std::collections::HashSet::new();
    let mut seen_ipo = std::collections::HashSet::new();
    for config in configs {
        let config = ConfigId(config);
        for (id, target) in model.targets() {
            if !seen_ipo.contains(&id)
                && let Some(diagnostic) =
                    ipo_diagnostic(model, target, &config, &ipo_variables, in_probe)
            {
                seen_ipo.insert(id);
                diagnostics.push(diagnostic);
            }
            let Some(links) = &target.usage.link_libraries else {
                continue;
            };
            let trace = &target.backtrace;
            for link in links {
                let value = if link.text.value.windows(2).any(|pair| pair == b"$<") {
                    let ctx = EvalContext {
                        config: config.clone(),
                        current: Some(TargetHandle(id.index() as u32)),
                        head: Some(TargetHandle(id.index() as u32)),
                        traversal: UsageTraversal::Link,
                        quiet: true,
                        site: EvalSite {
                            backtrace: link.text.backtrace.clone(),
                            property: Some("LINK_LIBRARIES".into()),
                        },
                        ..EvalContext::default()
                    };
                    let Ok(ast) = rcmake_genex::parse(&link.text.value) else {
                        continue;
                    };
                    let Ok((value, _)) =
                        rcmake_genex::evaluate(&ast, &ctx, &mut resolver.host(link.lookup_dir))
                    else {
                        continue;
                    };
                    value
                } else {
                    link.text.value.clone()
                };
                for item in list::iter(&value, list::Empty::Drop) {
                    let item = item.as_ref();
                    let trimmed = item.trim_ascii();
                    let linked = model.find_target(link.lookup_dir, trimmed);
                    if !trimmed.is_empty() && trimmed != item {
                        diagnostics.push(LinkDiagnostic::error(trace.clone(), format!(
                        "Target \"{}\" links to item \"{}\" which has leading or trailing whitespace.  This is now an error according to policy CMP0004.",
                        target.name, BString::from(item)
                    )));
                    }
                    if trimmed == target.name.as_slice()
                        || target.policies.policies.get(PolicyId::new(108)) == PolicyStatus::New
                            && linked == Some(id)
                    {
                        diagnostics.push(LinkDiagnostic::error(
                            trace.clone(),
                            format!("Target \"{}\" links to itself.", target.name),
                        ));
                        break;
                    }
                    if let Some(linked_id) = linked
                        && let Some(linked) = model.target(linked_id)
                    {
                        if let Some(message) = linked
                            .props
                            .get(b"DEPRECATION")
                            .filter(|message| !message.is_empty())
                            && seen_deprecations.insert(linked_id)
                        {
                            diagnostics.push(LinkDiagnostic {
                            location: Location::Backtrace(link.text.backtrace.clone()),
                            kind: MessageKind::AuthorWarning,
                            category: Some(DiagCategory::Author),
                            body: format!("The library that is being linked to, {}, is marked as being deprecated by the owner.  The message provided by the developer is:\n{message}\n", linked.name).into(),
                        });
                        }
                        if let Some(error) = missing_import(&resolver, linked, &config)
                            && seen_imports.insert((linked_id, config.clone()))
                        {
                            diagnostics.push(error);
                        }
                    }
                }
            }
        }
    }
    let mut failed = false;
    for diagnostic in diagnostics {
        failed |= generation_diagnostic(
            cx,
            &root,
            diagnostic.location,
            diagnostic.kind,
            diagnostic.category,
            diagnostic.body,
        );
    }
    if failed {
        super::generation_failed(cx);
    }
}

struct LinkDiagnostic {
    location: Location,
    kind: MessageKind,
    category: Option<DiagCategory>,
    body: BString,
}

impl LinkDiagnostic {
    fn error(trace: Backtrace, message: String) -> Self {
        Self {
            location: Location::Backtrace(trace),
            kind: MessageKind::Error,
            category: None,
            body: message.into(),
        }
    }
}

fn ipo_diagnostic(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    languages: &[[(Vec<u8>, BString); 3]],
    in_probe: bool,
) -> Option<LinkDiagnostic> {
    let property = b"INTERPROCEDURAL_OPTIMIZATION";
    let configured = [property.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
    if target.is_imported()
        || matches!(
            target.kind,
            TargetKind::InterfaceLibrary | TargetKind::Utility
        )
        || !target
            .props
            .get(&configured)
            .or_else(|| target.props.get(property))
            .is_some_and(|value| truth::is_on(value))
    {
        return None;
    }
    let status = target.policies.policies.get(PolicyId::new(69));
    let location = Location::Backtrace(target.backtrace.clone());
    let declaration = target
        .sources
        .iter()
        .flat_map(|entry| list::iter(&entry.value, list::Empty::Drop));
    if status == PolicyStatus::Warn && !in_probe {
        return Some(LinkDiagnostic {
            kind: MessageKind::AuthorWarning,
            category: Some(DiagCategory::Policy),
            location,
            body: format!(
                "Policy CMP0069 is not set: INTERPROCEDURAL_OPTIMIZATION is enforced when enabled.  Run \"cmake --help-policy CMP0069\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\nINTERPROCEDURAL_OPTIMIZATION property will be ignored for target '{}'.",
                target.name
            ).into(),
        });
    }
    if status != PolicyStatus::New {
        return None;
    }
    let extensions: Vec<_> = declaration
        .filter_map(|source| {
            let offset = source.iter().rposition(|&byte| byte == b'.')?;
            let extension = source.get(offset + 1..)?;
            (!extension.contains(&b'/') && !extension.contains(&b'>')).then(|| extension.to_vec())
        })
        .collect();
    for [
        (extensions_name, defined),
        (support_name, support),
        (compiler_name, compiler),
    ] in languages
    {
        let value = |name: &[u8], fallback: &BString| {
            if target.dir == DirId::ROOT {
                fallback.clone()
            } else {
                model
                    .definition(target.dir, name)
                    .cloned()
                    .unwrap_or_else(|| fallback.clone())
            }
        };
        let known = value(extensions_name, defined);
        if !list::iter(&known, list::Empty::Drop).any(|known| {
            extensions
                .iter()
                .any(|ext| ext.as_slice() == known.as_ref())
        }) {
            continue;
        }
        let support = value(support_name, support);
        let compiler = value(compiler_name, compiler);
        let body = if !support.is_empty() && truth::is_off(&support) {
            "CMake doesn't support IPO for current compiler"
        } else if !compiler.is_empty() && truth::is_off(&compiler) {
            "Compiler doesn't support IPO"
        } else {
            continue;
        };
        return Some(LinkDiagnostic::error(
            target.backtrace.clone(),
            body.to_owned(),
        ));
    }
    None
}

fn missing_import(
    resolver: &UsageResolver<'_>,
    target: &Target,
    config: &ConfigId,
) -> Option<LinkDiagnostic> {
    if !target.is_imported()
        || !linkable(target)
        || UsageResolver::location_property_config(target.kind, b"LOCATION").is_none()
        || resolver.imported_runtime_location(target, config).is_some()
    {
        return None;
    }
    let kind = match target.policies.policies.get(PolicyId::new(111)) {
        PolicyStatus::New => MessageKind::Error,
        PolicyStatus::Warn => MessageKind::AuthorWarning,
        PolicyStatus::Old => return None,
    };
    let property = if target.kind == TargetKind::SharedLibrary {
        "IMPORTED_LOCATION or IMPORTED_IMPLIB"
    } else {
        "IMPORTED_LOCATION"
    };
    let config = if config.0.is_empty() {
        String::new()
    } else {
        format!(" configuration \"{}\"", config.0)
    };
    let prefix = if kind == MessageKind::AuthorWarning {
        "Policy CMP0111 is not set: An imported target missing its location property fails during generation.  Run \"cmake --help-policy CMP0111\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n"
    } else {
        ""
    };
    Some(LinkDiagnostic {
        location: Location::FileLine {
            file: b"CMakeLists.txt".as_slice().into(),
            line: 0,
        },
        kind,
        category: (kind == MessageKind::AuthorWarning).then_some(DiagCategory::Policy),
        body: format!(
            "{prefix}{property} not set for imported target \"{}\"{config}.",
            target.name
        )
        .into(),
    })
}

fn is_keyword(arg: &[u8]) -> bool {
    link_scope(arg).is_some() || Qualifier::parse(arg).is_some()
}

fn genex_depth(mut depth: usize, arg: &[u8]) -> usize {
    for (index, byte) in arg.iter().enumerate() {
        match byte {
            b'$' if arg.get(index + 1) == Some(&b'<') => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

fn invalid_link_scope(
    target: &Target,
    scope: UsageScope,
    signature: Signature,
) -> Option<&'static str> {
    if scope == UsageScope::Interface && signature == Signature::Modern {
        None
    } else if target.kind == TargetKind::InterfaceLibrary {
        Some("INTERFACE")
    } else if target.is_imported() {
        Some("IMPORTED")
    } else {
        None
    }
}

fn maybe_target_name(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-:".contains(byte))
        || contains_genex(value)
}

fn contains_genex(value: &[u8]) -> bool {
    value
        .windows(2)
        .position(|pair| pair == b"$<")
        .and_then(|start| value.get(start + 2..))
        .is_some_and(|tail| tail.contains(&b'>'))
}

fn signature_error(target: &Target, signature: Signature, has_item: bool) -> Option<String> {
    if !has_item {
        return None;
    }
    let (property, previous) = if signature == Signature::Plain {
        (KEYWORD_CALLS, "keyword")
    } else {
        (PLAIN_CALLS, "plain")
    };
    let calls = target.props.get(property)?;
    let name = &target.name;
    Some(format!(
        "The {previous} signature for target_link_libraries has already been used with the target \"{name}\".  All uses of target_link_libraries with a target must be either all-keyword or all-plain.\nThe uses of the {previous} signature are here:{calls}\n"
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signature {
    Plain,
    Modern,
    Legacy,
}

impl Signature {
    fn property(self) -> BString {
        if self == Self::Plain {
            PLAIN_CALLS.into()
        } else {
            KEYWORD_CALLS.into()
        }
    }
}

struct Keyword {
    scope: UsageScope,
    signature: Signature,
}

fn link_scope(value: &[u8]) -> Option<Keyword> {
    if let Some(scope) = scope(value) {
        return Some(Keyword {
            scope,
            signature: Signature::Modern,
        });
    }
    let scope = match value {
        b"LINK_PRIVATE" => UsageScope::Private,
        b"LINK_PUBLIC" => UsageScope::Public,
        b"LINK_INTERFACE_LIBRARIES" => UsageScope::Interface,
        _ => return None,
    };
    Some(Keyword {
        scope,
        signature: Signature::Legacy,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Qualifier {
    Debug,
    Optimized,
    General,
}

impl Qualifier {
    fn parse(value: &[u8]) -> Option<Self> {
        match value {
            b"debug" => Some(Self::Debug),
            b"optimized" => Some(Self::Optimized),
            b"general" => Some(Self::General),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Optimized => "optimized",
            Self::General => "general",
        }
    }

    fn apply(self, debug: &[u8], value: BString) -> BString {
        match self {
            Self::Debug => [b"$<", debug, b":", value.as_slice(), b">"].concat().into(),
            Self::Optimized => [b"$<$<NOT:", debug, b">:", value.as_slice(), b">"]
                .concat()
                .into(),
            Self::General => value,
        }
    }
}

fn missing_library(cx: &mut Cx<'_>, qualifier: Qualifier) -> CmdError {
    reported(
        cx,
        format!(
            "The \"{}\" argument must be followed by a library.",
            qualifier.name()
        ),
    )
}

fn debug_condition(configs: Option<&BString>) -> BString {
    let mut configs = configs.map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    if configs.is_empty() {
        configs.push("DEBUG".into());
    }
    let expressions: Vec<_> = configs
        .into_iter()
        .map(|mut config| {
            config.make_ascii_uppercase();
            [b"$<CONFIG:", config.as_slice(), b">"].concat()
        })
        .collect();
    if let [expr] = expressions.as_slice() {
        return expr.clone().into();
    }
    [b"$<OR:", expressions.join(&b',').as_slice(), b">"]
        .concat()
        .into()
}
