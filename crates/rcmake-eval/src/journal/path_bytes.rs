//! Serialization for native paths on the current platform.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serializer};

pub(super) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes: Vec<_> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        serializer.serialize_bytes(&bytes)
    }
    #[cfg(not(windows))]
    {
        serializer.serialize_bytes(path.as_os_str().as_encoded_bytes())
    }
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(decoder: D) -> Result<PathBuf, D::Error> {
    let bytes = Vec::<u8>::deserialize(decoder)?;
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let (pairs, rest) = bytes.as_chunks::<2>();
        if !rest.is_empty() {
            return Err(serde::de::Error::custom("Invalid Windows path encoding."));
        }
        let wide: Vec<_> = pairs.iter().copied().map(u16::from_le_bytes).collect();
        Ok(OsString::from_wide(&wide).into())
    }
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes).into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(serde::de::Error::custom)
    }
}
