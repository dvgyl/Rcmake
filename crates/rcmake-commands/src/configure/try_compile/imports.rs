use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rcmake_base::{BString, list, truth};
use rcmake_diag::Backtrace;
use rcmake_eval::{CmdError, Cx};
use rcmake_genex::{
    ConfigId, EvalContext, EvalSite, TargetHandle, UsageTraversal, evaluate_with_target_lookups,
    parse,
};
use rcmake_model::{DirId, LinkItem, ProjectModel, Target, TargetId, TargetKind, TargetOrigin};
use rcmake_resolve::usage::UsageResolver;

use super::{Options, TryCompileSeed, probe_error};

const INTERFACE_LINKS: &[u8] = b"INTERFACE_LINK_LIBRARIES";

pub(super) fn validate_direct_links(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
) -> Result<(), CmdError> {
    let dir = cx.current_dir_id();
    let libraries = options.many(b"LINK_LIBRARIES");
    for &name in libraries {
        let model = cx.project();
        let Some(target) = model.find_target(dir, name).and_then(|id| model.target(id)) else {
            continue;
        };
        let valid = match target.kind {
            TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::InterfaceLibrary
            | TargetKind::UnknownLibrary => true,
            TargetKind::Executable => target
                .props
                .get(b"ENABLE_EXPORTS")
                .is_some_and(|value| truth::is_on(value)),
            _ => false,
        };
        if !valid {
            let kind = target.kind.as_str();
            return Err(probe_error(
                cx,
                format!(
                    "Only libraries may be used as try_compile or try_run IMPORTED LINK_LIBRARIES.  Got {} of type {kind}.",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
    }
    Ok(())
}

pub(super) fn discover(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    seed: &TryCompileSeed,
    languages: &[BString],
    probe_name: &[u8],
    run: bool,
) -> Result<Vec<(Target, Vec<BString>)>, CmdError> {
    let dir = cx.current_dir_id();
    let libraries = options.many(b"LINK_LIBRARIES");
    if libraries.is_empty() {
        return Ok(Vec::new());
    }
    // `cmCoreTryCompile` evaluates the imported target properties for the
    // `CMAKE_TRY_COMPILE_CONFIGURATION` configuration, not for `CMAKE_FLAGS` values.
    let config = ConfigId(cx.var_or_empty(b"CMAKE_TRY_COMPILE_CONFIGURATION"));
    let kind = if !run && cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE") == b"STATIC_LIBRARY" {
        TargetKind::StaticLibrary
    } else {
        TargetKind::Executable
    };
    let backtrace = cx.backtrace();
    let mut model = cx.project().clone();
    let mut probe = Target::new(
        probe_name.into(),
        kind,
        TargetOrigin::Normal,
        dir,
        backtrace,
        Default::default(),
    );
    if let Some(linker) = options.one(b"LINKER_LANGUAGE") {
        probe.props.set("LINKER_LANGUAGE".into(), linker.into());
    }
    let head = model
        .add_target(probe)
        .map_err(|error| CmdError::status(error.to_string()))?;
    let mut link_languages: Vec<_> = options
        .one(b"LINKER_LANGUAGE")
        .map(|name| vec![name.into()])
        .unwrap_or_else(|| list::split(&seed.enabled_languages, list::Empty::Drop));
    if link_languages.is_empty() {
        link_languages.extend_from_slice(languages);
    }
    let mut walker = Walker {
        model: &model,
        resolver: UsageResolver::new(&model),
        ids: model.targets().map(|(id, _)| id).collect(),
        pending: VecDeque::new(),
        queued: BTreeSet::new(),
        config,
        head: handle(head),
        languages,
        link_languages: &link_languages,
        remaining: cx.limits().max_value_bytes,
    };
    // `FindTargetToUse` matches each direct item by its literal name, so a
    // generator expression in `LINK_LIBRARIES` never names an imported target.
    for &name in libraries {
        walker.charge(name.len())?;
        if let Some(id) = model.find_target(dir, name) {
            walker.push(id, name.into())?;
        }
    }
    let mut targets = BTreeMap::<TargetId, (Target, Vec<BString>)>::new();
    while let Some((id, name)) = walker.pending.pop_front() {
        let Some(target) = model.target(id) else {
            continue;
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = targets.entry(id) {
            walker.follow(id, target)?;
            entry.insert((remap(target), Vec::new()));
        }
        if name != target.name
            && let Some((_, aliases)) = targets.get_mut(&id)
        {
            aliases.push(name);
        }
    }
    Ok(targets.into_values().collect())
}

fn handle(id: TargetId) -> TargetHandle {
    TargetHandle(id.index() as u32)
}

fn marker(item: &LinkItem) -> bool {
    item.text.value.is_empty() && (item.opens_lookup_scope || item.closes_lookup_scope)
}

fn remap(target: &Target) -> Target {
    let mut target = target.clone();
    target.dir = DirId::ROOT;
    for items in [
        &mut target.usage.link_libraries,
        &mut target.usage.interface_link_libraries,
    ]
    .into_iter()
    .flatten()
    {
        items.retain(|item| !marker(item));
        for item in items {
            item.lookup_dir = DirId::ROOT;
            item.opens_lookup_scope = false;
            item.closes_lookup_scope = false;
        }
    }
    target
}

struct Walker<'a> {
    model: &'a ProjectModel,
    resolver: UsageResolver<'a>,
    ids: Vec<TargetId>,
    pending: VecDeque<(TargetId, BString)>,
    queued: BTreeSet<(TargetId, BString)>,
    config: ConfigId,
    head: TargetHandle,
    languages: &'a [BString],
    link_languages: &'a [BString],
    remaining: usize,
}

impl Walker<'_> {
    fn follow(&mut self, id: TargetId, target: &Target) -> Result<(), CmdError> {
        for item in target
            .usage
            .interface_link_libraries
            .iter()
            .flatten()
            .filter(|item| !marker(item))
        {
            self.evaluate(
                item.lookup_dir,
                id,
                INTERFACE_LINKS,
                &item.text.value,
                &item.text.backtrace,
                true,
            )?;
        }
        for (field, entries) in &target.usage.fields {
            if field.name().starts_with("INTERFACE_") {
                for entry in entries {
                    self.evaluate(
                        target.dir,
                        id,
                        field.name().as_bytes(),
                        &entry.value,
                        &entry.backtrace,
                        false,
                    )?;
                }
            }
        }
        for key in target.props.names() {
            let link = key == INTERFACE_LINKS
                || key.starts_with(b"INTERFACE_LINK_LIBRARIES_DIRECT")
                || key.starts_with(b"IMPORTED_LINK_INTERFACE_LIBRARIES")
                || key.starts_with(b"IMPORTED_LINK_DEPENDENT_LIBRARIES");
            if (link || key.starts_with(b"INTERFACE_"))
                && let Some(value) = target.props.get(&key)
            {
                self.evaluate(target.dir, id, &key, value, &target.backtrace, link)?;
            }
        }
        Ok(())
    }

    fn evaluate(
        &mut self,
        dir: DirId,
        id: TargetId,
        property: &[u8],
        value: &[u8],
        trace: &Backtrace,
        link: bool,
    ) -> Result<(), CmdError> {
        self.charge(value.len())?;
        if !value.windows(2).any(|bytes| bytes == b"$<") {
            if link {
                self.names(dir, value)?;
            }
            return Ok(());
        }
        let Ok(ast) = parse(value) else { return Ok(()) };
        let current = Some(handle(id));
        let mut ctx = EvalContext {
            config: self.config.clone(),
            head: Some(self.head),
            current,
            site: EvalSite {
                backtrace: trace.clone(),
                property: Some(property.into()),
            },
            quiet: true,
            ..Default::default()
        };
        let link_use = link || property.starts_with(b"INTERFACE_LINK_");
        for link_language in std::iter::once(None).chain(self.link_languages.iter().map(Some)) {
            ctx.link_language = link_language.cloned();
            for language in std::iter::once(None).chain(self.languages.iter().map(Some)) {
                ctx.language = language.cloned();
                ctx.traversal = if language.is_none() && link_use {
                    UsageTraversal::Link
                } else {
                    UsageTraversal::Compile
                };
                self.charge(value.len())?;
                let mut seen = Vec::new();
                let mut host = self.resolver.host_at(dir, current);
                let result = evaluate_with_target_lookups(&ast, &ctx, &mut host, &mut seen);
                for (name, handle) in seen {
                    if let Some(&id) = self.ids.get(handle.0 as usize) {
                        self.push(id, name)?;
                    }
                }
                if link && let Ok((out, _)) = result {
                    self.names(dir, &out)?;
                }
            }
        }
        Ok(())
    }

    fn names(&mut self, dir: DirId, value: &[u8]) -> Result<(), CmdError> {
        self.charge(value.len())?;
        for name in list::split(value, list::Empty::Drop) {
            if let Some(id) = self.model.find_target(dir, &name) {
                self.push(id, name)?;
            }
        }
        Ok(())
    }

    fn push(&mut self, id: TargetId, name: BString) -> Result<(), CmdError> {
        if self.model.target(id).is_some_and(Target::is_imported)
            && self.queued.insert((id, name.clone()))
        {
            self.charge(name.len())?;
            self.pending.push_back((id, name));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), CmdError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.saturating_add(64))
            .ok_or_else(|| {
                CmdError::status("try_compile imported target discovery exceeded its limit.")
            })?;
        Ok(())
    }
}
