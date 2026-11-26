use rcmake_base::{BString, list};
use rcmake_diag::Backtrace;

use super::{ModelError, ProjectModel, target_properties};
use crate::{
    AppendMode, BT, CacheEntry, CacheType, PropOrigin, PropScope, Target, TargetId, UsageField,
    UsageProps,
};

impl ProjectModel {
    fn record_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        op: crate::PropOp,
        old: Option<BString>,
        origin: PropOrigin<'_>,
    ) {
        if self.prop_ledger.is_none() {
            return;
        }
        let new = self.get_prop(scope, name);
        if let Some(ledger) = &mut self.prop_ledger {
            self.prop_seq = self.prop_seq.saturating_add(1);
            ledger.record(
                scope,
                name,
                crate::PropEvent::new(
                    self.prop_seq,
                    op,
                    old.as_ref().map(|v| v.as_slice()),
                    new.as_ref().map(|v| v.as_slice()),
                    origin.backtrace,
                ),
            );
        }
    }

    /// Update target properties with their native storage rules and record the writes.
    pub fn update_target_properties<T>(
        &mut self,
        id: TargetId,
        append: impl IntoIterator<Item = (&'static [u8], AppendMode)>,
        origin: PropOrigin<'_>,
        update: impl FnOnce(&mut Target) -> T,
    ) -> Result<T, ModelError> {
        let scope = PropScope::Target(id);
        let previous: Vec<_> = if self.prop_ledger.is_some() {
            append
                .into_iter()
                .map(|(name, mode)| (name, mode, self.get_prop(&scope, name)))
                .collect()
        } else {
            Vec::new()
        };
        let result = update(self.target_mut(id).ok_or(ModelError::UnknownObject)?);
        for (name, mode, value) in previous {
            self.record_prop(&scope, name, crate::PropOp::Append(mode), value, origin);
        }
        Ok(result)
    }

    /// Set (`Some`) or unset (`None`) property `name` on `scope`.
    pub fn set_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: Option<BString>,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        let old = self
            .prop_ledger
            .as_ref()
            .and_then(|_| self.get_prop(scope, name));
        let op = if value.is_some() {
            crate::PropOp::Set
        } else {
            crate::PropOp::Unset
        };
        self.set_prop_value(scope, name, value, origin)?;
        self.record_prop(scope, name, op, old, origin);
        Ok(())
    }

    fn set_prop_value(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: Option<BString>,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        match scope {
            PropScope::Global => {
                set_map(&mut self.global.props, name, value);
                Ok(())
            }
            PropScope::Directory(id) => {
                let dir = self.dir_mut(*id);
                if name == b"EXCLUDE_FROM_ALL" {
                    dir.exclude_from_all = value
                        .as_ref()
                        .is_some_and(|value| rcmake_base::truth::is_on(value));
                }
                match UsageField::from_name(name).filter(|f| f.on_directory()) {
                    Some(field) => set_usage(&mut dir.usage, field, value, origin.backtrace),
                    None => set_map(&mut dir.props, name, value),
                }
                Ok(())
            }
            PropScope::Target(id) => {
                if target_properties::is_scoped_link(name) {
                    return self.write_link_property(*id, name, value, origin, false);
                }
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                target_properties::set_target_prop(target, name, value, origin)
            }
            PropScope::Source { dir, path } => {
                let props = self
                    .dir_mut(*dir)
                    .source_props
                    .entry(path.clone())
                    .or_default();
                set_map(props, name, value);
                Ok(())
            }
            PropScope::Test { dir, name: test } => {
                let id = self
                    .find_test(*dir, test)
                    .ok_or(ModelError::UnknownObject)?;
                let test = self.test_mut(id).ok_or(ModelError::UnknownObject)?;
                set_map(&mut test.props, name, value);
                Ok(())
            }
            PropScope::Cache(entry) => {
                let entry = self.cache.get_mut(entry).ok_or(ModelError::UnknownObject)?;
                set_cache_prop(entry, name, value)
            }
            PropScope::FileSet(id) => {
                let file_set = self.file_set_mut(*id).ok_or(ModelError::UnknownObject)?;
                set_map(&mut file_set.props, name, value);
                Ok(())
            }
            PropScope::Install { file } => {
                if let Some(value) = value {
                    self.install_props
                        .entry(file.clone())
                        .or_default()
                        .set(name.into(), value);
                } else if let Some(props) = self.install_props.get_mut(file) {
                    props.unset(name);
                    if props.names().is_empty() {
                        self.install_props.remove(file);
                    }
                }
                Ok(())
            }
        }
    }

    /// Append `value` to property `name` on `scope` (`set_property(APPEND)` or
    /// `APPEND_STRING`). Keep empty entries for target `SOURCES`.
    pub fn append_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: &[u8],
        mode: AppendMode,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        let target_sources = matches!(scope, PropScope::Target(_)) && name == b"SOURCES";
        if value.is_empty() && !target_sources {
            return Ok(());
        }
        let previous = self
            .prop_ledger
            .as_ref()
            .and_then(|_| self.get_prop(scope, name));
        if let PropScope::Target(id) = scope {
            if target_properties::is_scoped_link(name) {
                self.write_link_property(*id, name, Some(value.into()), origin, true)?;
                self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
                return Ok(());
            }
            if target_properties::is_direct_link(name) {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                target.append_direct_link(name, value, origin.backtrace);
                self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
                return Ok(());
            }
            if target_properties::is_tracked_wrapper(name) {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                target
                    .props
                    .append_tracked(name.into(), value, origin.backtrace.clone());
                self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
                return Ok(());
            }
        }
        if (target_sources || mode == AppendMode::List)
            && self.append_entry(scope, name, value, origin)?
        {
            self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
            return Ok(());
        }
        let old = match scope {
            PropScope::Source { dir, path } if name == b"GENERATED" => self
                .dir(*dir)
                .source_props
                .get(path)
                .and_then(|props| props.get(name))
                .cloned(),
            _ => self.get_prop(scope, name),
        };
        let new = match (old, mode) {
            (Some(old), AppendMode::List) if !old.is_empty() => list::join([old.as_slice(), value]),
            (Some(old), AppendMode::String) => [old.as_slice(), value].concat().into(),
            _ => BString::from(value),
        };
        self.set_prop_value(scope, name, Some(new), origin)?;
        self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
        Ok(())
    }

    /// Append one backtrace-tagged entry to a usage, `SOURCES`, or link property.
    /// Returns `false` when `name` is a plain property.
    fn append_entry(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: &[u8],
        origin: PropOrigin<'_>,
    ) -> Result<bool, ModelError> {
        let entry = BT::new(BString::from(value), origin.backtrace.clone());
        let field = UsageField::from_name(name);
        match scope {
            PropScope::Directory(id) => match field.filter(|f| f.on_directory()) {
                Some(field) => self.dir_mut(*id).usage.push(field, entry),
                None => return Ok(false),
            },
            PropScope::Target(id) => {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                let usage = &mut target.usage;
                match (field, name) {
                    (Some(field), _) => usage.push(field, entry),
                    (None, b"SOURCES") => target.sources.push(entry),
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

pub(super) fn set_map(props: &mut crate::PropertyMap, name: &[u8], value: Option<BString>) {
    match value {
        Some(value) => props.set(name.into(), value),
        None => props.unset(name),
    }
}

pub(super) fn set_usage(
    usage: &mut UsageProps,
    field: UsageField,
    value: Option<BString>,
    bt: &Backtrace,
) {
    match value {
        Some(value) => {
            usage.fields.insert(field, Vec::new());
            usage.push(field, BT::new(value, bt.clone()));
        }
        None => {
            usage.fields.shift_remove(&field);
        }
    }
}

fn set_cache_prop(
    entry: &mut CacheEntry,
    name: &[u8],
    value: Option<BString>,
) -> Result<(), ModelError> {
    match name {
        b"VALUE" => entry.value = value.unwrap_or_default(),
        b"HELPSTRING" => entry.help = value.unwrap_or_default(),
        b"TYPE" => {
            let value = value.unwrap_or_default();
            entry.ty = CacheType::parse(&value)
                .filter(|ty| ty.as_str().as_bytes() == value.as_slice())
                .ok_or(ModelError::InvalidValue)?;
        }
        b"ADVANCED" => {
            entry.advanced = value.as_ref().is_some_and(|v| rcmake_base::truth::is_on(v));
            set_map(&mut entry.props, name, value);
        }
        _ => set_map(&mut entry.props, name, value),
    }
    Ok(())
}
