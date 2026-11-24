use std::collections::HashSet;

use rcmake_base::{BString, list};
use rcmake_diag::{Location, MessageKind};
use rcmake_genex::EvalContext;
use rcmake_model::{BT, TargetId};

use super::UsageResolver;
use crate::ResolveError;

impl UsageResolver<'_> {
    pub(crate) fn tool_wrappers(
        &mut self,
        id: TargetId,
        prefix: &[u8],
        ctx: &EvalContext,
    ) -> Result<Vec<BString>, ResolveError> {
        let property = [prefix, b"_WRAPPERS"].concat();
        let ctx = self.property_context(id, &property, ctx)?;
        let target = self.target(id)?;
        let usage = self.walk(id, &property, &ctx)?;
        let mut names = HashSet::new();
        let mut entries = usage
            .contributions
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .filter(|entry| names.insert(&entry.value))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.value.cmp(&right.value));
        let mut args = Vec::new();
        let mut failure = None;
        for entry in entries {
            let name = &entry.value;
            let definition = [b"CMAKE_", prefix, b"_WRAPPER_", name.as_slice()].concat();
            if let Some(raw) = self.model.global.props.get(&definition) {
                let raw = BT::new(raw.clone(), entry.backtrace.clone());
                let (value, _) = self.eval_entry(id, target.dir, &property, &raw, &ctx, false)?;
                if !value.is_empty() {
                    args.extend(list::split(&value, list::Empty::Keep));
                }
            } else {
                let kind = BString::from(prefix.to_ascii_lowercase());
                let definition = BString::from(definition);
                let body = BString::from(format!(
                    "Specified {kind} wrapper '{name}' for target '{}' does not exist.\nSet the GLOBAL property '{definition}'.",
                    target.name
                ));
                let error = if let Some(paths) = self.source_paths() {
                    paths.emit(
                        self.model,
                        MessageKind::Error,
                        Location::Backtrace(entry.backtrace.clone()),
                        body.clone(),
                    );
                    ResolveError::Reported(body)
                } else {
                    ResolveError::Message(body)
                };
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(args), Err)
    }
}
