//! Native MSBuild unity builds (`UNITY_BUILD`), as CMake writes them.
//!
//! CMake lists each member source of a unity batch in the project with
//! `IncludeInUnityFile="true"`, and the generated unity source with
//! `IncludeInUnityFile="false"`. Both have `CustomUnityFile="true"` and the
//! directory of the unity source in `UnityFilesDirectory`. MSBuild then
//! compiles the members through the unity source that CMake generated.

use rcmake_base::{path_lex::Lexical, truth};
use rcmake_model::Target;
use rcmake_resolve::{
    ResolvedTarget,
    ide::{IdeFacts, IdeSource, IdeSourceKind},
};

use super::{Context, Kind, Plan};
use crate::{GenError, absolute, plan::RegexMatcher, string};

/// The unity build metadata of one `ClCompile` item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnitySource {
    /// The source is a member that MSBuild includes in the unity source.
    /// The generated unity source itself has `false`.
    pub(crate) include: bool,
    /// The directory of the unity source, with `/` separators.
    pub(crate) dir: String,
}

impl Context<'_> {
    /// Tell if the target uses a unity build (`UNITY_BUILD`).
    pub(crate) fn unity_build(&self) -> bool {
        self.project
            .target
            .and_then(|id| self.model.target(id))
            .and_then(|target| target.props.get(b"UNITY_BUILD"))
            .is_some_and(|value| truth::is_on(value))
    }

    /// Make IDE sources for the unity members that the target settings do
    /// not list. The group of each member is its source group.
    ///
    /// # Errors
    ///
    /// Returns an error when the source group expressions use up the step
    /// budget.
    pub(super) fn unity_members(
        &self,
        ide: &IdeFacts,
        target: &Target,
    ) -> Result<Vec<IdeSource>, GenError> {
        let dir = self.model.dir(target.dir);
        let mut matcher = RegexMatcher::default();
        let mut members: Vec<IdeSource> = Vec::new();
        for unity in &ide.unity_files {
            for member in &unity.member_sources {
                let listed = ide.sources.iter().any(|source| source.path == *member)
                    || members.iter().any(|source| source.path == *member);
                if listed {
                    continue;
                }
                let group =
                    rcmake_resolve::ide::find_source_group(dir, member, &mut |pattern, text| {
                        matcher.matches(pattern, text)
                    });
                matcher.check()?;
                members.push(IdeSource {
                    path: member.clone(),
                    kind: IdeSourceKind::Compile {
                        language: unity.language.clone(),
                    },
                    group,
                    generated: false,
                    excluded_configs: Vec::new(),
                    file_set: None,
                    package_location: None,
                    skip_precompile_headers: false,
                    compile: None,
                });
            }
        }
        Ok(members)
    }

    /// Set the unity metadata of the sources of configuration `config`.
    ///
    /// As in CMake:
    ///
    /// * A member source compiles through its unity source. With the `FULL`
    ///   intermediate directory strategy, it has no object file name of its
    ///   own. With `SHORT`, it keeps the hashed object name of its source.
    /// * A CUDA member never compiles on its own, so MSBuild does not build
    ///   both the `.cu` file and the unity source.
    /// * A toolset older than `v141` has no native unity builds. Then the
    ///   members are excluded from the build, and the unity sources compile
    ///   as other sources.
    pub(super) fn mark_unity_sources(
        &self,
        plan: &mut Plan,
        (ide, fact): (&IdeFacts, Option<&ResolvedTarget>),
        members: &[IdeSource],
        config: &str,
    ) {
        if !self.unity_build() {
            return;
        }
        let native = native_unity(&self.settings.toolset);
        let short = self
            .model
            .cache
            .get(b"CMAKE_INTERMEDIATE_DIR_STRATEGY")
            .is_some_and(|entry| entry.value == "SHORT");
        let target = self.project.target.and_then(|id| self.model.target(id));
        for unity in &ide.unity_files {
            let unity_path = self.source_path(&string(&unity.path));
            let dir = string(Lexical::new(unity_path.as_bytes()).parent());
            if native && let Some(item) = plan.items.get_mut(&(Kind::Compile, unity_path.clone())) {
                item.unity = Some(UnitySource {
                    include: false,
                    dir: dir.clone(),
                });
            }
            for member in &unity.member_sources {
                let path = self.source_path(&string(member));
                if let Some(item) = plan.items.get_mut(&(Kind::Cuda, path.clone())) {
                    if let Some(source) = item.configs.get_mut(config) {
                        source.excluded = true;
                    }
                    continue;
                }
                let Some(item) = plan.items.get_mut(&(Kind::Compile, path)) else {
                    continue;
                };
                if members.iter().any(|source| source.path == *member)
                    && let Some(source) = item.configs.get_mut(config)
                {
                    source.object = match (short, target, fact) {
                        (true, Some(target), Some(fact)) => absolute(
                            &self.settings.bin,
                            &string(&rcmake_resolve::lower::object_path(
                                self.model,
                                target,
                                &fact.config,
                                member,
                            )),
                        ),
                        _ => String::new(),
                    };
                    if !native {
                        source.excluded = true;
                    }
                }
                if native {
                    item.unity = Some(UnitySource {
                        include: true,
                        dir: dir.clone(),
                    });
                }
            }
        }
    }
}

/// Tell if the platform toolset has native MSBuild unity builds, as CMake
/// decides: an empty toolset, or a toolset version of at least `141`.
///
/// CMake removes each `v` and compares the rest as a version, so `v143`
/// is `143`, and a name without a leading number, such as `ClangCL`, is
/// `0`.
pub(crate) fn native_unity(toolset: &str) -> bool {
    let version = toolset.replace('v', "");
    if version.is_empty() {
        return true;
    }
    let digits: String = version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().is_ok_and(|major| major >= 141)
}

#[cfg(test)]
mod tests {
    use super::native_unity;

    #[test]
    fn native_unity_needs_toolset_141_or_later() {
        assert!(native_unity(""));
        assert!(native_unity("v141"));
        assert!(native_unity("v143"));
        assert!(native_unity("v145"));
        assert!(!native_unity("v140"));
        assert!(!native_unity("ClangCL"));
    }
}
