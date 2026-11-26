//! Render Android NDK prebuilt module declarations.

use rcmake_base::{BString, list};
use rcmake_resolve::{AndroidExportTarget, ExportPlan};

pub(super) fn render(plan: &ExportPlan, destination: &[u8]) -> BString {
    let mut out = BString::default();
    if !plan.append {
        out.extend_from_slice(b"LOCAL_PATH := $(call my-dir)\n");
        if plan.install {
            out.extend_from_slice(b"_IMPORT_PREFIX := $(LOCAL_PATH)");
            for _ in destination
                .split(|&byte| byte == b'/')
                .filter(|part| !part.is_empty())
            {
                out.extend_from_slice(b"/..");
            }
            out.push(b'\n');
        }
        out.push(b'\n');
    }
    for target in &plan.targets {
        let Some(android) = &target.android else {
            continue;
        };
        out.extend_from_slice(b"include $(CLEAR_VARS)\nLOCAL_MODULE := ");
        out.extend_from_slice(&plan.namespace);
        out.extend_from_slice(&target.name);
        out.extend_from_slice(b"\nLOCAL_SRC_FILES := ");
        if plan.install {
            out.extend_from_slice(b"$(_IMPORT_PREFIX)/");
        }
        for &byte in android.artifact.as_slice() {
            if byte == b' ' {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out.push(b'\n');
        let linked = [&android.shared, &android.static_libraries, &android.ldlibs];
        if !target.properties.is_empty() || linked.iter().any(|items| !items.is_empty()) {
            out.extend_from_slice(b"LOCAL_CPP_FEATURES := rtti exceptions\n");
            let mut properties = target.properties.iter().collect::<Vec<_>>();
            let link_key = BString::from("INTERFACE_LINK_LIBRARIES");
            let empty = BString::default();
            if !target.properties.contains_key(&link_key) {
                properties.push((&link_key, &empty));
            }
            properties.sort_unstable_by_key(|(key, _)| *key);
            for (key, value) in properties {
                match key.as_slice() {
                    b"INTERFACE_COMPILE_OPTIONS" => {
                        line(&mut out, b"LOCAL_CPP_FEATURES += ", value)
                    }
                    b"INTERFACE_INCLUDE_DIRECTORIES" => {
                        out.extend_from_slice(b"LOCAL_EXPORT_C_INCLUDES := ");
                        for (index, item) in list::iter(value, list::Empty::Drop).enumerate() {
                            if index > 0 {
                                out.extend_from_slice(b"\\\n");
                            }
                            out.extend_from_slice(&item);
                        }
                        out.push(b'\n');
                    }
                    b"INTERFACE_LINK_LIBRARIES" => links(&mut out, android),
                    b"INTERFACE_LINK_OPTIONS" => {
                        let flags = list::split(value, list::Empty::Drop)
                            .iter()
                            .map(|flag| flag.as_slice())
                            .collect::<Vec<_>>()
                            .join(&b' ');
                        line(&mut out, b"LOCAL_EXPORT_LDFLAGS := ", &flags);
                    }
                    _ => {
                        out.extend_from_slice(b"# ");
                        out.extend_from_slice(key);
                        line(&mut out, b" ", value);
                    }
                }
            }
        }
        if android.has_cpp {
            out.extend_from_slice(b"LOCAL_HAS_CPP := true\n");
        }
        out.extend_from_slice(match target.kind.as_slice() {
            b"SHARED" | b"MODULE" => b"include $(PREBUILT_SHARED_LIBRARY)\n",
            b"STATIC" => b"include $(PREBUILT_STATIC_LIBRARY)\n",
            _ => b"",
        });
        out.push(b'\n');
    }
    out
}

fn line(out: &mut BString, prefix: &[u8], value: &[u8]) {
    out.extend_from_slice(prefix);
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn links(out: &mut BString, target: &AndroidExportTarget) {
    for (name, items) in [
        (b"LOCAL_SHARED_LIBRARIES :=".as_slice(), &target.shared),
        (b"LOCAL_STATIC_LIBRARIES :=", &target.static_libraries),
        (b"LOCAL_EXPORT_LDLIBS :=", &target.ldlibs),
    ] {
        if !items.is_empty() {
            out.extend_from_slice(name);
            for item in items {
                out.push(b' ');
                out.extend_from_slice(item);
            }
            out.push(b'\n');
        }
    }
}
