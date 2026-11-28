//! NuGet `PackageReference` items from `VS_PACKAGE_REFERENCES`.

use rcmake_base::list;
use rcmake_model::Target;

use crate::{PACKAGE_REFERENCES, string, xml::Writer};

/// Split `VS_PACKAGE_REFERENCES` into `(name, version)` pairs.
///
/// Each list element has the form `<name>_<version>`. The version follows
/// the last `_`. An element without `_` gives no pair.
pub(crate) fn references(value: &[u8]) -> Vec<(String, String)> {
    list::split(value, list::Empty::Drop)
        .iter()
        .filter_map(|element| {
            let split = element.iter().rposition(|byte| *byte == b'_')?;
            let name = element.get(..split)?;
            let version = element.get(split + 1..)?;
            Some((string(name), string(version)))
        })
        .collect()
}

/// Write an `ItemGroup` with one `PackageReference` for each package.
///
/// CMake writes the group when the list has elements, also when no element
/// has a version.
pub(crate) fn write(xml: &mut Writer, target: &Target) {
    let Some(value) = target.props.get(PACKAGE_REFERENCES) else {
        return;
    };
    if list::split(value, list::Empty::Drop).is_empty() {
        return;
    }
    let references = references(value);
    if references.is_empty() {
        xml.empty("ItemGroup", &[]);
        return;
    }
    xml.start("ItemGroup", &[]);
    for (name, version) in &references {
        xml.empty(
            "PackageReference",
            &[("Include", name.as_str()), ("Version", version.as_str())],
        );
    }
    xml.end();
}

#[cfg(test)]
mod tests {
    use rcmake_model::{DirId, Target, TargetKind, TargetOrigin};

    use super::{references, write};
    use crate::xml::Writer;

    fn with_references(name: &str, kind: TargetKind, value: &str) -> Target {
        let mut target = Target::new(
            name.into(),
            kind,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        );
        target
            .props
            .set("VS_PACKAGE_REFERENCES".into(), value.into());
        target
    }

    #[test]
    fn package_references_split_at_the_last_underscore() {
        assert_eq!(
            references(b"Newtonsoft.Json_13.0.3;My_Pkg_1.2;NoVersion;;Trailing_"),
            [
                ("Newtonsoft.Json".to_owned(), "13.0.3".to_owned()),
                ("My_Pkg".to_owned(), "1.2".to_owned()),
                ("Trailing".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn package_reference_items_escape_attributes() {
        let target = with_references("app", TargetKind::Executable, "A&B<\"q\">_1.0;Plain");
        let mut xml = Writer::new(false, "utf-8");
        write(&mut xml, &target);
        let output = xml.finish();
        assert!(output.contains("<ItemGroup>\r\n"), "{output}");
        assert!(
            output.contains(
                r#"<PackageReference Include="A&amp;B&lt;&quot;q&quot;&gt;" Version="1.0" />"#
            ),
            "{output}"
        );
        assert!(!output.contains("Plain"), "{output}");

        let target = with_references("lib", TargetKind::StaticLibrary, "NoVersion");
        let mut xml = Writer::new(false, "utf-8");
        write(&mut xml, &target);
        assert!(xml.finish().contains("<ItemGroup />"));
    }
}
