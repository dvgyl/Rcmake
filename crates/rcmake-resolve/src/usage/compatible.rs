use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rcmake_base::{BString, list, truth};
use rcmake_diag::{Location, MessageKind, format_paragraphs};
use rcmake_genex::{ConfigId, EvalContext, EvalFlags, InterfaceView};
use rcmake_host::Stream;
use rcmake_model::{ProjectModel, Target, TargetId, TargetKind};

use super::{UsageResolver, handle};
use crate::{ResolveError, lower::definition, sources::SourcePaths};

mod builtin;

const PIC: &[u8] = b"POSITION_INDEPENDENT_CODE";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Bool,
    String,
    Min,
    Max,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Bool, Self::String, Self::Min, Self::Max];

    fn declaration(self) -> &'static str {
        match self {
            Self::Bool => "COMPATIBLE_INTERFACE_BOOL",
            Self::String => "COMPATIBLE_INTERFACE_STRING",
            Self::Min => "COMPATIBLE_INTERFACE_NUMBER_MIN",
            Self::Max => "COMPATIBLE_INTERFACE_NUMBER_MAX",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Bool => "Boolean",
            Self::String => "String",
            Self::Min => "Numeric minimum",
            Self::Max => "Numeric maximum",
        }
    }

    fn compare(self, current: &[u8], next: &[u8]) -> Option<Comparison> {
        match self {
            Self::Bool => {
                (truth::is_on(current) == truth::is_on(next)).then_some(Comparison::Agree)
            }
            Self::String => (current == next).then_some(Comparison::Agree),
            Self::Min | Self::Max => {
                let (current, next) = (number(current)?, number(next)?);
                Some(if current == next {
                    Comparison::Agree
                } else if (self == Self::Min && next < current)
                    || (self == Self::Max && next > current)
                {
                    Comparison::Dominant
                } else {
                    Comparison::Ignored
                })
            }
        }
    }

    fn display(self, value: Option<&[u8]>) -> BString {
        match self {
            Self::Bool if value.is_some_and(truth::is_on) => "TRUE".into(),
            Self::Bool => "FALSE".into(),
            _ => value.map_or_else(|| "(unset)".into(), BString::from),
        }
    }
}

enum Comparison {
    Agree,
    Dominant,
    Ignored,
    Set,
}

impl Comparison {
    fn description(&self) -> &'static str {
        match self {
            Self::Agree => "Agree",
            Self::Dominant => "Dominant",
            Self::Ignored => "Ignored",
            Self::Set => "Interface set",
        }
    }
}

#[derive(Default)]
pub(super) struct Properties {
    names: OnceCell<HashSet<BString>>,
    pub(super) implied: HashMap<(TargetId, ConfigId), HashSet<BString>>,
}

type Requirements = BTreeMap<BString, BTreeSet<Kind>>;

impl Properties {
    pub(super) fn with_implied(implied: HashMap<(TargetId, ConfigId), HashSet<BString>>) -> Self {
        Self {
            names: OnceCell::new(),
            implied,
        }
    }

    pub(super) fn has_declarations(target: &Target) -> bool {
        Kind::ALL
            .into_iter()
            .any(|kind| target.props.get(kind.declaration().as_bytes()).is_some())
    }

    fn contains(&self, model: &ProjectModel, name: &[u8]) -> bool {
        self.names
            .get_or_init(|| {
                model
                    .targets()
                    .flat_map(|(_, target)| declarations(target).map(|(_, name)| name))
                    .collect()
            })
            .contains(name)
    }

    fn implied(&self, target: TargetId, config: &ConfigId, name: &[u8]) -> bool {
        self.implied
            .get(&(target, config.clone()))
            .is_some_and(|names| names.contains(name))
    }
}

impl UsageResolver<'_> {
    /// Return whether CMake documents this name as a built-in target property.
    #[must_use]
    pub fn is_builtin_target_property(name: &[u8]) -> bool {
        builtin::contains(name)
    }

    pub(super) fn scalar_property(
        &mut self,
        id: TargetId,
        name: &[u8],
        raw: Option<BString>,
        ctx: &EvalContext,
    ) -> Result<InterfaceView, ResolveError> {
        let raw_view = |raw: Option<BString>| InterfaceView {
            value: raw.unwrap_or_default(),
            flags: EvalFlags::default(),
        };
        if name != PIC
            && (!self.has_compatible_decls
                || builtin::contains(name)
                || !self.compatible.contains(self.model, name))
        {
            return Ok(raw_view(raw));
        }
        if let Some(property) = &ctx.site.property
            && matches!(
                property.as_slice(),
                b"LINK_LIBRARIES"
                    | b"INTERFACE_LINK_LIBRARIES"
                    | b"INTERFACE_LINK_LIBRARIES_DIRECT"
                    | b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
            )
        {
            if raw.is_none() && ctx.head == Some(handle(id)) {
                self.compatible
                    .implied
                    .entry((id, ctx.config.clone()))
                    .or_default()
                    .insert(name.into());
            }
            return Ok(raw_view(raw));
        }
        if name == PIC && raw.is_some() {
            return Ok(raw_view(raw));
        }
        self.check_reentry(id, name)?;
        self.with_property(id, name, |this| {
            let mut ctx = ctx.clone();
            ctx.head = Some(handle(id));
            let deps = this.compatibility_targets(id, &ctx)?;
            let kind = if name == PIC {
                Some(Kind::Bool)
            } else {
                deps.iter()
                    .filter_map(|&dep| this.model.target(dep))
                    .flat_map(declarations)
                    .find_map(|(kind, property)| (property == name).then_some(kind))
            };
            let Some(kind) = kind else {
                return Ok(raw_view(raw));
            };
            if matches!(kind, Kind::Bool | Kind::String) && raw.is_some() {
                return Ok(raw_view(raw));
            }
            let mut merge = Merge::new(
                kind,
                raw,
                this.compatible.implied(id, &ctx.config, name),
                None,
            );
            this.merge_compatible(id, name, &deps, &ctx, &mut merge)?;
            Ok(InterfaceView {
                value: if kind == Kind::Bool {
                    if merge
                        .value
                        .as_ref()
                        .is_some_and(|value| truth::is_on(value))
                    {
                        "1".into()
                    } else {
                        "0".into()
                    }
                } else {
                    merge.value.unwrap_or_default()
                },
                flags: EvalFlags {
                    context_sensitive: true,
                    ..EvalFlags::default()
                },
            })
        })
    }

    pub(crate) fn validate_compatible(
        &mut self,
        id: TargetId,
        ctx: &EvalContext,
        paths: &SourcePaths<'_>,
    ) -> Result<Option<bool>, ResolveError> {
        let target = self.target(id)?;
        if target.kind == TargetKind::InterfaceLibrary {
            return Ok(None);
        }
        if !self.has_compatible_decls && !self.has_interface_pic && !self.debug_properties {
            let own = target.props.get(PIC);
            let implied = self.compatible.implied(id, &ctx.config, PIC);
            return Ok(own
                .map(|value| truth::is_on(value))
                .or_else(|| implied.then_some(false)));
        }
        let deps = self.compatibility_targets(id, ctx)?;
        let mut rules = self.declared_rules(target, &deps, paths)?;
        rules.entry(PIC.into()).or_default().insert(Kind::Bool);
        let mut ctx = ctx.clone();
        ctx.head = Some(handle(id));
        let debug = definition(self.model, target.dir, b"CMAKE_DEBUG_TARGET_PROPERTIES")
            .map(|value| list::split(value, list::Empty::Drop))
            .unwrap_or_default();
        let mut pic = None;
        for kind in Kind::ALL {
            for (name, kinds) in &rules {
                if !kinds.contains(&kind) {
                    continue;
                }
                let own = target.props.get(name).cloned();
                let implied = self.compatible.implied(id, &ctx.config, name);
                let report = debug.contains(name).then(String::new);
                let mut merge = Merge::new(kind, own, implied, report);
                self.merge_compatible(id, name, &deps, &ctx, &mut merge)?;
                if name == PIC {
                    pic = merge.value.as_ref().map(|value| truth::is_on(value));
                }
                if let Some(report) = merge.report {
                    let result = kind.display(merge.value.as_ref().map(|value| value.as_slice()));
                    let header = format!(
                        "{} compatibility of property \"{name}\" for target \"{}\" (result: \"{result}\"):",
                        kind.description(),
                        target.name,
                    );
                    paths.host.write_out(
                        Stream::Stderr,
                        &[
                            b"CMake Debug Log:\n".as_slice(),
                            &format_paragraphs(header.as_bytes(), 2, 77),
                            b"\n",
                            report.as_bytes(),
                            b"\n\n\n\n",
                        ]
                        .concat(),
                    );
                }
            }
        }
        Ok(pic)
    }

    /// Check that the dependencies of a target declare only user-defined
    /// compatible properties, each with one kind of compatibility.
    ///
    /// CMake does this check before it evaluates the usage requirements of
    /// the target. A failure in this check therefore hides a failure in the
    /// usage requirements.
    pub(crate) fn validate_compatible_declarations(
        &mut self,
        id: TargetId,
        ctx: &EvalContext,
        paths: &SourcePaths<'_>,
    ) -> Result<(), ResolveError> {
        let target = self.target(id)?;
        if target.kind == TargetKind::InterfaceLibrary || !self.has_compatible_decls {
            return Ok(());
        }
        // The link language is not known yet. A failure to find the
        // dependencies is reported later, when the link step is resolved.
        let Ok(deps) = self.compatibility_targets(id, ctx) else {
            return Ok(());
        };
        self.declared_rules(target, &deps, paths).map(|_| ())
    }

    fn declared_rules(
        &self,
        target: &Target,
        deps: &[TargetId],
        paths: &SourcePaths<'_>,
    ) -> Result<Requirements, ResolveError> {
        let mut rules = Requirements::new();
        for &owner in deps {
            for (kind, name) in declarations(self.target(owner)?) {
                if builtin::contains(&name) {
                    let owner = self.target(owner)?;
                    let declaration = kind.declaration();
                    return Err(self.compatibility_diagnostic(target, paths, format!(
                        "Target \"{}\" has property \"{name}\" listed in its {declaration} property.  This is not allowed.  Only user-defined properties may appear listed in the {declaration} property.",
                        owner.name,
                    )));
                }
                rules.entry(name).or_default().insert(kind);
            }
        }
        for (name, kinds) in &rules {
            if kinds.len() > 1 {
                let mut declarations: Vec<_> =
                    kinds.iter().map(|kind| kind.declaration()).collect();
                declarations.sort_unstable();
                let last = declarations.pop().unwrap_or_default();
                let first = declarations.join(", ");
                return Err(self.compatibility_diagnostic(target, paths, format!(
                    "Property \"{name}\" appears in both the {first} and the {last} property in the dependencies of target \"{}\".  This is not allowed.  A property may only require compatibility in a boolean interpretation, a numeric minimum, a numeric maximum or a string interpretation, but not a mixture.",
                    target.name,
                )));
            }
        }
        Ok(rules)
    }

    fn merge_compatible(
        &mut self,
        id: TargetId,
        name: &[u8],
        deps: &[TargetId],
        ctx: &EvalContext,
        merge: &mut Merge,
    ) -> Result<(), ResolveError> {
        let target = self.target(id)?;
        if let Some(report) = &mut merge.report {
            let prefix = format!("   * Target \"{}\" ", target.name);
            report.push_str(&prefix);
            if merge.explicit {
                let value = merge
                    .kind
                    .display(merge.value.as_ref().map(|value| value.as_slice()));
                report.push_str(&format!("has property content \"{value}\"\n"));
            } else if merge.implied {
                report.push_str("property is implied by use.\n");
            } else {
                report.push_str("property not set.\n");
            }
        }
        let interface = [b"INTERFACE_", name].concat();
        for &dep in deps {
            let dependency = self.target(dep)?;
            let Some(value) = dependency.props.get(&interface) else {
                continue;
            };
            let value = if name == PIC {
                self.eval_ide_property(dep, &interface, value, ctx)?
            } else {
                value.clone()
            };
            let comparison = match &merge.value {
                None => Some(Comparison::Set),
                Some(current) => merge.kind.compare(current, &value),
            };
            let Some(comparison) = comparison else {
                return Err(merge.conflict(target, dependency, name));
            };
            if let Some(report) = &mut merge.report {
                let display = merge.kind.display(Some(&value));
                report.push_str(&format!(
                    "   * Target \"{}\" property value \"{display}\" ({})\n",
                    dependency.name,
                    comparison.description(),
                ));
            }
            if matches!(comparison, Comparison::Set | Comparison::Dominant) {
                merge.value = Some(value);
            }
        }
        Ok(())
    }

    fn compatibility_diagnostic(
        &self,
        target: &Target,
        paths: &SourcePaths<'_>,
        message: String,
    ) -> ResolveError {
        let directory = self.model.dir(target.dir);
        let location =
            directory
                .listfiles
                .first()
                .map_or(Location::None, |file| Location::FileLine {
                    file: file.as_slice().into(),
                    line: 0,
                });
        let message = BString::from(message);
        paths.emit(self.model, MessageKind::Error, location, message.clone());
        ResolveError::Reported(message)
    }
}

struct Merge {
    kind: Kind,
    explicit: bool,
    implied: bool,
    value: Option<BString>,
    report: Option<String>,
}

impl Merge {
    fn new(kind: Kind, value: Option<BString>, implied: bool, report: Option<String>) -> Self {
        let explicit = value.is_some();
        Self {
            kind,
            explicit,
            implied: implied && !explicit,
            value: value.or_else(|| implied.then(BString::default)),
            report,
        }
    }

    fn conflict(&self, target: &Target, dependency: &Target, name: &[u8]) -> ResolveError {
        let name = BString::from(name);
        let body = if self.explicit {
            format!(
                "Property {name} on target \"{}\" does\nnot match the INTERFACE_{name} property requirement\nof dependency \"{}\".",
                target.name, dependency.name,
            )
        } else if self.implied {
            let implied = if self.kind == Kind::Bool {
                "FALSE"
            } else {
                "empty"
            };
            format!(
                "Property {name} on target \"{}\" is\nimplied to be {implied} because it was used to determine the link libraries\nalready. The INTERFACE_{name} property on\ndependency \"{}\" is in conflict.",
                target.name, dependency.name,
            )
        } else {
            format!(
                "The INTERFACE_{name} property of \"{}\" does\nnot agree with the value of {name} already determined\nfor \"{}\".",
                dependency.name, target.name,
            )
        };
        ResolveError::Message(body.into())
    }
}

fn declarations(target: &Target) -> impl Iterator<Item = (Kind, BString)> + '_ {
    Kind::ALL.into_iter().flat_map(|kind| {
        target
            .props
            .get(kind.declaration().as_bytes())
            .into_iter()
            .flat_map(move |value| {
                list::split(value, list::Empty::Drop)
                    .into_iter()
                    .map(move |name| (kind, name))
            })
    })
}

fn number(value: &[u8]) -> Option<i64> {
    let value = core::str::from_utf8(value)
        .ok()?
        .trim_start_matches(|ch: char| ch.is_ascii_whitespace());
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (digits, radix) = if let Some(digits) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        (digits, 16)
    } else if unsigned.starts_with('0') {
        (unsigned, 8)
    } else {
        (unsigned, 10)
    };
    if !digits.as_bytes().first()?.is_ascii_hexdigit() {
        return None;
    }
    let magnitude = u64::from_str_radix(digits, radix).ok()?;
    let signed = if value.starts_with('-') {
        -i128::from(magnitude)
    } else {
        i128::from(magnitude)
    };
    i64::try_from(signed).ok()
}
