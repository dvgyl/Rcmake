use rcmake_base::{BString, list};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{
    BT, DirId, LinkItem, ModelError, Target, TargetBinding, TargetKind, UsageScope,
};

use super::{bytes, model_error, reported, scope, usage_target};

const PLAIN_CALLS: &[u8] = b"\0target_link_libraries/plain";
const KEYWORD_CALLS: &[u8] = b"\0target_link_libraries/keyword";

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, rest @ ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let id = usage_target(cx, &name.value, "link libraries")?;
    let dir = cx.current_dir_id();
    if matches!(
        cx.project().lookup_target(dir, bytes(name)),
        Some(TargetBinding::Alias(_))
    ) {
        return Err(CmdError::status("can not be used on an ALIAS target."));
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
    let bt = cx.backtrace();
    let model = cx.project();
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    validate_scope(target, scope)?;
    let owner = target.dir;
    let propagate_private = target.kind.propagates_private_links();
    let mut qualifier = None;
    let mut own = Vec::new();
    let mut interface = Vec::new();
    let mut warnings = Vec::new();
    let debug = debug_condition(model.global.props.get(b"DEBUG_CONFIGURATIONS"));
    let mut has_item = false;
    for (index, arg) in rest.iter().enumerate() {
        let value = bytes(arg);
        if let Some(keyword) = link_scope(value) {
            if let Some(pending) = qualifier {
                return Err(missing_library(cx, pending));
            }
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
            validate_scope(target, scope)?;
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
        let qualifier = qualifier.take().unwrap_or(Qualifier::General);
        has_item = true;
        if value.is_empty() {
            continue;
        }
        let item = |value| LinkItem {
            text: BT::new(value, bt.clone()),
            lookup_dir: dir,
        };
        if scope != UsageScope::Interface {
            let stable_target = model
                .find_target(dir, value)
                .and_then(|id| model.target(id))
                .is_some_and(|target| !target.is_imported());
            let text = if qualifier != Qualifier::General && stable_target {
                [b"$<TARGET_NAME:", value, b">"].concat().into()
            } else {
                arg.value.clone()
            };
            own.push(item(qualifier.apply(&debug, text)));
        }
        if scope != UsageScope::Private || propagate_private {
            let mut text = qualifier.apply(&debug, arg.value.clone());
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
    if owner != dir && cx.policy(PolicyId::new(79)) != PolicyStatus::New {
        if let Some(item) = own.first() {
            let text = &item.text.value;
            return Err(crate::command_error(
                cx,
                format!(
                    "Attempt to add link library \"{text}\" to target \"{}\" which is not built in this directory.\n\nThis is allowed only when policy CMP0079 is set to NEW.",
                    name.value
                ),
            ));
        }
        for item in &mut interface {
            item.lookup_dir = owner;
        }
    }
    let model = cx.project();
    let root = model.dir(DirId::ROOT).source_dir.clone();
    let target = model
        .target_mut(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
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
    if !own.is_empty() {
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
    Ok(Flow::Normal)
}

fn validate_scope(target: &Target, scope: UsageScope) -> Result<(), CmdError> {
    if scope != UsageScope::Interface {
        let kind = if target.kind == TargetKind::InterfaceLibrary {
            Some("INTERFACE")
        } else if target.is_imported() {
            Some("IMPORTED")
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(CmdError::status(format!(
                "{kind} library can only be used with the INTERFACE keyword of target_link_libraries"
            )));
        }
    }
    Ok(())
}

fn maybe_target_name(value: &[u8]) -> bool {
    value.windows(2).any(|pair| pair == b"$<")
        || value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-:".contains(byte))
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
        "The {previous} signature for target_link_libraries has already been used with the target \"{name}\".  All uses of target_link_libraries with a target must be either all-keyword or all-plain.\n\nThe uses of the {previous} signature are here:\n{calls}\n"
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
