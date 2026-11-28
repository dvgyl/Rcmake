use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;

use super::{Result, fail, keyword};

const CMP0208: PolicyId = PolicyId::new(208);
const CMP0103: PolicyId = PolicyId::new(103);

pub(super) fn normalize<'a>(cx: &mut Cx<'_>, args: &[&'a [u8]]) -> Result<Vec<&'a [u8]>> {
    let mut parsed = Vec::with_capacity(args.len() + 1);
    let mut missing = Vec::new();
    let mut words = args.iter().copied().peekable();
    while let Some(key) = words.next() {
        if !matches!(
            key,
            b"EXPORT" | b"FILE" | b"NAMESPACE" | b"CXX_MODULES_DIRECTORY"
        ) {
            parsed.push(key);
            continue;
        }
        let value = words.next_if(|word| !keyword(word));
        match value {
            Some(value) if key == b"NAMESPACE" || !value.is_empty() => {
                parsed.extend([key, value]);
            }
            invalid => {
                let reason = if invalid.is_some() {
                    "empty value is not allowed"
                } else {
                    "missing required value"
                };
                missing.push(format!("  {}: {reason}", String::from_utf8_lossy(key)));
                if key == b"EXPORT" {
                    parsed.extend([key, b""]);
                }
            }
        }
    }
    if !missing.is_empty() {
        let message = format!("EXPORT given invalid argument:\n{}", missing.join("\n"));
        match cx.policy(CMP0208) {
            PolicyStatus::New => return Err(fail(message)),
            PolicyStatus::Warn => {
                crate::author_warning(cx, BString::from(format!("export {message}")));
                cx.issue_policy_warning(CMP0208, b"", b"");
            }
            PolicyStatus::Old => {}
        }
    }
    Ok(parsed)
}

pub(super) fn output_path(binary_dir: &[u8], file: &[u8], windows: bool) -> BString {
    let normalize_seps = |input: &[u8]| -> BString {
        if !windows {
            return input.into();
        }
        let slashes: Vec<u8> = input
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect();
        if let Some(verbatim) = slashes.strip_prefix(b"//?/") {
            if let Some(unc) = verbatim
                .get(..4)
                .filter(|prefix| prefix.eq_ignore_ascii_case(b"UNC/"))
                .and_then(|_| verbatim.get(4..))
            {
                return [b"//".as_slice(), unc].concat().into();
            }
            if matches!(verbatim, [drive, b':', b'/', ..] if drive.is_ascii_alphabetic()) {
                return verbatim.into();
            }
        }
        slashes.into()
    };
    let base = normalize_seps(binary_dir);
    let target = normalize_seps(file);
    let joined = if windows {
        Lexical::windows(&base).append(&target)
    } else {
        Lexical::new(&base).append(&target)
    };
    if windows {
        Lexical::windows(&joined).normalize()
    } else {
        Lexical::new(&joined).normalize()
    }
}

pub(super) fn repeat(cx: &mut Cx<'_>, file: &BString) -> Result<()> {
    let message =
        format!("command already specified for the file\n  {file}\nDid you miss 'APPEND' keyword?");
    match cx.policy(CMP0103) {
        PolicyStatus::New => Err(fail(message)),
        PolicyStatus::Warn => {
            let warning = format!("export() {message}");
            cx.issue_policy_warning(CMP0103, b"", warning.as_bytes());
            Ok(())
        }
        PolicyStatus::Old => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::output_path;

    #[test]
    fn windows_export_output_key_matches_relative_and_cmake_binary_dir_paths() {
        let root_build = br"C:\Users\runner\Repeat-CMP0103-WARN-build";
        let sub_build = br"C:\Users\runner\Repeat-CMP0103-WARN-build\Repeat";
        let from_relative = output_path(root_build, b"foo.cmake", true);
        let from_cmake_var = output_path(
            sub_build,
            b"C:/Users/runner/Repeat-CMP0103-WARN-build/foo.cmake",
            true,
        );
        assert_eq!(from_relative, from_cmake_var);
        assert_eq!(
            from_relative,
            "C:/Users/runner/Repeat-CMP0103-WARN-build/foo.cmake"
        );
    }

    #[test]
    fn windows_export_output_key_normalizes_verbatim_drive_and_unc_only() {
        let build = br"C:\Users\runner\build";
        let verbatim_drive = output_path(build, br"\\?\C:\Users\runner\build\foo.cmake", true);
        assert_eq!(verbatim_drive, output_path(build, b"foo.cmake", true));

        let unc_build = br"\\host\share\build";
        let verbatim_unc = output_path(unc_build, br"\\?\UNC\host\share\build\foo.cmake", true);
        assert_eq!(verbatim_unc, output_path(unc_build, b"foo.cmake", true));

        let volume_guid = output_path(
            build,
            br"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\exports\out.cmake",
            true,
        );
        let relative_like = output_path(
            build,
            b"Volume{01234567-89ab-cdef-0123-456789abcdef}/exports/out.cmake",
            true,
        );
        assert_ne!(volume_guid, relative_like);
        assert_eq!(
            volume_guid,
            "//?/Volume{01234567-89ab-cdef-0123-456789abcdef}/exports/out.cmake"
        );
        assert_eq!(
            relative_like,
            "C:/Users/runner/build/Volume{01234567-89ab-cdef-0123-456789abcdef}/exports/out.cmake"
        );
    }
}
