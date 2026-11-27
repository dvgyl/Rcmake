//! Plain-text to RTF license conversion for the CPack WiX generator.

use rcmake_base::CMAKE_VERSION_STR;

/// Convert plain-text license bytes into a WiX-compatible RTF document, matching CMake's
/// `cmWIXRichTextFormatWriter` handling of valid UTF-8 and invalid legacy bytes.
pub(super) fn plain_text_bytes_to_rtf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(b"{\\rtf1\\ansi\\ansicpg1252\\deff0\\deflang1033");
    out.extend_from_slice(b"{\\fonttbl{\\f0\\fswiss\\fcharset0 Consolas;}}");
    out.extend_from_slice(
        b"{\\colortbl ;\\red255\\green0\\blue0;\\red0\\green255\\blue0;\\red0\\green0\\blue255;}",
    );
    out.extend_from_slice(
        format!("{{\\*\\generator CPack WiX Generator ({CMAKE_VERSION_STR});}}").as_bytes(),
    );
    out.extend_from_slice(b"\\viewkind4\\uc1\\pard\\f0\\fs14");

    let mut i = 0;
    while let Some(&c) = bytes.get(i) {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'{' => out.extend_from_slice(b"\\{"),
            b'}' => out.extend_from_slice(b"\\}"),
            b'\n' => out.extend_from_slice(b"\\par\r\n"),
            b'\r' => {}
            0..=0x7F => out.push(c),
            0x80..=0xC0 => emit_invalid_byte(&mut out, c),
            0xC1..=0xDF => {
                if let Some(&b1) = bytes.get(i + 1) {
                    let code = u32::from(b1 & 0x3F) | (u32::from(c & 0x1F) << 6);
                    emit_unicode(&mut out, code);
                    i += 1;
                } else {
                    emit_invalid_byte(&mut out, c);
                }
            }
            0xE0..=0xEF => {
                if let (Some(&b1), Some(&b2)) = (bytes.get(i + 1), bytes.get(i + 2)) {
                    let code = u32::from(b2 & 0x3F)
                        | (u32::from(b1 & 0x3F) << 6)
                        | (u32::from(c & 0x0F) << 12);
                    emit_unicode(&mut out, code);
                    i += 2;
                } else {
                    emit_invalid_byte(&mut out, c);
                }
            }
            0xF0..=0xF7 => {
                if let (Some(&b1), Some(&b2), Some(&b3)) =
                    (bytes.get(i + 1), bytes.get(i + 2), bytes.get(i + 3))
                {
                    let code = u32::from(b3 & 0x3F)
                        | (u32::from(b2 & 0x3F) << 6)
                        | (u32::from(b1 & 0x3F) << 12)
                        | (u32::from(c & 0x07) << 18);
                    emit_unicode(&mut out, code);
                    i += 3;
                } else {
                    emit_invalid_byte(&mut out, c);
                }
            }
            0xF8..=0xFF => emit_invalid_byte(&mut out, c),
        }
        i += 1;
    }

    if !bytes.ends_with(b"\n") && !bytes.is_empty() {
        out.extend_from_slice(b"\\par\r\n");
    }

    out.extend_from_slice(b"}\r\n");
    out.push(0);
    out
}

fn emit_unicode(out: &mut Vec<u8>, code: u32) {
    if code == 0xFEFF {
        return;
    }
    if code <= 0xFFFF {
        emit_surrogate(out, code);
    } else {
        let adjusted = code - 0x10000;
        emit_surrogate(out, ((adjusted >> 10) & 0x3FF) + 0xD800);
        emit_surrogate(out, (adjusted & 0x3FF) + 0xDC00);
    }
}

fn emit_surrogate(out: &mut Vec<u8>, unit: u32) {
    let signed = if unit <= 32767 {
        unit as i32
    } else {
        (unit as i32) - 65536
    };
    out.extend_from_slice(format!("\\u{signed}?").as_bytes());
}

fn emit_invalid_byte(out: &mut Vec<u8>, byte: u8) {
    out.extend_from_slice(format!("\\cf1 [INVALID-BYTE-{byte}]\\cf0 ").as_bytes());
}
