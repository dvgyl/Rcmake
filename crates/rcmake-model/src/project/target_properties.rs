use rcmake_base::{BString, list};
use rcmake_diag::Backtrace;

use super::props::{set_map, set_usage};
use super::{ModelError, ProjectModel};
use crate::{BT, DirId, LinkItem, PropOrigin, Target, TargetId, UsageField};

pub(super) fn is_scoped_link(name: &[u8]) -> bool {
    matches!(name, b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES")
}

pub(super) fn is_direct_link(name: &[u8]) -> bool {
    matches!(
        name,
        b"INTERFACE_LINK_LIBRARIES_DIRECT" | b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE"
    )
}

pub(super) fn is_tracked_wrapper(name: &[u8]) -> bool {
    matches!(
        name,
        b"COMPILE_WRAPPERS"
            | b"INTERFACE_COMPILE_WRAPPERS"
            | b"LINK_WRAPPERS"
            | b"INTERFACE_LINK_WRAPPERS"
    )
}

pub(super) fn set_target_prop(
    target: &mut Target,
    name: &[u8],
    value: Option<BString>,
    origin: PropOrigin<'_>,
) -> Result<(), ModelError> {
    let entry = |v| BT::new(v, origin.backtrace.clone());
    match name {
        b"NAME" | b"TYPE" | b"IMPORTED" | b"SOURCE_DIR" | b"BINARY_DIR" => {
            return Err(ModelError::ReadOnly);
        }
        b"SOURCES" => target.sources = value.into_iter().map(entry).collect(),
        _ => match UsageField::from_name(name) {
            Some(field) => set_usage(&mut target.usage, field, value, origin.backtrace),
            None => {
                if let Some(entries) = target.direct_link_slot(name) {
                    *entries = value.clone().map(|value| vec![entry(value)]);
                }
                if matches!(
                    name,
                    b"C_STANDARD"
                        | b"CXX_STANDARD"
                        | b"CUDA_STANDARD"
                        | b"HIP_STANDARD"
                        | b"OBJC_STANDARD"
                        | b"OBJCXX_STANDARD"
                        | b"SYCL_CXX_STANDARD"
                ) {
                    if value.is_some() {
                        target
                            .standard_origins
                            .insert(name.into(), origin.backtrace.clone());
                    } else {
                        target.standard_origins.shift_remove(name);
                    }
                }
                if is_tracked_wrapper(name) {
                    match value {
                        Some(value) => {
                            target
                                .props
                                .set_tracked(name.into(), value, origin.backtrace.clone());
                        }
                        None => target.props.unset(name),
                    }
                } else {
                    set_map(&mut target.props, name, value);
                }
            }
        },
    }
    Ok(())
}

impl Target {
    /// Return the origin of a language standard property write or initializer.
    #[must_use]
    pub fn standard_origin(&self, property: &[u8]) -> &Backtrace {
        self.standard_origins
            .get(property)
            .unwrap_or(&self.backtrace)
    }

    /// Return the recorded entries for a direct link interface property.
    #[must_use]
    pub fn direct_link_entries(&self, name: &[u8]) -> Option<&[BT<BString>]> {
        match name {
            b"INTERFACE_LINK_LIBRARIES_DIRECT" => self.direct_links.as_deref(),
            b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE" => self.direct_excludes.as_deref(),
            _ => None,
        }
    }

    fn direct_link_slot(&mut self, name: &[u8]) -> Option<&mut Option<Vec<BT<BString>>>> {
        match name {
            b"INTERFACE_LINK_LIBRARIES_DIRECT" => Some(&mut self.direct_links),
            b"INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE" => Some(&mut self.direct_excludes),
            _ => None,
        }
    }

    pub(super) fn append_direct_link(&mut self, name: &[u8], value: &[u8], origin: &Backtrace) {
        let fallback = self
            .props
            .get(name)
            .cloned()
            .map(|value| BT::new(value, self.backtrace.clone()));
        if let Some(slot) = self.direct_link_slot(name) {
            let entries = slot.get_or_insert_with(|| fallback.into_iter().collect());
            entries.push(BT::new(value.into(), origin.clone()));
            let joined = list::join(entries.iter().map(|entry| &entry.value));
            self.props.set(name.into(), joined);
        }
    }
}

impl ProjectModel {
    pub(super) fn write_link_property(
        &mut self,
        id: TargetId,
        name: &[u8],
        value: Option<BString>,
        origin: PropOrigin<'_>,
        append: bool,
    ) -> Result<(), ModelError> {
        let owner = self.target(id).ok_or(ModelError::UnknownObject)?.dir;
        let items = value.map(|value| self.link_property_entries(&value, owner, origin));
        let target = self.target_mut(id).ok_or(ModelError::UnknownObject)?;
        let slot = if name == b"LINK_LIBRARIES" {
            &mut target.usage.link_libraries
        } else {
            &mut target.usage.interface_link_libraries
        };
        if append {
            slot.get_or_insert_default()
                .extend(items.into_iter().flatten());
        } else {
            *slot = items;
        }
        Ok(())
    }

    fn link_property_entries(
        &self,
        value: &[u8],
        owner: DirId,
        origin: PropOrigin<'_>,
    ) -> Vec<LinkItem> {
        let item = |text: &[u8], dir| LinkItem {
            opens_lookup_scope: false,
            closes_lookup_scope: false,
            text: BT::new(text.into(), origin.backtrace.clone()),
            lookup_dir: dir,
            scope: None,
        };
        if value.is_empty() {
            return Vec::new();
        }
        if !value.windows(3).any(|bytes| bytes == b"::@") {
            return vec![item(value, owner)];
        }
        let marker = |text: &[u8]| -> Option<DirId> {
            let digits = text.strip_prefix(b"::@(0x")?.strip_suffix(b")")?;
            let index = usize::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
            let id = DirId::from_index(index)?;
            (index <= self.subdirs.len() && text == format!("::@(0x{index:x})").as_bytes())
                .then_some(id)
        };
        let split = link_elements(value);
        if !split
            .iter()
            .any(|text| *text == b"::@" || marker(text).is_some())
        {
            return vec![item(value, owner)];
        }
        let mut dir = owner;
        split
            .into_iter()
            .map(|text| {
                if text == b"::@" {
                    dir = owner;
                    LinkItem {
                        closes_lookup_scope: true,
                        ..item(b"", dir)
                    }
                } else if let Some(scope) = marker(text) {
                    dir = scope;
                    LinkItem {
                        opens_lookup_scope: true,
                        ..item(b"", dir)
                    }
                } else {
                    item(text, dir)
                }
            })
            .collect()
    }
}

// Preserve raw bytes and ignore separators inside generator expressions.
fn link_elements(value: &[u8]) -> Vec<&[u8]> {
    let mut elements = Vec::new();
    let mut start = 0;
    let mut genex = 0_usize;
    let mut brackets = 0_isize;
    let mut bytes = value.iter().enumerate().peekable();
    while let Some((index, &byte)) = bytes.next() {
        match byte {
            b'\\' if bytes.peek().is_some_and(|(_, next)| **next == b';') => {
                bytes.next();
            }
            b'$' if bytes.peek().is_some_and(|(_, next)| **next == b'<') => {
                genex += 1;
                bytes.next();
            }
            b'>' if genex > 0 => genex -= 1,
            b'[' if genex == 0 => brackets += 1,
            b']' if genex == 0 => brackets -= 1,
            b';' if genex == 0 && brackets == 0 => {
                elements.extend(value.get(start..index));
                start = index + 1;
            }
            _ => {}
        }
    }
    elements.extend(value.get(start..));
    elements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppendMode, ImportedVisibility, PropScope, TargetKind, TargetOrigin};

    #[test]
    fn unmarked_links_use_the_target_directory() -> Result<(), ModelError> {
        let mut model = ProjectModel::default();
        let trace = Backtrace::default();
        let child = model.add_directory(
            DirId::ROOT,
            "/source/sub".into(),
            "/build/sub".into(),
            trace.clone(),
        )?;
        let create = |name: &str, dir, origin| {
            Target::new(
                name.into(),
                TargetKind::InterfaceLibrary,
                origin,
                dir,
                trace.clone(),
                Default::default(),
            )
        };
        let owner = model.add_target(create("owner", DirId::ROOT, TargetOrigin::Normal))?;
        let dep = model.add_target(create(
            "dep",
            child,
            TargetOrigin::Imported(ImportedVisibility::Local),
        ))?;
        assert_eq!(model.find_target(child, b"dep"), Some(dep));
        model.append_prop(
            &PropScope::Target(owner),
            b"INTERFACE_LINK_LIBRARIES",
            b"dep",
            AppendMode::List,
            PropOrigin {
                dir: child,
                backtrace: &trace,
            },
        )?;
        let target = model.target(owner).ok_or(ModelError::UnknownObject)?;
        let items = target
            .usage
            .interface_link_libraries
            .as_deref()
            .unwrap_or_default();
        let [item] = items else {
            return Err(ModelError::UnknownObject);
        };
        assert_eq!(item.lookup_dir, DirId::ROOT);
        assert!(model.find_target(item.lookup_dir, b"dep").is_none());
        Ok(())
    }
}
