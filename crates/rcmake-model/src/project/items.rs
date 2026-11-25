use std::collections::BTreeSet;

use rcmake_base::BString;

use super::{ModelError, ProjectModel, push_arena};
use crate::{CommandId, CustomCommand, DirId, FileSet, FileSetId, InstallRule, TestEntry, TestId};

impl ProjectModel {
    /// Add a custom command.
    pub fn add_command(&mut self, command: CustomCommand) -> Result<CommandId, ModelError> {
        push_arena(&mut self.commands, command, CommandId::from_index)
    }

    /// Custom command `id`.
    #[must_use]
    pub fn command(&self, id: CommandId) -> Option<&CustomCommand> {
        self.commands.get(id.index())
    }

    /// Mutable custom command `id` (`add_custom_command(APPEND)`).
    pub fn command_mut(&mut self, id: CommandId) -> Option<&mut CustomCommand> {
        self.commands.get_mut(id.index())
    }

    /// All custom commands in creation order.
    pub fn commands(&self) -> impl Iterator<Item = (CommandId, &CustomCommand)> {
        self.commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| CommandId::from_index(i).map(|id| (id, c)))
    }

    /// Add a test to its directory. Returns [`ModelError::DuplicateTest`] when the
    /// directory already has a test with the same name. A `discover_tests()` entry
    /// ([`crate::TestKind::Discover`]) goes into [`crate::Directory::discoveries`] and never
    /// conflicts with a name.
    pub fn add_test(&mut self, test: TestEntry) -> Result<TestId, ModelError> {
        let dir = test.dir;
        if test.is_discovery() {
            let id = push_arena(&mut self.tests, test, TestId::from_index)?;
            self.dir_mut(dir).discoveries.push(id);
            return Ok(id);
        }
        if let Some(&existing) = self.dir(dir).tests.get(&test.name) {
            return Err(ModelError::DuplicateTest(existing));
        }
        let name = test.name.clone();
        let id = push_arena(&mut self.tests, test, TestId::from_index)?;
        self.dir_mut(dir).tests.insert(name, id);
        Ok(id)
    }

    /// Tests and `discover_tests()` entries of directory `dir` in call order.
    pub fn dir_tests(&self, dir: DirId) -> impl Iterator<Item = (TestId, &TestEntry)> {
        let directory = self.dir(dir);
        let mut ids: Vec<TestId> = directory
            .tests
            .values()
            .chain(&directory.discoveries)
            .copied()
            .collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.test(id).map(|test| (id, test)))
    }

    /// Test `id`.
    #[must_use]
    pub fn test(&self, id: TestId) -> Option<&TestEntry> {
        self.tests.get(id.index())
    }

    /// Mutable test `id`.
    pub fn test_mut(&mut self, id: TestId) -> Option<&mut TestEntry> {
        self.tests.get_mut(id.index())
    }

    /// Test `name` of directory `dir` (`if(TEST)` looks only in the current directory).
    #[must_use]
    pub fn find_test(&self, dir: DirId, name: &[u8]) -> Option<TestId> {
        self.dir(dir).tests.get(name).copied()
    }

    /// Add a file set and attach it to its target.
    pub fn add_file_set(&mut self, file_set: FileSet) -> Result<FileSetId, ModelError> {
        let target = file_set.target;
        if self.target(target).is_none() {
            return Err(ModelError::UnknownObject);
        }
        let id = push_arena(&mut self.file_sets, file_set, FileSetId::from_index)?;
        if let Some(t) = self.target_mut(target) {
            t.file_sets.push(id);
        }
        Ok(id)
    }

    /// File set `id`.
    #[must_use]
    pub fn file_set(&self, id: FileSetId) -> Option<&FileSet> {
        self.file_sets.get(id.index())
    }

    /// Mutable file set `id`.
    pub fn file_set_mut(&mut self, id: FileSetId) -> Option<&mut FileSet> {
        self.file_sets.get_mut(id.index())
    }

    /// Add an install rule. Returns its index in [`Self::installs`].
    pub fn add_install(&mut self, rule: InstallRule) -> usize {
        self.installs.push(rule);
        self.installs.len() - 1
    }

    /// Names of all install components (`get_cmake_property(COMPONENTS)`), sorted. A
    /// rule without `COMPONENT` uses `CMAKE_INSTALL_DEFAULT_COMPONENT_NAME` of its
    /// directory, else `Unspecified`. Empty when the project has no install rules.
    #[must_use]
    pub fn install_components(&self) -> BTreeSet<BString> {
        self.installs
            .iter()
            .map(|rule| match &rule.options.component {
                Some(component) => component.clone(),
                None => self
                    .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| BString::from("Unspecified")),
            })
            .collect()
    }
}
