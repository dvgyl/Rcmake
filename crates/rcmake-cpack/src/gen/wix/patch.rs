//! Bounded WiX XML patch parser and applicator (`CPACK_WIX_PATCH_FILE`).

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_host::{FileKind, Follow, Host, ReadRange};

use crate::generators::installer::{XmlElement, XmlNode, XmlWriter, parse_xml_bounded};
use crate::{CpackError, error};

const MAX_PATCH_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A single `<CPackWiXFragment Id="...">` entry parsed from a patch file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PatchFragment {
    pub(super) attributes: Vec<(String, String)>,
    pub(super) children: Vec<XmlNode>,
}

/// Collection of WiX XML patch fragments indexed by target element ID.
#[derive(Clone, Debug, Default)]
pub(super) struct WixPatchSet {
    fragments: BTreeMap<String, PatchFragment>,
}

impl WixPatchSet {
    /// Load and validate all fragments from `patch_path` using the bounded safe XML parser
    /// (forbidding DTDs, external entities, and duplicate fragment IDs).
    pub(super) fn load_file(
        &mut self,
        host: &dyn Host,
        patch_path: &Path,
    ) -> Result<(), CpackError> {
        let meta = host
            .metadata_fresh(patch_path, Follow::Yes)
            .map_err(|err| {
                error(format!(
                    "Cannot read WiX patch file '{}': {err}",
                    patch_path.display()
                ))
            })?;
        if meta.kind != FileKind::File {
            return Err(error(format!(
                "Expected regular WiX patch file at '{}'",
                patch_path.display()
            )));
        }
        let bytes = host.read(
            patch_path,
            ReadRange::All {
                limit: MAX_PATCH_FILE_BYTES,
            },
        )?;
        let label = patch_path.display().to_string();
        let root = parse_xml_bounded(&bytes, &label)?;
        if root.name != "CPackWiXPatch" {
            return Err(error(format!(
                "Error while processing XML patch file '{label}': Expected root element 'CPackWiXPatch', got '{}'",
                root.name
            )));
        }
        for child in root.children {
            match child {
                XmlNode::Text(text) => {
                    if !trim_xml_whitespace(&text).is_empty() {
                        return Err(error(format!(
                            "Error while processing XML patch file '{label}': Unexpected text outside CPackWiXFragment"
                        )));
                    }
                }
                XmlNode::Element(elem) => {
                    if elem.name != "CPackWiXFragment" {
                        return Err(error(format!(
                            "Error while processing XML patch file '{label}': Expected 'CPackWiXFragment' element, got '{}'",
                            elem.name
                        )));
                    }
                    self.insert_fragment(elem, &label)?;
                }
            }
        }
        Ok(())
    }

    fn insert_fragment(&mut self, elem: XmlElement, label: &str) -> Result<(), CpackError> {
        let mut id: Option<String> = None;
        let mut extra_attrs = Vec::new();
        for (k, v) in elem.attributes {
            if k == "Id" {
                id = Some(v);
            } else {
                extra_attrs.push((k, v));
            }
        }
        let Some(id) = id.filter(|s| !s.is_empty()) else {
            return Err(error(format!(
                "Error while processing XML patch file '{label}': No 'Id' specified for 'CPackWiXFragment' element"
            )));
        };
        if self.fragments.contains_key(&id) {
            return Err(error(format!(
                "Error while processing XML patch file '{label}': Invalid reuse of 'CPackWiXFragment' 'Id': {id}"
            )));
        }
        self.fragments.insert(
            id,
            PatchFragment {
                attributes: extra_attrs,
                children: trim_patch_children(elem.children),
            },
        );
        Ok(())
    }

    /// Apply and consume any patch fragment targeting `id` into `writer`.
    ///
    /// Note: caller must invoke this while the target element's opening tag is still open so
    /// fragment attributes attach to the target element, followed by fragment child nodes.
    pub(super) fn apply(&mut self, id: &str, writer: &mut XmlWriter) -> Result<(), CpackError> {
        let Some(fragment) = self.fragments.remove(id) else {
            return Ok(());
        };
        for (k, v) in &fragment.attributes {
            writer.attr(k, v)?;
        }
        write_patch_children(&fragment.children, writer)
    }

    /// Fail if any loaded fragment ID was never matched during source rendering.
    pub(super) fn check_all_applied(&self) -> Result<(), CpackError> {
        if self.fragments.is_empty() {
            return Ok(());
        }
        let missing: Vec<String> = self.fragments.keys().map(|id| format!("'{id}'")).collect();
        Err(error(format!(
            "Some XML patch fragments did not have matching IDs: {}",
            missing.join(", ")
        )))
    }
}

fn trim_xml_whitespace(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\r', '\n'])
}

fn trim_patch_children(children: Vec<XmlNode>) -> Vec<XmlNode> {
    let mut out = Vec::with_capacity(children.len());
    for node in children {
        match node {
            XmlNode::Text(text) => {
                let trimmed = trim_xml_whitespace(&text);
                if !trimmed.is_empty() {
                    out.push(XmlNode::Text(trimmed.to_owned()));
                }
            }
            XmlNode::Element(mut elem) => {
                elem.children = trim_patch_children(elem.children);
                out.push(XmlNode::Element(elem));
            }
        }
    }
    out
}

fn write_patch_children(children: &[XmlNode], writer: &mut XmlWriter) -> Result<(), CpackError> {
    for node in children {
        match node {
            XmlNode::Text(text) => writer.text(text)?,
            XmlNode::Element(elem) => {
                writer.start_element(&elem.name)?;
                for (k, v) in &elem.attributes {
                    writer.attr(k, v)?;
                }
                write_patch_children(&elem.children, writer)?;
                writer.end_element(&elem.name)?;
            }
        }
    }
    Ok(())
}
