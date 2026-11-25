//! Reject internal link tags that a project writes in link library properties.
//!
//! `$<LINK_LIBRARY:...>` and `$<LINK_GROUP:...>` expand to internal tags such as
//! `<LINK_LIBRARY:feature>`. CMake rejects a literal tag in the raw value of
//! `LINK_LIBRARIES`, `INTERFACE_LINK_LIBRARIES`, and `INTERFACE_LINK_LIBRARIES_DIRECT`
//! before it generates the build system.

use rcmake_base::BString;
use rcmake_diag::{Location, MessageKind};
use rcmake_host::Host;
use rcmake_model::{BT, ProjectModel, TargetKind};

use crate::{ResolveError, sources::SourcePaths};

/// Report each property entry that contains a literal link tag.
///
/// As in CMake, the check of one target stops after the first property with an error.
pub(super) fn validate(model: &ProjectModel, host: &dyn Host) -> Result<(), ResolveError> {
    let paths = SourcePaths::new(host);
    let mut failure = None;
    for (_, target) in model
        .targets()
        .filter(|(_, target)| !target.is_imported() && target.kind != TargetKind::GlobalTarget)
    {
        let entries = |items: &Option<Vec<rcmake_model::LinkItem>>| {
            items
                .iter()
                .flatten()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>()
        };
        let direct = target
            .props
            .get(b"INTERFACE_LINK_LIBRARIES_DIRECT")
            .map(|value| BT::new(value.clone(), target.backtrace.clone()));
        let properties = [
            ("LINK_LIBRARIES", entries(&target.usage.link_libraries)),
            (
                "INTERFACE_LINK_LIBRARIES",
                entries(&target.usage.interface_link_libraries),
            ),
            (
                "INTERFACE_LINK_LIBRARIES_DIRECT",
                direct.into_iter().collect(),
            ),
        ];
        for (name, entries) in properties {
            let mut failed = false;
            for entry in &entries {
                let Some((tag, kind)) = link_tag(&entry.value) else {
                    continue;
                };
                let body: BString = format!(
                    "Property {name} contains the invalid item \"{}\".  The {name} property may contain the generator-expression \"$<LINK_{kind}:...>\" which may be used to specify how the libraries are linked.",
                    BString::from(tag)
                )
                .into();
                paths.emit(
                    model,
                    MessageKind::Error,
                    Location::Backtrace(entry.backtrace.clone()),
                    body.clone(),
                );
                failure.get_or_insert(body);
                failed = true;
            }
            if failed {
                break;
            }
        }
    }
    failure.map_or(Ok(()), |body| Err(ResolveError::Reported(body)))
}

/// Find the first element `<LINK_LIBRARY:...>`, `</LINK_LIBRARY:...>`,
/// `<LINK_GROUP:...>`, or `</LINK_GROUP:...>` and return it with its kind.
fn link_tag(value: &[u8]) -> Option<(&[u8], &'static str)> {
    value.split(|&byte| byte == b';').find_map(|element| {
        let rest = element
            .strip_prefix(b"</")
            .or_else(|| element.strip_prefix(b"<"))?
            .strip_prefix(b"LINK_")?;
        let (kind, rest) = if let Some(rest) = rest.strip_prefix(b"LIBRARY:") {
            ("LIBRARY", rest)
        } else {
            ("GROUP", rest.strip_prefix(b"GROUP:")?)
        };
        let body = rest.strip_suffix(b">")?;
        (!body.contains(&b'>')).then_some((element, kind))
    })
}

#[cfg(test)]
mod tests {
    use super::link_tag;

    #[test]
    fn finds_only_literal_link_tags() {
        assert_eq!(
            link_tag(b"foo;</LINK_LIBRARY:feat>"),
            Some((b"</LINK_LIBRARY:feat>".as_slice(), "LIBRARY"))
        );
        assert_eq!(
            link_tag(b"<LINK_GROUP:feat>"),
            Some((b"<LINK_GROUP:feat>".as_slice(), "GROUP"))
        );
        assert_eq!(link_tag(b"$<LINK_LIBRARY:feat,foo>"), None);
        assert_eq!(link_tag(b"<LINK_LIBRARY:a>b>"), None);
        assert_eq!(link_tag(b"<LINK_OTHER:feat>"), None);
    }
}
