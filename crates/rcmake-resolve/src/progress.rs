//! Build progress messages shared by Ninja and Unix Makefiles.

use std::collections::HashMap;

use crate::{BuildGraph, GenerationResult, LinkKind, LinkStep};

/// Read the language of each compiled object and precompiled header.
pub fn compile_languages<'a>(
    result: &'a GenerationResult,
    graph: &BuildGraph,
) -> HashMap<&'a [u8], &'a [u8]> {
    let units = result
        .targets
        .iter()
        .filter(|target| target.config == graph.config)
        .flat_map(|target| {
            target.compile_units.iter().filter_map(|unit| {
                let group = target.compile_groups.get(unit.group)?;
                Some((&unit.object, group.language.as_slice()))
            })
        });
    let mut languages = HashMap::new();
    for (object, language) in units {
        languages.insert(object.as_slice(), language);
        if let Some(inputs) = result.compile_inputs.get(object) {
            for input in inputs {
                if result.compile_inputs.contains_key(input) {
                    languages.entry(input.as_slice()).or_insert(language);
                }
            }
        }
    }
    languages
}

/// Describe a compile step.
pub fn compile_description(language: Option<&[u8]>, output: &[u8]) -> Vec<u8> {
    match language.filter(|language| !language.is_empty()) {
        Some(language) => [b"Building ".as_slice(), language, b" object ", output].concat(),
        None => [b"Building ".as_slice(), output].concat(),
    }
}

/// Describe a link or archive step.
pub fn link_description(step: &LinkStep, output: &[u8]) -> Vec<u8> {
    let kind: &[u8] = match step.kind {
        LinkKind::Executable => b"executable",
        LinkKind::SharedLibrary => b"shared library",
        LinkKind::ModuleLibrary => b"shared module",
        LinkKind::StaticArchive => b"static library",
    };
    let language = step.language.as_slice();
    let language_space = if language.is_empty() { &[][..] } else { b" " };
    [
        b"Linking ".as_slice(),
        language,
        language_space,
        kind,
        b" ",
        output,
    ]
    .concat()
}
