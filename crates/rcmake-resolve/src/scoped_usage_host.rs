use rcmake_genex::TargetHandle;
use rcmake_model::DirId;

use crate::usage::{ModelGenexHost, UsageResolver};

impl<'a> UsageResolver<'a> {
    /// Borrow a generator expression host for a property entry in its lookup directory.
    pub fn host_at(
        &mut self,
        lookup_dir: DirId,
        current: Option<TargetHandle>,
    ) -> ModelGenexHost<'a, '_> {
        let mut host = self.host(lookup_dir);
        host.current = current;
        host
    }
}
