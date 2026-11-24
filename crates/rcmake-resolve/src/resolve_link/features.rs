use std::collections::HashMap;

use rcmake_base::{BString, list, truth};
use rcmake_model::{ProjectModel, Target};

use super::{LinkFragment, LinkFragmentRole, ResolvedLinkItem, ResolvedLinkKind, link_fragment};
use crate::{ResolveError, lower, sources::SourcePaths};

#[derive(Clone)]
struct Definition {
    prefix: BString,
    suffix: BString,
    template: Option<BString>,
}

pub(super) struct Features<'a, 'host> {
    model: &'a ProjectModel,
    target: &'a Target,
    paths: &'a SourcePaths<'host>,
    language: &'a BString,
    definitions: HashMap<(bool, BString), Definition>,
}

impl<'a, 'host> Features<'a, 'host> {
    pub(super) fn new(
        model: &'a ProjectModel,
        target: &'a Target,
        paths: &'a SourcePaths<'host>,
        language: &'a BString,
    ) -> Self {
        Self {
            model,
            target,
            paths,
            language,
            definitions: HashMap::new(),
        }
    }

    pub(super) fn append<'item>(
        &mut self,
        items: impl IntoIterator<Item = &'item ResolvedLinkItem>,
        fragments: &mut Vec<LinkFragment>,
        plain: &mut impl FnMut(&ResolvedLinkItem, &mut Vec<LinkFragment>),
    ) -> Result<(), ResolveError> {
        let mut library = None::<(&ResolvedLinkItem, Definition)>;
        for item in items {
            let feature = item
                .feature
                .as_ref()
                .filter(|name| name.as_slice() != b"DEFAULT");
            if library
                .as_ref()
                .is_some_and(|(first, _)| first.feature.as_ref() != feature)
                && let Some((first, definition)) = library.take()
            {
                push(fragments, &definition.suffix, first);
            }
            if let ResolvedLinkKind::Group { feature, members } = &item.kind {
                let definition = self.definition(true, feature)?;
                push(fragments, &definition.prefix, item);
                self.append(members.iter(), fragments, plain)?;
                push(fragments, &definition.suffix, item);
                continue;
            }
            if let Some(feature) = feature
                && library.is_none()
            {
                let definition = self.definition(false, feature)?;
                push(fragments, &definition.prefix, item);
                library = Some((item, definition));
            }
            let start = fragments.len();
            plain(item, fragments);
            if let Some(template) = library
                .as_ref()
                .and_then(|(_, definition)| definition.template.as_ref())
            {
                for fragment in fragments
                    .iter_mut()
                    .skip(start)
                    .filter(|fragment| fragment.role == LinkFragmentRole::Libraries)
                {
                    let value = &fragment.text.value;
                    let use_path =
                        value.contains(&b'/') || matches!(item.kind, ResolvedLinkKind::Library(_));
                    let template =
                        alternative(template, if use_path { b"PATH{" } else { b"NAME{" });
                    fragment.text.value = lower::expand_rule(template, |key| match key {
                        b"LINK_ITEM" | b"LIBRARY" | b"LIB_ITEM" => Some(value.clone()),
                        _ => None,
                    });
                }
            }
        }
        if let Some((item, definition)) = library {
            push(fragments, &definition.suffix, item);
        }
        Ok(())
    }

    fn definition(&mut self, group: bool, feature: &BString) -> Result<Definition, ResolveError> {
        let key = (group, feature.clone());
        if let Some(definition) = self.definitions.get(&key) {
            return Ok(definition.clone());
        }
        let operator = if group { "LINK_GROUP" } else { "LINK_LIBRARY" };
        let specific = format!("CMAKE_{}_{operator}_USING_{feature}", self.language);
        let generic = format!("CMAKE_{operator}_USING_{feature}");
        let find = |key: &str| lower::definition(self.model, self.target.dir, key.as_bytes());
        let supports =
            |key: &str| find(&format!("{key}_SUPPORTED")).is_some_and(|value| truth::is_on(value));
        let target = &self.target.name;
        let language = self.language;
        let describe = |reason| {
            self.error(format!(
                "Feature '{feature}', specified through generator-expression '$<{operator}>' to link target '{target}', is not {reason} for the '{language}' link language."
            ))
        };
        let variable = [specific, generic]
            .into_iter()
            .find(|name| supports(name))
            .ok_or_else(|| describe("supported"))?;
        let value = find(&variable).ok_or_else(|| describe("defined"))?;
        let mut elements = list::split(value, list::Empty::Keep).into_iter();
        let malformed = |reason| {
            self.error(format!(
            "Feature '{feature}', specified by variable '{variable}', is malformed ({reason}) and cannot be used to link target '{target}'."
        ))
        };
        let definition = match (group, elements.len()) {
            (true, 2) => Definition {
                prefix: elements.next().unwrap_or_default(),
                suffix: elements.next().unwrap_or_default(),
                template: None,
            },
            (false, 1 | 3) => {
                let prefix = if elements.len() == 3 {
                    elements.next().unwrap_or_default()
                } else {
                    BString::default()
                };
                let template = elements.next().unwrap_or_default();
                validate_template(&template).map_err(malformed)?;
                Definition {
                    prefix,
                    suffix: elements.next().unwrap_or_default(),
                    template: Some(template),
                }
            }
            _ => return Err(malformed("wrong number of elements")),
        };
        self.definitions.insert(key, definition.clone());
        Ok(definition)
    }

    fn error(&self, body: String) -> ResolveError {
        self.paths
            .target_error(self.model, self.target, body.into())
    }
}

fn push(fragments: &mut Vec<LinkFragment>, text: &BString, item: &ResolvedLinkItem) {
    if !text.is_empty() {
        fragments.push(link_fragment(
            text.clone(),
            &item.text,
            LinkFragmentRole::Libraries,
        ));
    }
}

fn validate_template(value: &[u8]) -> Result<(), &'static str> {
    let has_placeholder = |template: &[u8]| {
        [b"<LIBRARY>".as_slice(), b"<LIB_ITEM>", b"<LINK_ITEM>"]
            .iter()
            .any(|placeholder| {
                template
                    .windows(placeholder.len())
                    .any(|token| token == *placeholder)
            })
    };
    if !has_placeholder(alternative(value, b"PATH{")) {
        return Err(if value.windows(5).any(|token| token == b"PATH{") {
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing for \"PATH{}\" alternative"
        } else {
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing"
        });
    }
    if !has_placeholder(alternative(value, b"NAME{")) {
        return Err(
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing for \"NAME{}\" alternative",
        );
    }
    Ok(())
}

fn alternative<'a>(template: &'a [u8], marker: &[u8]) -> &'a [u8] {
    template
        .windows(marker.len())
        .position(|bytes| bytes == marker)
        .and_then(|start| template.get(start + marker.len()..))
        .and_then(|tail| {
            tail.iter()
                .position(|&byte| byte == b'}')
                .and_then(|end| tail.get(..end))
        })
        .unwrap_or(template)
}
