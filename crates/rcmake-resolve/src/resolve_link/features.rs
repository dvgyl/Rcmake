use std::collections::HashMap;

use rcmake_base::{BString, ByteSlice, list, shell::quote, truth};
use rcmake_model::{ProjectModel, Target};

use super::{LinkFragment, LinkFragmentRole, ResolvedLinkItem, ResolvedLinkKind, link_fragment};
use crate::{ResolveError, lower, sources::SourcePaths, usage::link_options::Wrapper};

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
    wrapper: Wrapper<'a>,
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
            wrapper: Wrapper::new(model, target, language, b"LINKER"),
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
                self.push(fragments, &definition.suffix, first)?;
            }
            if let ResolvedLinkKind::Group { feature, members } = &item.kind {
                let definition = self.definition(true, feature)?;
                self.push(fragments, &definition.prefix, item)?;
                self.append(members.iter(), fragments, plain)?;
                self.push(fragments, &definition.suffix, item)?;
                continue;
            }
            if let Some(feature) = feature
                && library.is_none()
            {
                let definition = self.definition(false, feature)?;
                self.push(fragments, &definition.prefix, item)?;
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
                    // A feature names a framework by its bundle name.
                    let framework = value
                        .strip_prefix(b"-framework ")
                        .map(|name| name.strip_suffix(b".framework").unwrap_or(name))
                        .or_else(|| match item.kind {
                            ResolvedLinkKind::Library(_) => {
                                crate::lower::framework_bundle_name(value)
                            }
                            ResolvedLinkKind::Raw if !value.contains(&b'/') => item
                                .text
                                .value
                                .strip_suffix(b".framework")
                                .filter(|name| !name.is_empty()),
                            _ => None,
                        });
                    let use_path = framework.is_none()
                        && (value.contains(&b'/')
                            || matches!(item.kind, ResolvedLinkKind::Library(_)));
                    let raw = item.text.value.as_slice();
                    if !use_path
                        && (raw.starts_with(b"-")
                            || raw.starts_with(b"$")
                            || raw.starts_with(b"`")
                            || raw.starts_with(b"LINKER:"))
                    {
                        continue;
                    }
                    let template = alternative(template, use_path);
                    let comma_arguments = template
                        .strip_prefix(b"LINKER:")
                        .is_some_and(|arguments| !arguments.starts_with(b"SHELL:"));
                    let definition = |key: &[u8]| {
                        lower::definition(self.model, self.target.dir, key)
                            .map_or(b"".as_slice(), |v| v.as_slice())
                    };
                    let suffix = definition(b"CMAKE_LINK_LIBRARY_SUFFIX");
                    let name = [library_name(raw, &definition), suffix].concat();
                    let flag =
                        lower::definition(self.model, self.target.dir, b"CMAKE_LINK_LIBRARY_FLAG")
                            .map_or(b"-l".as_slice(), |v| v.as_slice());
                    let expand = |template: &[u8]| {
                        let word = |arg: &[u8]| {
                            if comma_arguments {
                                BString::from(arg)
                            } else {
                                quote(arg)
                            }
                        };
                        let fragment_word = |arg: &[u8]| {
                            if comma_arguments {
                                unquote_link_word(arg)
                            } else {
                                BString::from(arg)
                            }
                        };
                        lower::expand_rule(template, |key| match key {
                            b"LINK_ITEM" | b"LIBRARY" | b"LIB_ITEM" if use_path => {
                                Some(fragment_word(value))
                            }
                            b"LIBRARY" | b"LIB_ITEM" if framework.is_some() => {
                                framework.map(fragment_word)
                            }
                            b"LINK_ITEM" if framework.is_some() => framework.map(|name| {
                                [b"-framework ", fragment_word(name).as_slice()]
                                    .concat()
                                    .into()
                            }),
                            b"LINK_ITEM" => Some(word(&[flag, &name].concat())),
                            b"LIBRARY" => Some(word(&name)),
                            b"LIB_ITEM" => Some(word(raw)),
                            _ => None,
                        })
                    };
                    fragment.text.value = if comma_arguments {
                        // Let the wrapper split commas before it sees library paths.
                        let args = self.wrapper.expand(&template)?;
                        render_arguments(args.iter().map(|arg| expand(arg)))
                    } else {
                        self.render(&expand(&template))?
                    };
                }
            }
        }
        if let Some((item, definition)) = library {
            self.push(fragments, &definition.suffix, item)?;
        }
        Ok(())
    }

    fn render(&self, text: &BString) -> Result<BString, ResolveError> {
        if !text.starts_with(b"LINKER:") {
            return Ok(text.clone());
        }
        Ok(render_arguments(self.wrapper.expand(text)?))
    }

    fn push(
        &self,
        fragments: &mut Vec<LinkFragment>,
        text: &BString,
        item: &ResolvedLinkItem,
    ) -> Result<(), ResolveError> {
        if !text.is_empty() {
            fragments.push(link_fragment(
                self.render(text)?,
                &item.text,
                LinkFragmentRole::Libraries,
            ));
        }
        Ok(())
    }

    pub(super) fn visual_studio_whole_archive(&self) -> Option<BString> {
        let definition = self.definitions.get(&(false, "WHOLE_ARCHIVE".into()))?;
        let template = alternative(definition.template.as_ref()?, true);
        (!template.is_empty()).then(|| template.into())
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
        let supports = |key: &str| find(&format!("{key}_SUPPORTED"));
        let target = &self.target.name;
        let language = self.language;
        let describe = |reason| {
            self.error(format!(
                "Feature '{feature}', specified through generator-expression '$<{operator}>' to link target '{target}', is not {reason} for the '{language}' link language."
            ))
        };
        let variable = if supports(&specific).is_some() {
            specific
        } else {
            generic
        };
        if !supports(&variable).is_some_and(|value| truth::is_on(value)) {
            return Err(describe("supported"));
        }
        let value = find(&variable).ok_or_else(|| describe("defined"))?;
        // CMake keeps empty elements here, so an empty value is one empty element.
        let mut elements = if value.is_empty() {
            vec![BString::default()]
        } else {
            list::split(value, list::Empty::Keep)
        }
        .into_iter();
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

fn render_arguments(args: impl IntoIterator<Item = BString>) -> BString {
    let parts: Vec<_> = args.into_iter().map(|arg| quote(&arg)).collect();
    parts
        .iter()
        .map(|arg| arg.as_slice())
        .collect::<Vec<_>>()
        .join(b" ".as_slice())
        .into()
}

fn unquote_link_word(arg: &[u8]) -> BString {
    arg.strip_prefix(b"'")
        .and_then(|word| word.strip_suffix(b"'"))
        .map_or_else(|| arg.into(), |word| word.replace(b"'\\''", b"'").into())
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
    if !has_placeholder(&alternative(value, true)) {
        return Err(if value.windows(5).any(|token| token == b"PATH{") {
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing for \"PATH{}\" alternative"
        } else {
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing"
        });
    }
    if !has_placeholder(&alternative(value, false)) {
        return Err(
            "\"<LIBRARY>\", \"<LIB_ITEM>\", or \"<LINK_ITEM>\" patterns are missing for \"NAME{}\" alternative",
        );
    }
    Ok(())
}

fn alternative(template: &[u8], use_path: bool) -> Vec<u8> {
    let (active, inactive) = if use_path {
        (b"PATH{", b"NAME{")
    } else {
        (b"NAME{", b"PATH{")
    };
    let position = |value: &[u8], tag: &[u8]| value.windows(tag.len()).position(|part| part == tag);
    let mut value = template.to_vec();
    if let Some(start) = position(&value, inactive)
        && let Some(tail) = value.get(start + inactive.len()..)
        && let Some(end) = position(tail, b"}")
    {
        value.drain(start..start + inactive.len() + end + 1);
    }
    if let Some(start) = position(&value, active) {
        value.drain(start..start + active.len());
        if let Some(end) = value.get(start..).and_then(|tail| position(tail, b"}")) {
            value.remove(start + end);
        }
    }
    value
}

fn library_name<'a, 'model>(
    raw: &'a [u8],
    definition: &impl Fn(&[u8]) -> &'model [u8],
) -> &'a [u8] {
    for (prefix, suffix) in [
        (
            b"CMAKE_IMPORT_LIBRARY_PREFIX".as_slice(),
            b"CMAKE_IMPORT_LIBRARY_SUFFIX".as_slice(),
        ),
        (
            b"CMAKE_SHARED_LIBRARY_PREFIX",
            b"CMAKE_SHARED_LIBRARY_SUFFIX",
        ),
        (
            b"CMAKE_STATIC_LIBRARY_PREFIX",
            b"CMAKE_STATIC_LIBRARY_SUFFIX",
        ),
    ] {
        let extension = definition(suffix);
        if !extension.is_empty()
            && let Some(name) = raw
                .strip_prefix(definition(prefix))
                .and_then(|name| name.strip_suffix(extension))
        {
            return name;
        }
    }
    raw
}
