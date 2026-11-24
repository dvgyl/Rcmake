//! Bundle metadata and files to embed in a product.

use std::path::PathBuf;

use rcmake_gen_common::GenError;
use rcmake_host::{ReadRange, path::to_native};
use rcmake_model::{Target, TargetId};
use rcmake_resolve::{
    ResolvedTarget,
    ide::{BundleKind, IdeFacts},
};

use crate::{
    object::{Isa, Reference, attrs, list},
    phases,
    settings::{self, Product},
    text, workspace, write_file,
};

use super::{
    Project, file_ref,
    sources::{extension_file_type, references_config},
};

impl Project<'_> {
    pub(super) fn embeds(
        &mut self,
        key: &str,
        id: TargetId,
        raw: &Target,
    ) -> Result<Vec<Reference>, GenError> {
        let mut phases = Vec::new();
        for (kind, destination, label) in [
            ("FRAMEWORKS", "10", "Embed Frameworks"),
            ("PLUGINS", "13", "Embed PlugIns"),
            ("APP_EXTENSIONS", "13", "Embed App Extensions"),
            ("XPC_SERVICES", "16", "Embed XPC Services"),
            ("RESOURCES", "7", "Embed Resources"),
        ] {
            let property = format!("XCODE_EMBED_{kind}");
            let Some(expression) = raw.props.get(property.as_bytes()) else {
                continue;
            };
            let property_value = |ide: &IdeFacts| ide.attributes.get(property.as_bytes()).cloned();
            let mut values = self.configs.iter().filter_map(|config| {
                let target = self.resolved.get(&(id, config.clone()))?;
                target.ide.as_deref().map(property_value)
            });
            let items = values.next().flatten().unwrap_or_default();
            if references_config(&text(expression))
                || values.any(|value| value.as_ref() != Some(&items))
            {
                return Err(GenError::Invalid(format!(
                    "Xcode does not support per-config {property}:\n\n  {expression}\n\nspecified for target:\n\n  {}", raw.name
                ).into()));
            }
            let key = format!("{key}/embed/{kind}");
            let mut files = Vec::new();
            let mut attributes = Vec::new();
            for (suffix, name) in [
                ("CODE_SIGN_ON_COPY", "CodeSignOnCopy"),
                ("REMOVE_HEADERS_ON_COPY", "RemoveHeadersOnCopy"),
            ] {
                if settings::on(raw, format!("{property}_{suffix}").as_bytes()) {
                    attributes.push(name);
                }
            }
            for (index, item) in settings::strings(&items).into_iter().enumerate() {
                let file_key = format!("{key}/{index}");
                let (item, source_tree) = self
                    .model
                    .find_target(raw.dir, item.as_bytes())
                    .and_then(|binding| self.model.target(binding))
                    .filter(|target| !target.is_imported())
                    .map_or((item, "<absolute>"), |target| {
                        let config = &self.default_config;
                        let resolved =
                            self.model
                                .find_target(raw.dir, &target.name)
                                .and_then(|binding| {
                                    self.resolved.get(&(binding, config.clone())).copied()
                                });
                        (
                            Product::new(self.model, target, resolved, config, self.bin).file(),
                            "BUILT_PRODUCTS_DIR",
                        )
                    });
                let file = file_ref(
                    &mut self.objects,
                    &file_key,
                    &item,
                    ("explicitFileType", extension_file_type(&item)),
                    source_tree,
                )?;
                files.push(phases::build_file(
                    &mut self.objects,
                    &format!("{file_key}/build"),
                    label,
                    &file,
                    attrs([("ATTRIBUTES", list(attributes.iter().copied()))]),
                )?);
            }
            if files.is_empty() {
                continue;
            }
            let path = raw
                .props
                .get(format!("{property}_PATH").as_bytes())
                .map_or_else(String::new, |p| text(p));
            phases.push(self.objects.add(
                Isa::PBXCopyFilesBuildPhase,
                &key,
                label,
                attrs([
                    ("buildActionMask", "2147483647".into()),
                    ("dstPath", path.into()),
                    ("dstSubfolderSpec", destination.into()),
                    ("files", list(files)),
                    ("name", label.into()),
                    ("runOnlyForDeploymentPostprocessing", "0".into()),
                ]),
            )?);
        }
        Ok(phases)
    }

    pub(super) fn info_plist(
        &self,
        raw: &Target,
        name: &str,
        product: &Product,
        facts: Option<&ResolvedTarget>,
    ) -> Result<Option<PathBuf>, GenError> {
        let (template, prefix, executable_key, package) = match product.bundle {
            BundleKind::None => return Ok(None),
            BundleKind::App { .. } => (
                "MacOSXBundleInfo.plist.in",
                "MACOSX_BUNDLE",
                "MACOSX_BUNDLE_EXECUTABLE_NAME",
                "APPL",
            ),
            BundleKind::Framework { .. } => (
                "MacOSXFrameworkInfo.plist.in",
                "MACOSX_FRAMEWORK",
                "MACOSX_FRAMEWORK_NAME",
                "FMWK",
            ),
            BundleKind::Bundle { .. } => (
                "MacOSXBundleInfo.plist.in",
                "MACOSX_BUNDLE",
                "MACOSX_BUNDLE_EXECUTABLE_NAME",
                "BNDL",
            ),
        };
        if let Some(plist) = facts
            .and_then(|facts| facts.ide.as_ref())
            .and_then(|ide| ide.info_plist.as_ref())
        {
            let path = self.bin.join(text(plist));
            if self.host.metadata(&path, rcmake_host::Follow::Yes).is_ok() {
                return Ok(Some(path));
            }
        }
        let custom = raw.props.get(format!("{prefix}_INFO_PLIST").as_bytes());
        let template_path = custom
            .map(|path| to_native(&self.model.dir(raw.dir).source_dir).join(text(path)))
            .or_else(|| {
                self.model
                    .definition(raw.dir, b"CMAKE_ROOT")
                    .map(|root| to_native(root).join("Modules").join(template))
            });
        let input = match template_path {
            Some(path) => self
                .host
                .read(&path, ReadRange::All { limit: 1024 * 1024 })
                .map(|data| text(&data))
                .map_err(|error| {
                    GenError::Invalid(format!("Cannot read {}: {error}", path.display()).into())
                })?,
            None => format!(
                "{}{}<dict><key>CFBundleExecutable</key><string>${{{executable_key}}}</string>\n<key>CFBundleIdentifier</key><string>$(PRODUCT_BUNDLE_IDENTIFIER)</string>\n<key>CFBundlePackageType</key><string>{package}</string>\n<key>CFBundleVersion</key><string>1</string></dict>\n</plist>\n",
                workspace::XML,
                workspace::PLIST
            ),
        };
        let lookup = |key: &str| {
            if key == executable_key {
                return "$(EXECUTABLE_NAME)".into();
            }
            raw.props
                .get(key.as_bytes())
                .or_else(|| self.model.definition(raw.dir, key.as_bytes()))
                .map_or_else(String::new, |value| text(value))
        };
        let info = expand_plist(&input, lookup);
        let path = to_native(&self.model.dir(raw.dir).binary_dir)
            .join(format!("CMakeFiles/{name}.dir/Info.plist"));
        let path = self.bin.join(path);
        write_file(self.host, &path, &info)?;
        Ok(Some(path))
    }
}

fn expand_plist(mut input: &str, lookup: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    while let Some((start, open)) = input.match_indices(['@', '$']).next() {
        let Some(tail) = input.get(start + open.len()..) else {
            break;
        };
        out.push_str(input.get(..start).unwrap_or_default());
        let token = if open == "@" {
            tail.split_once('@')
        } else {
            tail.strip_prefix('{').and_then(|tail| tail.split_once('}'))
        };
        if let Some((key, rest)) = token.filter(|(key, _)| {
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            out.push_str(&lookup(key));
            input = rest;
        } else {
            out.push_str(open);
            input = tail;
        }
    }
    out.push_str(input);
    out
}
