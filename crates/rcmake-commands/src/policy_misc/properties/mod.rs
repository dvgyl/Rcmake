use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_model::PropScope;

use super::{quoted, values};

pub(super) use define::define;
pub(super) use get::{get, get_cmake, get_directory, get_source};
pub(super) use print::print_value;
pub(super) use rule::set;
pub(super) use set::{set_directory, set_sources};

mod computed;
mod define;
mod file_set;
mod generated;
mod get;
mod known_features;
mod print;
mod rule;
mod scope;
mod set;

use known_features::known_features;

fn is_inherited(cx: &mut Cx<'_>, scope: &[u8], name: &[u8]) -> bool {
    cx.global()
        .defined_props
        .iter()
        .any(|entry| entry.scope == scope && entry.name == name && entry.inherited)
}

fn model_value(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    if inherited {
        cx.project().get_prop_inherited(scope, name)
    } else {
        cx.project().get_prop(scope, name)
    }
}
