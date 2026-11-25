//! Read the install name from a Mach-O library.

use std::path::Path;

use rcmake_host::{Host, ReadRange};

pub(super) fn uses_rpath(host: &dyn Host, file: &Path) -> bool {
    let Some(header) = read(host, file, 0, 32) else {
        return false;
    };
    let (little, wide) = match header.get(..4) {
        Some([0xca, 0xfe, 0xba, 0xbe]) => (false, false),
        Some([0xca, 0xfe, 0xba, 0xbf]) => (false, true),
        Some([0xbe, 0xba, 0xfe, 0xca]) => (true, false),
        Some([0xbf, 0xba, 0xfe, 0xca]) => (true, true),
        _ => return thin_uses_rpath(host, file, 0, &header).unwrap_or_default(),
    };
    let Some(count) = number(&header, 4, little).filter(|&count| count <= 64) else {
        return false;
    };
    let size = if wide { 32 } else { 20 };
    let Some(table) = read(host, file, 8, u64::from(count) * size) else {
        return false;
    };
    table.chunks_exact(size as usize).any(|entry| {
        let offset = if wide {
            entry
                .get(8..16)
                .and_then(|part| part.try_into().ok())
                .map(|part| {
                    if little {
                        u64::from_le_bytes(part)
                    } else {
                        u64::from_be_bytes(part)
                    }
                })
        } else {
            number(entry, 8, little).map(u64::from)
        };
        offset
            .and_then(|offset| {
                let header = read(host, file, offset, 32)?;
                thin_uses_rpath(host, file, offset, &header)
            })
            .unwrap_or_default()
    })
}

fn thin_uses_rpath(host: &dyn Host, file: &Path, offset: u64, header: &[u8]) -> Option<bool> {
    let (little, size) = match header.get(..4)? {
        [0xce, 0xfa, 0xed, 0xfe] => (true, 28),
        [0xcf, 0xfa, 0xed, 0xfe] => (true, 32),
        [0xfe, 0xed, 0xfa, 0xce] => (false, 28),
        [0xfe, 0xed, 0xfa, 0xcf] => (false, 32),
        _ => return None,
    };
    let count = number(header, 16, little)?;
    let length = number(header, 20, little)?;
    if length > 4 * 1024 * 1024 || count > length / 8 {
        return None;
    }
    let data = read(host, file, offset.checked_add(size)?, u64::from(length))?;
    let mut commands = data.as_slice();
    for _ in 0..count {
        let command = number(commands, 0, little)?;
        let size = number(commands, 4, little)? as usize;
        if size < 8 {
            return None;
        }
        let entry = commands.get(..size)?;
        if command == 0x0d {
            let name = number(entry, 8, little)? as usize;
            return Some(entry.get(name..)?.starts_with(b"@rpath/"));
        }
        commands = commands.get(size..)?;
    }
    Some(false)
}

fn number(data: &[u8], offset: usize, little: bool) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn read(host: &dyn Host, file: &Path, offset: u64, len: u64) -> Option<Vec<u8>> {
    host.read(
        file,
        ReadRange::Slice {
            offset,
            len: Some(len),
        },
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn reads_the_rpath_install_name_from_thin_and_universal_libraries()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::default();
        let path = Path::new("/library.dylib");
        let mut thin = Vec::new();
        for word in [0xfeed_facf_u32, 0, 0, 6, 1, 48, 0, 0, 0x0d, 48, 24, 0, 0, 0] {
            thin.extend_from_slice(&word.to_le_bytes());
        }
        thin.extend_from_slice(b"@rpath/library.dylib\0\0\0\0");
        host.seed_file(path, &thin)?;
        assert!(uses_rpath(&host, path));
        let mut universal = Vec::new();
        for word in [0xcafe_babe_u32, 1, 0, 0, 28, thin.len() as u32, 0] {
            universal.extend_from_slice(&word.to_be_bytes());
        }
        universal.extend_from_slice(&thin);
        host.seed_file(path, &universal)?;
        assert!(uses_rpath(&host, path));
        host.seed_file(path, b"\xcf\xfa\xed\xfe")?;
        assert!(!uses_rpath(&host, path));
        Ok(())
    }
}
