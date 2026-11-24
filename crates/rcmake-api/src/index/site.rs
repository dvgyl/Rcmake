use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_model::DirId;

use super::{CallableDefinition, SourceLocation, TargetDefinition, VariableDefinition};

const MAX_EXECUTED_SITES: usize = 16_384;
const MAX_SITE_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Half-open byte range in an evaluated listfile or virtual source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceRange {
    /// Offset of the first byte.
    pub start: u32,
    /// Offset after the last byte.
    pub end: u32,
}

/// Symbol binding at the time an evaluated reference ran.
///
/// An absent definition means the symbol was undefined at this site.
#[derive(Clone, Debug)]
pub enum ExecutedSymbol {
    /// Target name and visible definition.
    Target {
        /// Name used by the command.
        name: BString,
        /// Target or alias at execution time.
        definition: Option<TargetDefinition>,
    },
    /// Variable name and effective value.
    Variable {
        /// Name read by the command.
        name: BString,
        /// Normal or cache binding at execution time.
        definition: Option<VariableDefinition>,
    },
    /// Callable name and visible definition.
    Callable {
        /// Name used by the invocation.
        name: BString,
        /// Function or macro registered at execution time.
        definition: Option<CallableDefinition>,
    },
}

/// Binding observed during one execution of a command or argument.
#[derive(Clone, Debug)]
pub struct ExecutedSite {
    /// Directory in which evaluation ran.
    pub dir: DirId,
    /// Location of the symbol reference.
    pub site: SourceLocation,
    /// Binding observed when the evaluator reached this site.
    pub symbol: ExecutedSymbol,
}

/// Bounded set of symbol references collected in evaluation order.
#[derive(Clone, Debug, Default)]
pub struct ExecutedSites {
    by_file: BTreeMap<BString, Vec<ExecutedSite>>,
    len: usize,
    text_bytes: usize,
    truncated: bool,
}

impl ExecutedSites {
    /// Record a symbol reference.
    ///
    /// Retain at most 16,384 references and 8 MiB of text per snapshot.
    /// Return false if the reference did not fit.
    pub fn record(&mut self, record: ExecutedSite) -> bool {
        let text_bytes = self.text_bytes.saturating_add(record.text_bytes());
        if self.len == MAX_EXECUTED_SITES || text_bytes > MAX_SITE_TEXT_BYTES {
            self.truncated = true;
            return false;
        }
        self.by_file
            .entry(record.site.path.clone())
            .or_default()
            .push(record);
        self.len += 1;
        self.text_bytes = text_bytes;
        true
    }

    /// Return whether the limit caused records to be discarded.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub(super) fn matching(
        &self,
        dir: Option<DirId>,
        site: &SourceLocation,
    ) -> impl Iterator<Item = &ExecutedSite> {
        let query = site.clone();
        self.by_file
            .get(&query.path)
            .into_iter()
            .flatten()
            .filter(move |record| {
                dir.is_none_or(|dir| dir == record.dir) && record.site.matches(&query)
            })
    }
}

impl ExecutedSite {
    fn text_bytes(&self) -> usize {
        let location_bytes = |site: &SourceLocation| site.path.len();
        let (name, details) = match &self.symbol {
            ExecutedSymbol::Target { name, definition } => (
                name,
                definition.as_ref().map_or(0, |definition| {
                    definition
                        .declared_links
                        .iter()
                        .chain(&definition.usage.changed_properties)
                        .fold(definition.name.len(), |size, link| {
                            size.saturating_add(link.len())
                        })
                        .saturating_add(definition.alias_of.as_ref().map_or(0, |alias| alias.len()))
                        .saturating_add(definition.location.as_ref().map_or(0, location_bytes))
                }),
            ),
            ExecutedSymbol::Variable { name, definition } => (
                name,
                definition.as_ref().map_or(0, |definition| {
                    definition
                        .value
                        .len()
                        .saturating_add(definition.location.as_ref().map_or(0, location_bytes))
                }),
            ),
            ExecutedSymbol::Callable { name, definition } => (
                name,
                definition.as_ref().map_or(0, |definition| {
                    definition
                        .parameters
                        .iter()
                        .fold(definition.name.len(), |size, parameter| {
                            size.saturating_add(parameter.len())
                        })
                        .saturating_add(
                            definition
                                .documentation
                                .as_ref()
                                .map_or(0, |text| text.len()),
                        )
                        .saturating_add(location_bytes(&definition.location))
                }),
            ),
        };
        self.site
            .path
            .len()
            .saturating_add(name.len())
            .saturating_add(details)
    }
}
