//! Argument grammar of `rcmake explain`.
//!
//! The parser reads tokens with their option arity first, then selects the mode from
//! the parsed tokens. Values after `--` are always positional. `--target` is a flag in
//! query mode and takes a value in eval mode.

#![forbid(unsafe_code)]

use rcmake_api::ExplainFilter;
use rcmake_base::BString;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kind {
    #[default]
    Auto,
    Target,
    Variable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) name: BString,
    pub(super) build: Option<BString>,
    pub(super) kind: Kind,
    pub(super) config: Option<BString>,
    pub(super) filter: Option<BString>,
    pub(super) typed: Option<ExplainFilter>,
    pub(super) json: bool,
    pub(super) verbose: bool,
    pub(super) color: Color,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FindOptions {
    pub(super) package: BString,
    pub(super) build: Option<BString>,
    pub(super) call: Option<usize>,
    pub(super) all_candidates: bool,
    pub(super) fresh: bool,
    pub(super) json: bool,
    pub(super) color: Color,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct EvalOptions {
    pub(super) expression: Option<BString>,
    pub(super) property: Option<BString>,
    pub(super) owner: Option<BString>,
    pub(super) consumer: Option<BString>,
    pub(super) build: Option<BString>,
    pub(super) config: Option<BString>,
    pub(super) language: Option<BString>,
    pub(super) link_language: Option<BString>,
    pub(super) directory: Option<BString>,
    pub(super) link: bool,
    pub(super) no_steps: bool,
    pub(super) command_config: Option<BString>,
    pub(super) output_config: Option<BString>,
    pub(super) json: bool,
    pub(super) color: Color,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Invocation {
    Help,
    FindHelp,
    EvalHelp,
    Query(Options),
    Find(FindOptions),
    Eval(EvalOptions),
}

const VALUED: &[&[u8]] = &[
    b"-B",
    b"--config",
    b"--filter",
    b"--flag",
    b"--define",
    b"--include",
    b"--dep",
    b"--color",
    b"--find",
    b"--eval",
    b"--eval-property",
    b"--call",
    b"--lang",
    b"--link-lang",
    b"--directory",
    b"--command-config",
    b"--output-config",
];
const QUERY_ONLY: &[&[u8]] = &[
    b"--var",
    b"--filter",
    b"--flag",
    b"--define",
    b"--include",
    b"--dep",
    b"--verbose",
];
const FIND_ONLY: &[&[u8]] = &[b"--call", b"--all-candidates", b"--fresh"];
const EVAL_ONLY: &[&[u8]] = &[
    b"--lang",
    b"--link-lang",
    b"--directory",
    b"--link",
    b"--no-steps",
    b"--command-config",
    b"--output-config",
];

#[derive(Default)]
struct Parsed {
    used: Vec<&'static [u8]>,
    positionals: Vec<BString>,
    build: Option<BString>,
    kind: Kind,
    target_value: Option<BString>,
    config: Option<BString>,
    filter: Option<BString>,
    typed: Option<ExplainFilter>,
    find: Option<BString>,
    eval: Option<BString>,
    eval_property: Option<BString>,
    call: Option<usize>,
    language: Option<BString>,
    link_language: Option<BString>,
    directory: Option<BString>,
    command_config: Option<BString>,
    output_config: Option<BString>,
    json: bool,
    verbose: bool,
    link: bool,
    no_steps: bool,
    all_candidates: bool,
    fresh: bool,
    color: Color,
}

/// Mode flags seen before `--`, with option values skipped.
#[derive(Default)]
struct Scan {
    eval: bool,
    find: bool,
    help: bool,
    first_positional: Option<BString>,
}

fn flag_name(raw: &[u8]) -> (&[u8], Option<&[u8]>) {
    if raw.starts_with(b"--") {
        let mut parts = raw.splitn(2, |byte| *byte == b'=');
        (parts.next().unwrap_or(raw), parts.next())
    } else if raw.len() > 2 && raw.starts_with(b"-B") {
        (b"-B", raw.get(2..))
    } else {
        (raw, None)
    }
}

fn scan(args: &[BString]) -> Scan {
    let mut scan = Scan::default();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let (flag, attached) = flag_name(arg);
        match flag {
            b"--" => break,
            b"--help" | b"-h" => scan.help = true,
            b"--eval" | b"--eval-property" => scan.eval = true,
            b"--find" => scan.find = true,
            _ => {}
        }
        if attached.is_none() && VALUED.contains(&flag) {
            iter.next_if(|next| !matches!(next.as_slice(), b"--help" | b"-h"));
        } else if !flag.starts_with(b"-") && scan.first_positional.is_none() {
            scan.first_positional = Some(arg.clone());
        }
    }
    scan
}

fn value<'a>(
    args: &mut impl Iterator<Item = &'a BString>,
    attached: Option<&[u8]>,
    flag: &str,
) -> Result<BString, String> {
    let value = attached.or_else(|| {
        args.next()
            .map(|value| value.as_slice())
            .filter(|arg| !arg.starts_with(b"--") && !matches!(*arg, b"-B" | b"-h"))
    });
    value
        .filter(|value| !value.is_empty())
        .map(BString::from)
        .ok_or_else(|| {
            format!(
                "Supply a value for {flag}. Use {flag}=<value> for a value that starts with --."
            )
        })
}

fn once(slot: &mut Option<BString>, value: BString, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("Give {flag} once."));
    }
    *slot = Some(value);
    Ok(())
}

fn color(value: &[u8]) -> Result<Color, String> {
    match value {
        b"auto" => Ok(Color::Auto),
        b"always" => Ok(Color::Always),
        b"never" => Ok(Color::Never),
        _ => Err("Use --color=auto, --color=always, or --color=never.".into()),
    }
}

fn set_kind(parsed: &mut Parsed, kind: Kind) -> Result<(), String> {
    if parsed.kind != Kind::Auto && parsed.kind != kind {
        return Err("Use either --target or --var.".into());
    }
    parsed.kind = kind;
    Ok(())
}

fn intern(flag: &[u8]) -> &'static [u8] {
    VALUED
        .iter()
        .chain(QUERY_ONLY)
        .chain(FIND_ONLY)
        .chain(EVAL_ONLY)
        .chain(&[b"--target".as_slice(), b"--json"])
        .find(|known| **known == flag)
        .copied()
        .unwrap_or(b"")
}

/// Parse `rcmake explain` arguments after the subcommand name.
pub(super) fn parse(args: &[BString]) -> Result<Invocation, String> {
    let scan = scan(args);
    let why_not = scan
        .first_positional
        .as_deref()
        .map(|value| value.as_slice())
        == Some(b"why-not");
    if scan.help {
        return Ok(if scan.find || why_not {
            Invocation::FindHelp
        } else if scan.eval {
            Invocation::EvalHelp
        } else {
            Invocation::Help
        });
    }
    let mut parsed = Parsed::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let raw = arg.as_slice();
        if raw == b"--" {
            parsed.positionals.extend(args.cloned());
            break;
        }
        let (flag, attached) = flag_name(raw);
        let no_value = |flag: &[u8]| {
            if attached.is_some() {
                Err(format!("{} takes no value.", String::from_utf8_lossy(flag)))
            } else {
                Ok(())
            }
        };
        match flag {
            b"--target" if scan.eval => {
                let consumer = value(&mut args, attached, "--target")?;
                once(&mut parsed.target_value, consumer, "--target")?;
            }
            b"--target" => {
                no_value(flag)?;
                set_kind(&mut parsed, Kind::Target)?;
            }
            b"--var" => {
                no_value(flag)?;
                set_kind(&mut parsed, Kind::Variable)?;
            }
            b"--json" => {
                no_value(flag)?;
                parsed.json = true;
            }
            b"--verbose" => {
                no_value(flag)?;
                parsed.verbose = true;
            }
            b"--link" => {
                no_value(flag)?;
                parsed.link = true;
            }
            b"--no-steps" => {
                no_value(flag)?;
                parsed.no_steps = true;
            }
            b"--all-candidates" => {
                no_value(flag)?;
                parsed.all_candidates = true;
            }
            b"--fresh" => {
                no_value(flag)?;
                parsed.fresh = true;
            }
            b"--config" => parsed.config = Some(value(&mut args, attached, "--config")?),
            b"--filter" | b"--flag" | b"--define" | b"--include" | b"--dep" => {
                let name = String::from_utf8_lossy(flag).into_owned();
                let text = value(&mut args, attached, &name)?;
                if parsed.typed.is_some() {
                    return Err(
                        "Use only one of --filter, --flag, --define, --include, or --dep.".into(),
                    );
                }
                parsed.typed = Some(match flag {
                    b"--filter" => {
                        parsed.filter = Some(text.clone());
                        ExplainFilter::Any(text)
                    }
                    b"--flag" => ExplainFilter::Flag(text),
                    b"--define" => ExplainFilter::define(&text),
                    b"--include" => ExplainFilter::Include(text),
                    _ => ExplainFilter::Dependency(text),
                });
            }
            b"--color" => parsed.color = color(&value(&mut args, attached, "--color")?)?,
            b"-B" => parsed.build = Some(value(&mut args, attached, "-B")?),
            b"--find" => once(
                &mut parsed.find,
                value(&mut args, attached, "--find")?,
                "--find",
            )?,
            b"--eval" => once(
                &mut parsed.eval,
                value(&mut args, attached, "--eval")?,
                "--eval",
            )?,
            b"--eval-property" => {
                let property = value(&mut args, attached, "--eval-property")?;
                once(&mut parsed.eval_property, property, "--eval-property")?;
            }
            b"--call" => {
                let text = value(&mut args, attached, "--call")?;
                let call = std::str::from_utf8(&text)
                    .ok()
                    .and_then(|text| text.parse::<usize>().ok())
                    .filter(|call| *call > 0)
                    .ok_or("Give --call a positive number.")?;
                parsed.call = Some(call);
            }
            b"--lang" => parsed.language = Some(value(&mut args, attached, "--lang")?),
            b"--link-lang" => {
                parsed.link_language = Some(value(&mut args, attached, "--link-lang")?)
            }
            b"--directory" => parsed.directory = Some(value(&mut args, attached, "--directory")?),
            b"--command-config" => {
                parsed.command_config = Some(value(&mut args, attached, "--command-config")?);
            }
            b"--output-config" => {
                parsed.output_config = Some(value(&mut args, attached, "--output-config")?);
            }
            _ if raw.starts_with(b"-") => {
                return Err(format!("Unknown option {arg:?}."));
            }
            _ => {
                if raw.is_empty() {
                    return Err("Specify one nonempty name.".into());
                }
                parsed.positionals.push(arg.clone());
                continue;
            }
        }
        parsed.used.push(intern(flag));
    }
    select(parsed, why_not)
}

fn reject(used: &[&'static [u8]], banned: &[&[u8]], mode: &str) -> Result<(), String> {
    match used.iter().find(|flag| banned.contains(flag)) {
        Some(flag) => Err(format!(
            "{} does not apply to {mode}.",
            String::from_utf8_lossy(flag)
        )),
        None => Ok(()),
    }
}

fn select(parsed: Parsed, why_not: bool) -> Result<Invocation, String> {
    let find_alias = why_not && parsed.positionals.len() == 2;
    let eval = parsed.eval.is_some() || parsed.eval_property.is_some();
    if (parsed.find.is_some() || find_alias) && eval {
        return Err("Use one of --find, --eval, or --eval-property.".into());
    }
    if parsed.find.is_some() || find_alias {
        reject(&parsed.used, QUERY_ONLY, "--find")?;
        reject(&parsed.used, EVAL_ONLY, "--find")?;
        reject(&parsed.used, &[b"--target", b"--config"], "--find")?;
        let package = match (parsed.find, parsed.positionals.as_slice()) {
            (Some(package), []) => package,
            (None, [_, package]) => package.clone(),
            _ => return Err("Specify one package.".into()),
        };
        return Ok(Invocation::Find(FindOptions {
            package,
            build: parsed.build,
            call: parsed.call,
            all_candidates: parsed.all_candidates,
            fresh: parsed.fresh,
            json: parsed.json,
            color: parsed.color,
        }));
    }
    if eval {
        reject(&parsed.used, QUERY_ONLY, "--eval")?;
        reject(&parsed.used, FIND_ONLY, "--eval")?;
        if parsed.eval.is_some() && parsed.eval_property.is_some() {
            return Err("Use either --eval or --eval-property.".into());
        }
        let positional = match parsed.positionals.as_slice() {
            [] => None,
            [name] => Some(name.clone()),
            _ => return Err("Specify at most one target name.".into()),
        };
        let (owner, consumer) = if parsed.eval_property.is_some() {
            let owner = positional.ok_or(
                "Name the target that owns the property: rcmake explain <target> --eval-property <property>.",
            )?;
            (Some(owner), parsed.target_value)
        } else {
            if positional.is_some() && parsed.target_value.is_some() {
                return Err("Name the target once, with --target or as a positional name.".into());
            }
            (None, parsed.target_value.or(positional))
        };
        return Ok(Invocation::Eval(EvalOptions {
            expression: parsed.eval,
            property: parsed.eval_property,
            owner,
            consumer,
            build: parsed.build,
            config: parsed.config,
            language: parsed.language,
            link_language: parsed.link_language,
            directory: parsed.directory,
            link: parsed.link,
            no_steps: parsed.no_steps,
            command_config: parsed.command_config,
            output_config: parsed.output_config,
            json: parsed.json,
            color: parsed.color,
        }));
    }
    reject(&parsed.used, FIND_ONLY, "a target or variable query")?;
    reject(&parsed.used, EVAL_ONLY, "a target or variable query")?;
    let name = match parsed.positionals.as_slice() {
        [] => return Err("Specify the name of a target or variable.".into()),
        [name] => name.clone(),
        _ => return Err("Specify one name.".into()),
    };
    let mut options = Options {
        name,
        build: parsed.build,
        kind: parsed.kind,
        config: parsed.config,
        filter: parsed.filter,
        typed: parsed.typed,
        json: parsed.json,
        verbose: parsed.verbose,
        color: parsed.color,
    };
    if options.typed.is_some() || options.config.is_some() {
        if options.kind == Kind::Variable {
            return Err(
                "--config and the entry filters apply to targets. Omit --var to use them.".into(),
            );
        }
        options.kind = Kind::Target;
    }
    Ok(Invocation::Query(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Invocation, String> {
        parse(&args.iter().copied().map(BString::from).collect::<Vec<_>>())
    }

    fn find(package: &str, build: Option<&str>) -> Invocation {
        Invocation::Find(FindOptions {
            package: package.into(),
            build: build.map(BString::from),
            ..FindOptions::default()
        })
    }

    #[test]
    fn why_not_needs_a_second_positional() {
        let query = parse_args(&["why-not", "-B", "build"]);
        assert_eq!(
            query,
            Ok(Invocation::Query(Options {
                name: "why-not".into(),
                build: Some("build".into()),
                ..Options::default()
            }))
        );
        assert_eq!(
            parse_args(&["why-not", "Boost", "-B", "build"]),
            Ok(find("Boost", Some("build")))
        );
        assert_eq!(
            parse_args(&["-Bbuild", "why-not", "Boost"]),
            Ok(find("Boost", Some("build")))
        );
        assert_eq!(parse_args(&["--find", "Boost"]), Ok(find("Boost", None)));
        assert_eq!(
            parse_args(&["why-not", "--target"]),
            Ok(Invocation::Query(Options {
                name: "why-not".into(),
                kind: Kind::Target,
                ..Options::default()
            }))
        );
        assert_eq!(parse_args(&["--", "why-not"]).map(|_| ()), Ok(()));
    }

    #[test]
    fn eval_target_is_valued_in_either_order() {
        let expected = Invocation::Eval(EvalOptions {
            expression: Some("$<CONFIG>".into()),
            consumer: Some("app".into()),
            ..EvalOptions::default()
        });
        assert_eq!(
            parse_args(&["--eval", "$<CONFIG>", "--target", "app"]),
            Ok(expected)
        );
        let reversed = parse_args(&["--target=app", "--eval=$<CONFIG>"]);
        assert!(matches!(
            reversed,
            Ok(Invocation::Eval(EvalOptions {
                consumer: Some(_),
                ..
            }))
        ));
        let property = parse_args(&["--target", "app", "liba", "--eval-property", "INTERFACE_X"]);
        assert_eq!(
            property,
            Ok(Invocation::Eval(EvalOptions {
                property: Some("INTERFACE_X".into()),
                owner: Some("liba".into()),
                consumer: Some("app".into()),
                ..EvalOptions::default()
            }))
        );
        assert!(parse_args(&["--eval-property", "X"]).is_err());
    }

    #[test]
    fn typed_filters_take_dash_values_in_attached_form() {
        let flag = parse_args(&["app", "--flag=--sysroot=/x"]);
        assert!(matches!(
            flag,
            Ok(Invocation::Query(Options { typed: Some(ExplainFilter::Flag(ref value)), kind: Kind::Target, .. })) if value == "--sysroot=/x"
        ));
        assert!(parse_args(&["app", "--flag", "--sysroot=/x"]).is_err());
        assert!(parse_args(&["app", "--flag", "-O2"]).is_ok());
        assert!(matches!(
            parse_args(&["app", "--define", "FOO=1"]),
            Ok(Invocation::Query(Options {
                typed: Some(ExplainFilter::Define { value: Some(_), .. }),
                ..
            }))
        ));
    }

    #[test]
    fn rejects_mixed_modes_and_foreign_flags() {
        let cases: &[&[&str]] = &[
            &["app", "--flag", "-O2", "--filter", "x"],
            &["app", "--define", "A", "--dep", "b"],
            &["--find", "Boost", "--eval", "x"],
            &["why-not", "Boost", "--eval", "x"],
            &["--find", "Boost", "--verbose"],
            &["--find", "Boost", "--target"],
            &["--eval", "x", "--var"],
            &["--eval", "x", "--fresh"],
            &["app", "--lang", "CXX"],
            &["app", "--call", "1"],
            &["--find", "Boost", "extra"],
            &["--eval", "x", "a", "b"],
            &["--eval", "x", "--eval-property", "Y", "t"],
            &["app", "--target=x"],
            &["--find", "Boost", "--call", "0"],
            &["--var", "x", "--flag", "-O2"],
        ];
        for args in cases {
            assert!(parse_args(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn mode_help_is_selected_from_parsed_tokens() {
        assert_eq!(parse_args(&["--find", "--help"]), Ok(Invocation::FindHelp));
        assert_eq!(parse_args(&["why-not", "-h"]), Ok(Invocation::FindHelp));
        assert_eq!(parse_args(&["--eval", "x", "-h"]), Ok(Invocation::EvalHelp));
        assert_eq!(parse_args(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse_args(&["--filter", "--help"]), Ok(Invocation::Help));
    }
}
