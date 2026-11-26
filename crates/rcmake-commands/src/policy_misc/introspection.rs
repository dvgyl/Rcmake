use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_host::Stream;
use rcmake_model::{CacheType, UsageField};
use rcmake_regex::{MatchOptions, Regex, Scratch};

use super::{properties, quoted};

mod deferred;
pub use deferred::print_deferred_properties;

pub(super) fn targets(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    const KEYS: &[&[u8]] = &[b"REGEX", b"IGNORE_CASE", b"IMPORTED_ONLY", b"NO_IMPORTED"];
    let options = Options::parse(cx, args, KEYS, &[b"REGEX"], &[])?;
    options.check_unknown(
        "Unknown argument(s) given to cmake_language(PRINT_TARGETS) call: ",
        ".",
    )?;
    let imported = options.has(b"IMPORTED_ONLY");
    let local = options.has(b"NO_IMPORTED");
    let insensitive = options.has(b"IGNORE_CASE");
    let pattern = options.value(b"REGEX");
    if imported && local {
        return Err(CmdError::status(
            "IMPORTED_ONLY and NO_IMPORTED keywords are mutually exclusive in cmake_language(PRINT_TARGETS) call.",
        ));
    }
    if insensitive && pattern.is_none() {
        return Err(CmdError::status(
            "IGNORE_CASE keyword in cmake_language(PRINT_TARGETS) call is only valid with REGEX.",
        ));
    }
    let filter = compile(cx, "REGEX", pattern, insensitive)?;
    let suffix = pattern.map_or_else(String::new, |pattern| {
        format!(
            " matching REGEX '{}' ({})",
            BString::from(pattern),
            case_label(insensitive)
        )
    });
    let label = if imported {
        "Imported"
    } else if local {
        "Non-imported"
    } else {
        "All"
    };
    let mut matcher = Matcher::new(cx, insensitive);
    let model = cx.project();
    let mut lines = Vec::new();
    for (_, target) in model.targets() {
        let is_imported = target.is_imported();
        if (imported && !is_imported)
            || (local && is_imported)
            || !matcher.matches(filter.as_deref(), &target.name)?
        {
            continue;
        }
        lines.push((&target.name, target.kind, is_imported));
    }
    lines.sort_by(|left, right| left.0.cmp(right.0));
    if lines.is_empty() {
        if pattern.is_some() {
            warn(
                cx,
                format!("No targets{suffix} in cmake_language(PRINT_TARGETS ...)."),
            );
        }
    } else {
        let mut output = BString::from(format!("Printing targets...\n {label} targets{suffix}:\n"));
        for (name, kind, imported) in lines {
            let annotation = if imported { ", IMPORTED" } else { "" };
            output.extend_from_slice(
                format!("   {name} ({}{annotation})\n", kind.as_str()).as_bytes(),
            );
        }
        print(cx, output);
    }
    Ok(Flow::Normal)
}

pub(super) fn variables(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    const KEYS: &[&[u8]] = &[
        b"ALL",
        b"NAMED",
        b"NAME_REGEX",
        b"VALUE_REGEX",
        b"IGNORE_CASE",
        b"__CMAKE_PRINT_VARIABLES",
    ];
    let options = Options::parse(
        cx,
        args,
        KEYS,
        &[b"NAME_REGEX", b"VALUE_REGEX"],
        &[b"NAMED"],
    )?;
    options.check_unknown(
        "Unknown argument(s) given to cmake_language(PRINT_VARIABLES): ",
        ".",
    )?;
    let named = options.has(b"NAMED");
    let legacy = options.has(b"__CMAKE_PRINT_VARIABLES");
    let insensitive = options.has(b"IGNORE_CASE");
    let name = options.value(b"NAME_REGEX");
    let value = options.value(b"VALUE_REGEX");
    check_mode(&options, "PRINT_VARIABLES", "__CMAKE_PRINT_VARIABLES")?;
    if named {
        if insensitive || name.is_some() || value.is_some() {
            return Err(CmdError::status(
                "NAME_REGEX, VALUE_REGEX, and IGNORE_CASE in cmake_language(PRINT_VARIABLES) call are only valid with ALL, not NAMED.",
            ));
        }
        let mut output = BString::default();
        if !legacy {
            output.extend_from_slice(b"Printing variables...\n Named variables:\n");
        }
        for (index, name) in options.list(b"NAMED").iter().enumerate() {
            let value = cx.var(name);
            if legacy {
                if index > 0 {
                    output.extend_from_slice(b" ; ");
                }
                output.extend_from_slice(name);
                output.extend_from_slice(b"=\"");
                output.extend_from_slice(
                    value
                        .as_ref()
                        .map(|value| value.as_slice())
                        .unwrap_or_default(),
                );
                output.push(b'"');
            } else {
                assignment(
                    &mut output,
                    name,
                    value.as_ref().map(|value| value.as_slice()),
                );
            }
        }
        print(cx, output);
        return Ok(Flow::Normal);
    }

    let filters = Filters::new(cx, "", name, value, insensitive)?;
    let description = filters.description(insensitive);
    let mut matcher = Matcher::new(cx, insensitive);
    let mut lines = BString::default();
    let names = cx.variable_names();
    for name in names {
        if !matcher.matches(filters.name.as_deref(), &name)? {
            continue;
        }
        if let Some(value) = cx.var_normal_only(&name)
            && matcher.matches(filters.value.as_deref(), &value)?
        {
            assignment(&mut lines, &name, Some(&value));
        }
        if let Some(entry) = cx.cache().get(&name)
            && matcher.matches(filters.value.as_deref(), &entry.value)?
        {
            let mut label = quoted("CACHE{", &name, "}");
            if entry.ty != CacheType::Uninitialized {
                label.push(b':');
                label.extend_from_slice(entry.ty.as_str().as_bytes());
            }
            assignment(&mut lines, &label, Some(&entry.value));
        }
    }
    if lines.is_empty() {
        warn(
            cx,
            format!("No variables in scope{description} in cmake_language(PRINT_VARIABLES ...)."),
        );
    } else {
        let file = cx.current_list_file();
        let mut output = BString::from(format!(
            "Printing variables...\n Variables in scope at '{file}'{description}:\n"
        ));
        output.extend_from_slice(&lines);
        print(cx, output);
    }
    Ok(Flow::Normal)
}

const SCOPES: [(&[u8], &[u8]); 5] = [
    (b"TARGETS", b"TARGET"),
    (b"SOURCES", b"SOURCE"),
    (b"TESTS", b"TEST"),
    (b"DIRECTORIES", b"DIRECTORY"),
    (b"CACHE_ENTRIES", b"CACHE"),
];

pub(super) fn properties(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    const NAME_REGEX: &[u8] = b"PROPERTY_NAME_REGEX";
    const VALUE_REGEX: &[u8] = b"PROPERTY_VALUE_REGEX";
    const FILTERS: &[&[u8]] = &[NAME_REGEX, VALUE_REGEX];
    let mut lists: Vec<_> = SCOPES.iter().map(|&(keyword, _)| keyword).collect();
    lists.push(b"NAMED");
    let keys = [
        lists.as_slice(),
        FILTERS,
        &[
            b"ALL",
            b"DEFERRED",
            b"FOLLOW_DEPENDENCIES",
            b"__CMAKE_PRINT_PROPERTIES",
        ],
    ]
    .concat();
    let options = Options::parse(cx, args, &keys, FILTERS, &lists)?;
    if !options.unknown.is_empty() {
        return Err(CmdError::status(quoted(
            "Unknown keywords: \"",
            &options.unknown.join(&b' '),
            "\"",
        )));
    }
    let modes: Vec<_> = SCOPES
        .iter()
        .filter(|(name, _)| !options.list(name).is_empty())
        .collect();
    let &(keyword, scope) = match modes.as_slice() {
        [mode] => *mode,
        [] => {
            return Err(CmdError::status(
                "mode keyword missing in cmake_language(PRINT_PROPERTIES) call, there must be exactly one of TARGETS SOURCES TESTS DIRECTORIES CACHE_ENTRIES",
            ));
        }
        _ => {
            return Err(CmdError::status(
                "multiple mode keywords used in cmake_language(PRINT_PROPERTIES) call, there must be exactly one of TARGETS SOURCES TESTS DIRECTORIES CACHE_ENTRIES.",
            ));
        }
    };
    let named = options.has(b"NAMED");
    let all = !named;
    let legacy = options.has(b"__CMAKE_PRINT_PROPERTIES");
    check_mode(&options, "PRINT_PROPERTIES", "__CMAKE_PRINT_PROPERTIES")?;
    if scope != b"TARGET" {
        for flag in ["ALL", "DEFERRED", "FOLLOW_DEPENDENCIES"] {
            if options.has(flag.as_bytes()) {
                return Err(CmdError::status(format!(
                    "{flag} keyword in cmake_language(PRINT_PROPERTIES) call is only valid with the TARGETS scope."
                )));
            }
        }
        if all {
            return Err(CmdError::status(format!(
                "NAMED keyword missing in cmake_language(PRINT_PROPERTIES) call with {} scope.",
                BString::from(keyword)
            )));
        }
    }
    let name = options.value(NAME_REGEX);
    let value = options.value(VALUE_REGEX);
    if name.is_some() || value.is_some() {
        if scope != b"TARGET" {
            return Err(CmdError::status(
                "PROPERTY_NAME_REGEX and PROPERTY_VALUE_REGEX in cmake_language(PRINT_PROPERTIES) call are only valid with the TARGETS scope and ALL.",
            ));
        }
        if named {
            return Err(CmdError::status(
                "PROPERTY_NAME_REGEX and PROPERTY_VALUE_REGEX in cmake_language(PRINT_PROPERTIES) call are only valid with ALL, not NAMED.",
            ));
        }
    }
    let filters = Filters::new(cx, "PROPERTY_", name, value, false)?;
    let follow = options.has(b"FOLLOW_DEPENDENCIES");
    if options.has(b"DEFERRED") || follow {
        let request = rcmake_model::PropertyPrint {
            dir: cx.current_dir_id(),
            backtrace: cx.backtrace(),
            targets: options
                .list(keyword)
                .iter()
                .copied()
                .map(BString::from)
                .collect(),
            names: named.then(|| {
                options
                    .list(b"NAMED")
                    .iter()
                    .copied()
                    .map(BString::from)
                    .collect()
            }),
            name_filter: name.map(BString::from),
            value_filter: value.map(BString::from),
            follow,
            legacy,
        };
        cx.project().property_prints.push(request);
        return Ok(Flow::Normal);
    }

    let description = filters.description(false);
    let dir = cx.current_dir_id();
    let mut matcher = Matcher::new(cx, false);
    let mut lines = BString::default();
    for object in options.list(keyword) {
        let props = if all {
            let Some(id) = cx.project().find_target(dir, object) else {
                missing_target(&mut lines, object);
                continue;
            };
            target_property_names(cx, id)
        } else {
            if scope == b"TARGET" && cx.project().find_target(dir, object).is_none() {
                missing_target(&mut lines, object);
                continue;
            }
            options
                .list(b"NAMED")
                .iter()
                .copied()
                .map(BString::from)
                .collect()
        };
        let mut block = BString::default();
        for name in props {
            if !matcher.matches(filters.name.as_deref(), &name)? {
                continue;
            }
            let value = properties::print_value(cx, scope, object, &name)?;
            let Some(value) = value else { continue };
            if all && value.is_none() {
                continue;
            }
            if !matcher.matches(
                filters.value.as_deref(),
                value
                    .as_ref()
                    .map(|value| value.as_slice())
                    .unwrap_or_default(),
            )? {
                continue;
            }
            let qualified = [*object, b".", &name].concat();
            assignment(
                &mut block,
                &qualified,
                value.as_ref().map(|value| value.as_slice()),
            );
        }
        let scope = BString::from(scope);
        let object = BString::from(*object);
        if !block.is_empty() {
            let kind = if all { "All properties" } else { "Properties" };
            lines.extend_from_slice(
                format!(" {kind} for {scope} {object}{description}:\n").as_bytes(),
            );
            lines.extend_from_slice(&block);
        } else if all {
            warn(
                cx,
                format!(
                    "No properties for {scope} {object}{description} in cmake_language(PRINT_PROPERTIES ...)."
                ),
            );
        }
    }
    if !lines.is_empty() {
        let mut output = BString::from(if legacy {
            "\n"
        } else {
            "Printing properties...\n"
        });
        output.extend_from_slice(&lines);
        print(cx, output);
    }
    Ok(Flow::Normal)
}

fn target_property_names(cx: &mut Cx<'_>, id: rcmake_model::TargetId) -> Vec<BString> {
    let Some(target) = cx.project().target(id) else {
        return Vec::new();
    };
    let mut names = target.props.names();
    // These fields are stored separately from the target's property map.
    names.extend(UsageField::ALL.into_iter().map(|field| field.name().into()));
    names.extend(
        [
            "BINARY_DIR",
            "IMPORTED",
            "IMPORTED_GLOBAL",
            "INTERFACE_LINK_LIBRARIES",
            "LINK_LIBRARIES",
            "MANUALLY_ADDED_DEPENDENCIES",
            "NAME",
            "SOURCE_DIR",
            "SOURCES",
            "TYPE",
        ]
        .map(BString::from),
    );
    names.retain(|name| {
        // These file set aliases are queryable, but CMake does not enumerate them.
        name != b"FILE_SET_TYPES"
            && !name.starts_with(b"FILE_SETS_")
            && !name.starts_with(b"INTERFACE_FILE_SETS_")
    });
    names.sort();
    names.dedup();
    names
}

fn missing_target(output: &mut BString, name: &[u8]) {
    output.extend_from_slice(&quoted("\n No such TARGET \"", name, "\" !\n\n"));
}

fn print(cx: &Cx<'_>, output: BString) {
    cx.host()
        .write_out(Stream::Stdout, &[b"-- ", output.as_slice(), b"\n"].concat());
}

fn warn(cx: &mut Cx<'_>, message: String) {
    cx.issue(MessageKind::Warning, Effect::None, None, message.into());
}

fn assignment(output: &mut BString, name: &[u8], value: Option<&[u8]>) {
    output.extend_from_slice(b"   ");
    output.extend_from_slice(name);
    output.extend_from_slice(b" = ");
    if let Some(value) = value {
        output.push(b'"');
        output.extend_from_slice(value);
        output.extend_from_slice(b"\"\n");
    } else {
        output.extend_from_slice(b"<NOTFOUND>\n");
    }
}

fn case_label(insensitive: bool) -> &'static str {
    if insensitive {
        "case insensitive"
    } else {
        "case sensitive"
    }
}

fn compile(
    cx: &mut Cx<'_>,
    key: &str,
    pattern: Option<&[u8]>,
    insensitive: bool,
) -> Result<Option<Arc<Regex>>, CmdError> {
    pattern
        .map(|pattern| {
            let folded;
            let input = if insensitive {
                folded = pattern.to_ascii_lowercase();
                &folded
            } else {
                pattern
            };
            cx.regex(input).map_err(|_| {
                CmdError::status(format!(
                    "{key} regular expression \"{}\" cannot compile.",
                    BString::from(pattern)
                ))
            })
        })
        .transpose()
}

struct Filters<'a> {
    name: Option<Arc<Regex>>,
    value: Option<Arc<Regex>>,
    raw_name: Option<&'a [u8]>,
    raw_value: Option<&'a [u8]>,
    variable: bool,
}

impl<'a> Filters<'a> {
    fn new(
        cx: &mut Cx<'_>,
        prefix: &str,
        name: Option<&'a [u8]>,
        value: Option<&'a [u8]>,
        insensitive: bool,
    ) -> Result<Self, CmdError> {
        Ok(Self {
            name: compile(cx, &format!("{prefix}NAME_REGEX"), name, insensitive)?,
            value: compile(cx, &format!("{prefix}VALUE_REGEX"), value, insensitive)?,
            raw_name: name,
            raw_value: value,
            variable: prefix.is_empty(),
        })
    }

    fn description(&self, insensitive: bool) -> String {
        let mut description = match (self.raw_name, self.raw_value) {
            (Some(name), Some(value)) => format!(
                " matching name '{}' and value '{}'",
                BString::from(name),
                BString::from(value)
            ),
            (Some(name), None) => format!(" matching name '{}'", BString::from(name)),
            (None, Some(value)) => format!(" matching value '{}'", BString::from(value)),
            (None, None) => return String::new(),
        };
        if self.variable {
            description.push_str(&format!(" ({})", case_label(insensitive)));
        }
        description
    }
}

struct Matcher {
    budget: u64,
    scratch: Scratch,
    insensitive: bool,
}

impl Matcher {
    fn new(cx: &Cx<'_>, insensitive: bool) -> Self {
        Self {
            budget: cx.limits().regex_steps,
            scratch: Scratch::default(),
            insensitive,
        }
    }

    fn matches(&mut self, regex: Option<&Regex>, value: &[u8]) -> Result<bool, CmdError> {
        let Some(regex) = regex else { return Ok(true) };
        let folded;
        let input = if self.insensitive {
            folded = value.to_ascii_lowercase();
            &folded
        } else {
            value
        };
        regex
            .find_at_with_scratch(
                input,
                0,
                MatchOptions::default(),
                &mut self.budget,
                &mut self.scratch,
            )
            .map(|result| result.is_some())
            .map_err(|_| CmdError::status("regular expression match exceeds the step limit."))
    }
}

struct Options<'a> {
    entries: Vec<(&'a [u8], Vec<&'a [u8]>)>,
    unknown: Vec<&'a [u8]>,
}

impl<'a> Options<'a> {
    fn parse(
        cx: &mut Cx<'_>,
        args: &[&'a [u8]],
        keys: &[&[u8]],
        single: &[&[u8]],
        multi: &[&[u8]],
    ) -> Result<Self, CmdError> {
        let mut options = Self {
            entries: Vec::new(),
            unknown: Vec::new(),
        };
        let mut args = args.iter().copied().peekable();
        while let Some(key) = args.next() {
            if !keys.contains(&key) {
                options.unknown.push(key);
                continue;
            }
            let mut values = Vec::new();
            if single.contains(&key) {
                values.extend(args.next_if(|word| !keys.contains(word)));
            } else if multi.contains(&key) {
                while let Some(word) = args.next_if(|word| {
                    !keys.contains(word)
                        || (key == b"NAMED" && SCOPES.iter().any(|(scope, _)| scope == word))
                }) {
                    values.push(word);
                }
            }
            let scope = SCOPES.iter().any(|(name, _)| *name == key);
            if values.is_empty() && !scope && (single.contains(&key) || multi.contains(&key)) {
                return Err(crate::command_error(
                    cx,
                    quoted("Error after keyword \"", key, "\": missing required value"),
                ));
            }
            if let Some((_, previous)) = options.entries.iter_mut().find(|(name, _)| *name == key) {
                if single.contains(&key) {
                    *previous = values;
                } else {
                    previous.extend(values);
                }
            } else {
                options.entries.push((key, values));
            }
        }
        Ok(options)
    }

    fn has(&self, key: &[u8]) -> bool {
        self.entries.iter().any(|(name, _)| *name == key)
    }

    fn value(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.list(key).first().copied()
    }

    fn list(&self, key: &[u8]) -> &[&'a [u8]] {
        self.entries
            .iter()
            .find(|(name, _)| *name == key)
            .map_or(&[], |(_, values)| values.as_slice())
    }

    fn check_unknown(&self, prefix: &str, suffix: &str) -> Result<(), CmdError> {
        if self.unknown.is_empty() {
            return Ok(());
        }
        let mut message = BString::from(prefix);
        for (index, word) in self.unknown.iter().enumerate() {
            if index > 0 {
                message.push(b' ');
            }
            message.extend_from_slice(&quoted("\"", word, "\""));
        }
        message.extend_from_slice(suffix.as_bytes());
        Err(CmdError::status(message))
    }
}

fn check_mode(options: &Options<'_>, op: &str, legacy: &str) -> Result<(), CmdError> {
    if options.has(b"ALL") && options.has(b"NAMED") {
        return Err(CmdError::status(format!(
            "ALL and NAMED keywords in cmake_language({op}) call are mutually exclusive."
        )));
    }
    if options.has(legacy.as_bytes()) && !options.has(b"NAMED") {
        return Err(CmdError::status(format!(
            "{legacy} in cmake_language({op}) call is only valid with NAMED."
        )));
    }
    Ok(())
}
