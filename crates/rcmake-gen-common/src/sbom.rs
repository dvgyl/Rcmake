//! SPDX 3.0.1 JSON-LD software bill of materials.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rcmake_base::{BString, ByteSlice, list};
use rcmake_genex::{
    ConfigId, EvalContext, GenexNode, PreprocessMode, TargetHandle, UsageTraversal,
};
use rcmake_json::{Value, write_styled};
use rcmake_model::{DirId, ProjectMetadata, ProjectModel, SbomSpec, Target, TargetId, TargetKind};
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

use crate::GenError;

type Object = BTreeMap<BString, Value>;
const CREATION_INFO: &str = "_:Build#CreationInfo";

/// Render an SPDX document for one configuration. Use an empty name for `noconfig`.
///
/// # Errors
///
/// Return an error for an invalid format, export set, or link expression.
pub fn render_sbom(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &SbomSpec,
    config: &[u8],
) -> Result<BString, GenError> {
    render_for_tree(
        model,
        result,
        spec,
        DirId::ROOT,
        config,
        PreprocessMode::Build,
        0,
    )
}

pub(crate) fn validate_format(format: &[u8]) -> Result<(), GenError> {
    match format {
        b"spdx" | b"spdx-3.0+json" | b"spdx-3.0.1+json" => Ok(()),
        _ => Err(GenError::Invalid(
            format!("Invalid SBOM format \"{}\".", BString::from(format)).into(),
        )),
    }
}

pub(crate) fn with_project_metadata(model: &ProjectModel, dir: DirId, spec: &SbomSpec) -> SbomSpec {
    let current = model.definition(dir, b"PROJECT_NAME");
    let project = match &spec.project {
        ProjectMetadata::Named(name) => Some(name),
        ProjectMetadata::Current => current.filter(|name| *name == &spec.name),
        ProjectMetadata::None => None,
    };
    let mut spec = spec.clone();
    if let Some(project) = project {
        let field = |suffix: &[u8]| {
            model
                .definition(dir, &[project.as_slice(), b"_", suffix].concat())
                .filter(|value| !value.is_empty())
                .cloned()
        };
        let license = field(b"SPDX_LICENSE").or_else(|| field(b"LICENSE"));
        spec.data_license = spec.data_license.or_else(|| license.clone());
        spec.default_license = spec.default_license.or(license);
        spec.version = spec.version.or_else(|| field(b"VERSION"));
        spec.description = spec.description.or_else(|| field(b"DESCRIPTION"));
        spec.homepage_url = spec.homepage_url.or_else(|| field(b"HOMEPAGE_URL"));
    }
    spec
}

pub(crate) fn render_for_tree(
    model: &ProjectModel,
    result: &GenerationResult,
    spec: &SbomSpec,
    dir: DirId,
    config: &[u8],
    mode: PreprocessMode,
    epoch_seconds: i64,
) -> Result<BString, GenError> {
    if let Some(format) = &spec.format {
        validate_format(format)?;
    }
    let spec = with_project_metadata(model, dir, spec);
    let mut roots = BTreeSet::new();
    for set in &spec.exports {
        let members = model
            .exports
            .get(set)
            .ok_or_else(|| GenError::Invalid(format!("Unknown export set \"{set}\".").into()))?;
        roots.extend(&members.targets);
    }
    let created = jiff::Timestamp::from_second(epoch_seconds)
        .map_err(|error| GenError::Invalid(error.to_string().into()))?
        .to_string();
    let creation = object([
        ("@id", text(CREATION_INFO)),
        (
            "comment",
            text("This SBOM was generated from the CMakeLists.txt File"),
        ),
        ("created", text(created.as_str())),
        (
            "createdBy",
            array([text("https://gitlab.kitware.com/cmake/cmake")]),
        ),
        ("specVersion", text("3.0.1")),
        ("type", text("CreationInfo")),
    ]);
    let mut document = node(&spec.name, "SPDXDocument", "SpdxDocument");
    document.insert("name".into(), text(spec.name.clone()));
    document.insert(
        "profileConformance".into(),
        array([text("core"), text("software")]),
    );
    optional(&mut document, "description", spec.description.as_ref());
    if let Some(license) = spec.data_license.as_ref().filter(|value| !value.is_empty()) {
        document.insert(
            "dataLicense".into(),
            license_expression(&spec.name, "DataLicenseExpression", license),
        );
    }
    let mut usage = UsageResolver::new(model);
    let mut visited = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    let mut root_elements = Vec::new();
    let mut elements = Vec::new();
    let mut imported_relationships = Vec::new();
    let mut relationships = Vec::new();
    let mut queue: VecDeque<_> = roots.iter().copied().collect();
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id) {
            continue;
        }
        let target = model
            .target(id)
            .ok_or_else(|| GenError::Invalid("Unknown SBOM target.".into()))?;
        let name = package_name(target);
        let is_root = roots.contains(&id);
        let package = package(target, &name, &spec, is_root, &created);
        if is_root {
            root_elements.push(package);
        } else {
            elements.push(package);
        }

        let links = target_links(model, result, &mut usage, id, target, config, mode)?;
        let mut dependencies = Vec::new();
        let mut shared = Vec::new();
        for (lookup, link) in links {
            if let Some(dependency) = model.find_target(lookup, &link) {
                if dependency == id {
                    continue;
                }
                queue.push_back(dependency);
                if let Some(target) = model.target(dependency) {
                    let package = text(urn(&package_name(target), "Package"));
                    let group = if target.kind == TargetKind::SharedLibrary {
                        &mut shared
                    } else {
                        &mut dependencies
                    };
                    if !group.contains(&package) {
                        group.push(package);
                    }
                }
            } else if !link.starts_with(b"-") && unknown.insert(link.clone()) {
                let mut package = node(&link, "Package", "software_Package");
                package.insert("name".into(), text(link));
                elements.push(Value::Object(package));
            }
        }
        let relationships = if target.is_imported() {
            &mut imported_relationships
        } else {
            &mut relationships
        };
        if !dependencies.is_empty() {
            relationships.push(relationship(
                &name,
                "StaticRelationship",
                "dependsOn",
                dependencies,
                Some("Required Build-Time Libraries"),
            ));
        }
        if !shared.is_empty() {
            relationships.push(relationship(
                &name,
                "SharedRelationship",
                "dependsOn",
                shared,
                Some("Required Run-Time Libraries"),
            ));
        }
        if let Some(license) = target
            .props
            .get(b"SPDX_LICENSE")
            .or_else(|| is_root.then_some(spec.default_license.as_ref()).flatten())
            .filter(|value| !value.is_empty())
        {
            relationships.push(relationship(
                &name,
                "DeclaredLicenseRelationship",
                "hasDeclaredLicense",
                vec![license_expression(&name, "LicenseExpression", license)],
                None,
            ));
        }
    }
    if !elements.is_empty() {
        document.insert("element".into(), array(elements));
    }
    document.insert("rootElement".into(), array(root_elements));
    let graph = [creation, Value::Object(document)]
        .into_iter()
        .chain(imported_relationships)
        .chain(relationships);
    Ok(write_styled(
        &object([
            (
                "@context",
                text("https://spdx.org/rdf/3.0.1/spdx-context.jsonld"),
            ),
            ("@graph", array(graph)),
        ]),
        "  ",
    ))
}

fn target_links(
    model: &ProjectModel,
    result: &GenerationResult,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    config: &[u8],
    mode: PreprocessMode,
) -> Result<Vec<(DirId, BString)>, GenError> {
    let handle = TargetHandle(id.index() as u32);
    let context = EvalContext {
        config: ConfigId(config.into()),
        head: Some(handle),
        current: Some(handle),
        traversal: UsageTraversal::Link,
        link_language: result
            .targets
            .iter()
            .find(|facts| facts.id == id && facts.config.matches(config))
            .and_then(|facts| facts.link.as_ref())
            .map(|link| link.language.clone()),
        ..EvalContext::default()
    };
    let mut links = Vec::new();
    for (items, property) in [
        (&target.usage.link_libraries, b"LINK_LIBRARIES".as_slice()),
        (
            &target.usage.interface_link_libraries,
            b"INTERFACE_LINK_LIBRARIES",
        ),
    ] {
        let entries: Vec<_> = items.as_ref().map_or_else(
            || {
                target
                    .props
                    .get(property)
                    .into_iter()
                    .map(|value| (target.dir, value))
                    .collect()
            },
            |items| {
                items
                    .iter()
                    .map(|item| (item.lookup_dir, &item.text.value))
                    .collect()
            },
        );
        for (dir, value) in entries {
            let mut ast = rcmake_genex::parse(value)
                .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            include_compile_only(&mut ast.nodes);
            let ast = rcmake_genex::strip(&ast, mode);
            let (value, _) = rcmake_genex::evaluate(&ast, &context, &mut usage.host(dir))
                .map_err(|error| GenError::Invalid(error.to_string().into()))?;
            links.extend(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .filter(|link| {
                        ![
                            b"<LINK_LIBRARY:".as_slice(),
                            b"</LINK_LIBRARY:",
                            b"<LINK_GROUP:",
                            b"</LINK_GROUP:",
                        ]
                        .iter()
                        .any(|marker| link.starts_with(marker))
                    })
                    .map(|link| (dir, link)),
            );
        }
    }
    // Imported targets can refer to a target with the same name in another directory.
    links.retain(|(dir, link)| model.find_target(*dir, link) != Some(id));
    Ok(links)
}

// Both compile-only and link-only requirements belong in an SBOM.
fn include_compile_only(nodes: &mut [GenexNode]) {
    for node in nodes {
        let GenexNode::Expr(expr) = node else {
            continue;
        };
        if let [GenexNode::Text(name)] = expr.identifier.as_mut_slice()
            && name == b"COMPILE_ONLY"
        {
            *name = "LINK_ONLY".into();
        }
        include_compile_only(&mut expr.identifier);
        if let Some(params) = &mut expr.params {
            for param in params {
                include_compile_only(param);
            }
        }
    }
}

fn package_name(target: &Target) -> BString {
    if let Some((namespace, component)) = target.name.split_once_str(b"::") {
        return [namespace, b":", component].concat().into();
    }
    target.name.clone()
}

fn package(target: &Target, name: &[u8], spec: &SbomSpec, root: bool, created: &str) -> Value {
    let mut package = node(name, "Package", "software_Package");
    package.insert("name".into(), text(name));
    if target.is_imported() {
        if let Some(origin) = target
            .props
            .get(b"EXPORT_FIND_PACKAGE_NAME")
            .map(|value| value.as_slice())
            .or_else(|| name.split_once_str(b":").map(|(origin, _)| origin))
        {
            let mut organization = node(origin, "Organization", "Organization");
            organization.insert("name".into(), text(origin));
            package.insert("originatedBy".into(), array([Value::Object(organization)]));
            package.insert("builtTime".into(), text(created));
            package.insert(
                "externalRef".into(),
                array([object([
                    ("externalRefType", text("buildSystem")),
                    (
                        "locator",
                        text(format!("cmake:find_package({})", BString::from(origin))),
                    ),
                    ("type", text("ExternalRef")),
                ])]),
            );
        }
    } else {
        let purpose = if target.kind == TargetKind::Executable {
            "application"
        } else {
            "library"
        };
        package.insert("software_primaryPurpose".into(), text(purpose));
    }
    optional(
        &mut package,
        "software_packageVersion",
        target
            .props
            .get(b"VERSION")
            .or_else(|| root.then_some(spec.version.as_ref()).flatten()),
    );
    if root {
        optional(
            &mut package,
            "software_homePage",
            spec.homepage_url.as_ref(),
        );
        optional(
            &mut package,
            "software_downloadLocation",
            spec.package_url.as_ref(),
        );
    }
    Value::Object(package)
}

fn relationship(
    name: &[u8],
    id: &str,
    kind: &str,
    to: Vec<Value>,
    description: Option<&str>,
) -> Value {
    let mut relationship = node(name, id, "Relationship");
    relationship.insert("from".into(), text(urn(name, "Package")));
    relationship.insert("relationshipType".into(), text(kind));
    relationship.insert("to".into(), array(to));
    if let Some(description) = description {
        relationship.insert("description".into(), text(description));
    }
    Value::Object(relationship)
}

fn license_expression(name: &[u8], id: &str, license: &BString) -> Value {
    let mut expression = node(name, id, "simplelicensing_LicenseExpression");
    expression.insert(
        "simplelicensing_licenseExpression".into(),
        text(license.clone()),
    );
    Value::Object(expression)
}

fn node(name: &[u8], id: &str, kind: &str) -> Object {
    BTreeMap::from([
        ("creationInfo".into(), text(CREATION_INFO)),
        ("spdxId".into(), text(urn(name, id))),
        ("type".into(), text(kind)),
    ])
}

fn urn(name: &[u8], id: &str) -> BString {
    [b"urn:", name, b"#", id.as_bytes()].concat().into()
}

fn optional(out: &mut Object, field: &str, value: Option<&BString>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        out.insert(field.into(), text(value.clone()));
    }
}

fn text(value: impl Into<BString>) -> Value {
    Value::string(value)
}

fn array(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Array(values.into_iter().collect())
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
