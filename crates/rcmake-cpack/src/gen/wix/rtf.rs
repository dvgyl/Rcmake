//! Plain-text to RTF license conversion for the CPack WiX generator.

use rcmake_base::CMAKE_VERSION_STR;

/// Convert UTF-8 plain text license content into a valid WiX-compatible RTF document.
pub(super) fn plain_text_to_rtf(text: &str) -> Vec<u8> {
    let mut out = String::with_capacity(text.len() + 256);
    out.push_str("{\\rtf1\\ansi\\ansicpg1252\\deff0\\deflang1033");
    out.push_str("{\\fonttbl{\\f0\\fswiss\\fcharset0 Consolas;}}");
    out.push_str(
        "{\\colortbl ;\\red255\\green0\\blue0;\\red0\\green255\\blue0;\\red0\\green0\\blue255;}",
    );
    out.push_str(&format!(
        "{{\\*\\generator CPack WiX Generator ({CMAKE_VERSION_STR});}}"
    ));
    out.push_str("\\viewkind4\\uc1\\pard\\f0\\fs14");

    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\par\r\n"),
            '\r' => {}
            '\u{FEFF}' => {}
            c if (c as u32) <= 0x7F => out.push(c),
            c => emit_unicode(&mut out, c as u32),
        }
    }

    if !text.ends_with('\n') && !text.is_empty() {
        out.push_str("\\par\r\n");
    }

    out.push_str("}\r\n");
    let mut bytes = out.into_bytes();
    bytes.push(0);
    bytes
}

fn emit_unicode(out: &mut String, code: u32) {
    if code <= 0xFFFF {
        emit_surrogate(out, code);
    } else {
        let adjusted = code - 0x10000;
        emit_surrogate(out, ((adjusted >> 10) & 0x3FF) + 0xD800);
        emit_surrogate(out, (adjusted & 0x3FF) + 0xDC00);
    }
}

fn emit_surrogate(out: &mut String, unit: u32) {
    let signed = if unit <= 32767 {
        unit as i32
    } else {
        (unit as i32) - 65536
    };
    out.push_str(&format!("\\u{signed}?"));
}
