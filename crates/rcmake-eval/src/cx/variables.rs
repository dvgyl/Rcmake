use rcmake_base::BString;

use super::Cx;

impl Cx<'_> {
    /// Get the sorted, unique names of visible variables and cache entries.
    #[must_use]
    pub fn variable_names(&self) -> Vec<BString> {
        let mut names = self.variables_property_names();
        names.dedup();
        names
    }

    /// Get the sorted names of visible variables and cache entries for VARIABLES.
    /// A name can occur once per type of entry.
    #[must_use]
    pub fn variables_property_names(&self) -> Vec<BString> {
        let mut names = self.interp.vars.visible_names();
        names.extend(self.interp.project.cache.iter().map(|(k, _)| k.clone()));
        names.sort();
        names
    }
}
