use rcmake_base::BString;
use rcmake_diag::PolicyId;

pub(super) fn body(id: PolicyId) -> BString {
    id.warning_text().into()
}
