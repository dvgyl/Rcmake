use rcmake_base::{BString, Limits};
use rcmake_host::{ReadRange, path::to_native};

use crate::config::{CpackConfig, absolute};
use crate::{CpackError, PackageContext, error};

const LICENSE_DELIMITER: &str = "____cpack__here_doc____";
const PAYLOAD_LINE: &str = "###CPACK_HEADER_LENGTH###";

pub(super) fn header(cx: &PackageContext<'_>, cfg: &CpackConfig) -> Result<BString, CpackError> {
    let root = cfg
        .get(b"CMAKE_ROOT")
        .ok_or_else(|| error("Cannot locate the STGZ installer template"))?;
    let template = to_native(root).join("Modules/Internal/CPack/CPack.STGZ_Header.sh.in");
    let range = || ReadRange::All {
        limit: Limits::default().max_file_read,
    };
    let template = cx.host.read(&template, range())?;
    let template = std::str::from_utf8(&template)
        .map_err(|_| error("The STGZ installer template must be valid UTF-8"))?;
    let license = cfg
        .get(b"CPACK_RESOURCE_FILE_LICENSE")
        .filter(|value| !value.is_empty())
        .map(|value| {
            cx.host
                .read(&absolute(&cx.host.initial_cwd(), value), range())
        })
        .transpose()?
        .unwrap_or_default();
    render(template, &license, cfg, cx.file_name)
}

fn render(
    template: &str,
    license: &[u8],
    cfg: &CpackConfig,
    package_file_name: &[u8],
) -> Result<BString, CpackError> {
    if package_file_name
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0))
    {
        return Err(error(
            "An STGZ package file name cannot contain a line break or a null byte",
        ));
    }
    let mut delimiter = LICENSE_DELIMITER.to_owned();
    let license_lines = license
        .split(|&byte| byte == b'\n')
        .filter(|line| line.starts_with(LICENSE_DELIMITER.as_bytes()))
        .collect::<std::collections::BTreeSet<_>>();
    for suffix in 0..=license_lines.len() {
        if !license_lines.contains(delimiter.as_bytes()) {
            break;
        }
        delimiter = format!("{LICENSE_DELIMITER}{suffix}");
    }
    let template = template.replace(LICENSE_DELIMITER, &delimiter);
    let mut header = BString::default();
    let mut payload = None;
    for (i, part) in template.split('@').enumerate() {
        if i % 2 == 0 {
            if let Some((before, after)) = part.split_once(PAYLOAD_LINE) {
                header.extend_from_slice(before.as_bytes());
                payload = Some(header.len());
                header.extend_from_slice(PAYLOAD_LINE.as_bytes());
                header.extend_from_slice(after.as_bytes());
            } else {
                header.extend_from_slice(part.as_bytes());
            }
            continue;
        }
        match part {
            "CPACK_AT_SIGN" => header.push(b'@'),
            "CPACK_RESOURCE_FILE_LICENSE_CONTENT" => header.extend_from_slice(license),
            "CPACK_PACKAGE_FILE_NAME" => write_shell_text(&mut header, package_file_name),
            _ => write_shell_text(&mut header, cfg.get(part.as_bytes()).unwrap_or_default()),
        }
    }
    if !header.ends_with(b"\n") {
        header.push(b'\n');
    }
    let line = 1 + header.iter().filter(|&&byte| byte == b'\n').count();
    let start =
        payload.ok_or_else(|| error("The STGZ installer template has no payload marker"))?;
    let line = line.to_string();
    header.splice(start..start + PAYLOAD_LINE.len(), line.bytes());
    Ok(header)
}

fn write_shell_text(out: &mut BString, text: &[u8]) {
    for &byte in text {
        if matches!(byte, b'\\' | b'"' | b'$' | b'`') {
            out.push(b'\\');
        }
        out.push(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_payload_line_after_a_multiline_license() -> Result<(), CpackError> {
        let rendered = render(
            "usage @CPACK_PACKAGE_FILE_NAME@\n<< '____cpack__here_doc____'\n@CPACK_RESOURCE_FILE_LICENSE_CONTENT@\n____cpack__here_doc____\ntail +###CPACK_HEADER_LENGTH###\n",
            b"First line\n____cpack__here_doc____\nLast line",
            &CpackConfig::default(),
            b"archive \\ \" ` $",
        )?;
        assert_eq!(
            rendered,
            b"usage archive \\\\ \\\" \\` \\$\n<< '____cpack__here_doc____0'\nFirst line\n____cpack__here_doc____\nLast line\n____cpack__here_doc____0\ntail +8\n".as_slice()
        );
        Ok(())
    }
}
