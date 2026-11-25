//! CMake file permission keywords.

/// Return the POSIX mode bit for a CMake permission keyword.
#[must_use]
pub fn parse_permission(value: &[u8]) -> Option<u32> {
    Some(match value {
        b"SETUID" => 0o4000,
        b"SETGID" => 0o2000,
        b"OWNER_READ" => 0o400,
        b"OWNER_WRITE" => 0o200,
        b"OWNER_EXECUTE" => 0o100,
        b"GROUP_READ" => 0o040,
        b"GROUP_WRITE" => 0o020,
        b"GROUP_EXECUTE" => 0o010,
        b"WORLD_READ" => 0o004,
        b"WORLD_WRITE" => 0o002,
        b"WORLD_EXECUTE" => 0o001,
        _ => return None,
    })
}
