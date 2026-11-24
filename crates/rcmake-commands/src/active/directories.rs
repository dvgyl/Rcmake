use rcmake_base::{BString, ByteSlice};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{BT, CacheEntry, CacheType, GeneratorKind, Target, TargetKind, TargetOrigin};

use super::{status, values};

pub(super) fn aux_source_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    const EXTENSIONS: &[&[u8]] = &[
        b"C", b"c", b"c++", b"c++m", b"cc", b"ccm", b"cpp", b"cppm", b"cu", b"cxx", b"cxxm",
        b"ixx", b"m", b"mm", b"mpp",
    ];
    let values = values(args);
    let [path, output] = values.as_slice() else {
        return Err(status("called with incorrect number of arguments"));
    };
    let mut entries = cx
        .host()
        .read_dir(&cx.resolve_path(path))
        .unwrap_or_default();
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut result = cx.var(output).unwrap_or_default();
    if !result.is_empty() {
        result.push(b';');
    }
    let sources = entries.iter().filter(|entry| {
        entry
            .name
            .iter()
            .rposition(|&byte| byte == b'.')
            .filter(|&dot| dot > 0)
            .and_then(|dot| entry.name.get(dot + 1..))
            .is_some_and(|extension| EXTENSIONS.contains(&extension))
    });
    for (index, entry) in sources.enumerate() {
        if index > 0 {
            result.push(b';');
        }
        result.extend_from_slice(path);
        result.push(b'/');
        result.extend_from_slice(&entry.name);
        if result.len() > cx.limits().max_value_bytes {
            return Err(status("source list exceeds the value size limit"));
        }
    }
    cx.set_var(output, result);
    Ok(Flow::Normal)
}

pub(super) fn include_regular_expression(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let values = values(args);
    let (pattern, complain) = match values.as_slice() {
        [pattern] => (*pattern, b"^$".as_slice()),
        [pattern, complain] => (*pattern, *complain),
        _ => return Err(status("called with incorrect number of arguments")),
    };
    let props = cx.directory_props();
    props.set("INCLUDE_REGULAR_EXPRESSION".into(), pattern.into());
    props.set("_RCMAKE_INCLUDE_REGEX_COMPLAIN".into(), complain.into());
    Ok(Flow::Normal)
}

#[derive(Clone, Copy)]
enum ExternalOption {
    Type,
    Guid,
    Platform,
}

impl ExternalOption {
    const KEYWORDS: [(&'static [u8], Self); 3] = [
        (b"TYPE", Self::Type),
        (b"GUID", Self::Guid),
        (b"PLATFORM", Self::Platform),
    ];
}

pub(super) fn include_external_msproject(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let values = values(args);
    let [name, location, options @ ..] = values.as_slice() else {
        return Err(status(
            "INCLUDE_EXTERNAL_MSPROJECT called with incorrect number of arguments",
        ));
    };
    if !matches!(cx.project().generator, Some(GeneratorKind::VisualStudio(_))) {
        return Ok(Flow::Normal);
    }
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    let mut target = Target::new(
        (*name).into(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        dir,
        backtrace.clone(),
        *cx.policies_mut(),
    );
    let mut external_type = b"".as_slice();
    let mut guid = b"".as_slice();
    let mut platform = b"".as_slice();
    let mut next = None;
    for &argument in options {
        if let Some((_, option)) = ExternalOption::KEYWORDS
            .iter()
            .find(|(keyword, _)| argument == *keyword)
        {
            next = Some(*option);
            continue;
        }
        match next.take() {
            Some(ExternalOption::Type) => external_type = argument,
            Some(ExternalOption::Guid) => guid = argument,
            Some(ExternalOption::Platform) => platform = argument,
            None => target
                .utility_deps
                .push(BT::new(argument.into(), backtrace.clone())),
        }
    }
    target
        .props
        .set("GENERATOR_FILE_NAME".into(), (*name).into());
    target.props.set(
        "EXTERNAL_MSPROJECT".into(),
        location.replace(b"\\", b"/").into(),
    );
    for (property, value) in [
        ("VS_PROJECT_TYPE", external_type),
        ("VS_PLATFORM_MAPPING", platform),
    ] {
        if !value.is_empty() {
            target.props.set(property.into(), value.into());
        }
    }
    if cx.project().dir(dir).exclude_from_all {
        target.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    }
    cx.project()
        .add_target(target)
        .map_err(|error| status(error.to_string()))?;
    if !guid.is_empty() {
        let key: BString = [*name, b"_GUID_CMAKE"].concat().into();
        cx.cache().insert(
            key,
            CacheEntry {
                value: guid.into(),
                ty: CacheType::Internal,
                help: "Stored GUID".into(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
    Ok(Flow::Normal)
}
