use rcmake_base::BString;
use rcmake_diag::{
    DiagAction, DiagCategory, DiagEngine, DiagOptions, Diagnostic, Effect, Location, MessageKind,
    RenderOptions,
};
use rcmake_eval::{CacheDefine, Interp};
use rcmake_host::{Host, Stream};
use rcmake_json::Value;
use rcmake_model::{Cache, CacheType};

pub(super) fn apply_define(cache: &mut Cache, def: CacheDefine) -> bool {
    let previous = cache.get(&def.name);
    let ty = def.ty.or_else(|| previous.map(|entry| entry.ty));
    let normalized = |value: &[u8]| {
        if matches!(ty, Some(CacheType::Path | CacheType::Filepath)) {
            value
                .iter()
                .map(|&byte| if byte == b'\\' { b'/' } else { byte })
                .collect()
        } else {
            BString::from(value)
        }
    };
    let changed = previous.is_none_or(|entry| normalized(&entry.value) != normalized(&def.value));
    crate::cache::apply(cache, def);
    changed
}

pub(super) fn report(host: &dyn Host, interp: &mut Interp, results: &mut Vec<Value>) -> bool {
    let category = DiagCategory::UnusedCli;
    let state = *interp.cx().diag_state();
    let action = state.get(category);
    if action == DiagAction::Ignore {
        return false;
    }
    let names: Vec<_> = interp.unread_variables().map(BString::from).collect();
    let mut cx = interp.cx();
    let mut names = names
        .into_iter()
        .filter(|name| cx.cache().get(name).is_some())
        .peekable();
    if names.peek().is_none() {
        return false;
    }
    let is_error = action >= DiagAction::SendError;
    let message = b"Manually-specified variables were not used by the project:";
    let mut body = BString::from(message.as_slice());
    let mut display = BString::from(if is_error {
        "CMake Error (unused-cli):\n  "
    } else {
        "CMake Warning (unused-cli):\n  "
    });
    display.extend_from_slice(message);
    display.extend_from_slice(b"\n\n");
    for name in names {
        body.extend_from_slice(b"\n  ");
        body.extend_from_slice(&name);
        display.extend_from_slice(b"    ");
        display.extend_from_slice(&name);
        display.push(b'\n');
    }
    display.extend_from_slice(b"\n\n");
    host.write_out(Stream::Stderr, &display);

    let mut engine = DiagEngine::new(DiagOptions {
        initial_state: state,
        sarif: true,
    });
    let diagnostic = Diagnostic {
        annotations: None,
        kind: if is_error {
            MessageKind::Error
        } else {
            MessageKind::Warning
        },
        effect: Effect::None,
        category: Some(category),
        policy: None,
        location: Location::None,
        body,
    };
    engine.issue(&diagnostic, &state, &RenderOptions::default());
    results.extend(crate::sarif::results_from_engine(&engine));
    is_error
}
