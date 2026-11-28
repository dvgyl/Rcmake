//! Solution project type GUIDs.
//!
//! A solution entry names the kind of its project file with a type GUID. For
//! example, a `.vcxproj` file has the C++ type and a `.csproj` file has the C#
//! type. The values have no braces, as CMake stores them.

/// C++ project (`.vcxproj`). CMake also uses this type for unknown files.
pub(crate) const DEFAULT: &str = "8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942";
/// C# project (`.csproj`).
pub(crate) const CSHARP: &str = "FAE04EC0-301F-11D3-BF4B-00C04F79EFBC";
/// .NET Core project.
pub(crate) const DOTNET_CORE: &str = "9A19103F-16F7-4668-BE54-9A1E7A4F7556";
/// Solution folder.
pub(crate) const FOLDER: &str = "2150E333-8FDC-42A3-9474-1A3956D46DE8";

/// Project file extensions that set the type of an external project.
const EXTERNAL: [(&str, &str); 10] = [
    (".vfproj", "6989167D-11E4-40FE-8C1A-2192A86A7E90"),
    (".vbproj", "F184B08F-C81C-45F6-A57F-5ABD9991F28F"),
    (".csproj", CSHARP),
    (".fsproj", "F2A71F9B-5D33-465A-A702-920D77279786"),
    (".vdproj", "54435603-DBB4-11D2-8724-00A0C9A8B90C"),
    (".dbproj", "C8D11400-126E-41CD-887F-60BD40844F9E"),
    (".njsproj", "9092AA53-FB77-4645-B42D-1CCCA6BD08BD"),
    (".wapproj", "C7167F0D-BC9F-4E6E-AFE1-012C56B48DB5"),
    (".wixproj", "930C7802-8A8C-48F9-8165-68863BCCD9DD"),
    (".pyproj", "888888A0-9F3D-457C-B088-3A5042F75D52"),
];

/// Get the type of an external project from the last extension of its path.
///
/// The comparison is case-sensitive, as in CMake. An unknown extension gives
/// [`DEFAULT`].
pub(crate) fn external(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let extension = name.rfind('.').and_then(|index| name.get(index..));
    EXTERNAL
        .iter()
        .find(|(known, _)| Some(*known) == extension)
        .map_or(DEFAULT, |(_, id)| id)
}

/// Return `true` when the solution must name the project platform, also when
/// it is the same as the solution platform.
///
/// Some managed projects do not build in the IDE without this platform.
pub(crate) fn explicit_platform(type_id: &str) -> bool {
    type_id == CSHARP || type_id == DOTNET_CORE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_type_follows_the_last_extension() {
        for (path, id) in [
            ("external.csproj", CSHARP),
            (
                "dir/external.dbproj",
                "C8D11400-126E-41CD-887F-60BD40844F9E",
            ),
            (
                "a.b/external.fsproj",
                "F2A71F9B-5D33-465A-A702-920D77279786",
            ),
            ("external.njsproj", "9092AA53-FB77-4645-B42D-1CCCA6BD08BD"),
            ("external.pyproj", "888888A0-9F3D-457C-B088-3A5042F75D52"),
            ("external.vbproj", "F184B08F-C81C-45F6-A57F-5ABD9991F28F"),
            ("external.vdproj", "54435603-DBB4-11D2-8724-00A0C9A8B90C"),
            ("external.vfproj", "6989167D-11E4-40FE-8C1A-2192A86A7E90"),
            ("external.wapproj", "C7167F0D-BC9F-4E6E-AFE1-012C56B48DB5"),
            ("external.wixproj", "930C7802-8A8C-48F9-8165-68863BCCD9DD"),
            ("external.project", DEFAULT),
            ("external.CSPROJ", DEFAULT),
            ("dir.csproj/external", DEFAULT),
            ("external", DEFAULT),
        ] {
            assert_eq!(external(path), id, "{path}");
        }
    }

    #[test]
    fn managed_types_need_an_explicit_platform() {
        assert!(explicit_platform(CSHARP));
        assert!(explicit_platform(DOTNET_CORE));
        assert!(!explicit_platform(DEFAULT));
        assert!(!explicit_platform("aaa-bbb-ccc-ddd-eee"));
    }
}
