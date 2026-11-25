use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, RenderOptions, render};
use rcmake_model::{DirId, ProjectModel, Target};

use crate::ResolveError;

pub(super) fn validate(
    model: &ProjectModel,
    target: &Target,
    key: &BString,
    value: &BString,
) -> Result<(), ResolveError> {
    if value.is_empty() || value.starts_with(b"$<") || value.starts_with(b"${_IMPORT_PREFIX}/") {
        return Ok(());
    }
    let dir = model.dir(target.dir);
    let inside = |parent: &[u8], child: &[u8]| {
        !parent.is_empty()
            && Lexical::new(&Lexical::new(parent).normalize())
                .prefix(Lexical::new(&Lexical::new(child).normalize()))
    };
    let body = if !Lexical::new(value).absolute() {
        format!("contains relative path:\n\n  \"{value}\"")
    } else {
        let binary = inside(&dir.binary_dir, value);
        let source = inside(&dir.source_dir, value);
        let install = model.definition(target.dir, b"CMAKE_INSTALL_PREFIX");
        if install.is_some_and(|install| {
            inside(install, value)
                && (!binary || inside(&dir.binary_dir, install))
                && (!source || inside(&dir.source_dir, install))
        }) {
            return Ok(());
        }
        let kind = if binary {
            "build"
        } else if source {
            "source"
        } else if rcmake_genex::parse(value).is_ok_and(|ast| !ast.is_plain()) {
            return Err(ResolveError::Message(
                "Install interface contains a mixed path.".into(),
            ));
        } else {
            return Ok(());
        };
        format!("contains path:\n\n  \"{value}\"\n\nwhich is prefixed in the {kind} directory.")
    };
    let diagnostic = Diagnostic {
        annotations: None,
        kind: MessageKind::Error,
        effect: Effect::None,
        category: None,
        policy: None,
        location: Location::FileLine {
            file: Lexical::new(&dir.source_dir)
                .append(b"CMakeLists.txt")
                .as_slice()
                .into(),
            line: 0,
        },
        body: format!("Target \"{}\" {key} property {body}", target.name).into(),
    };
    let text = render(
        &diagnostic,
        &RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..Default::default()
        },
    );
    Err(ResolveError::Message(text))
}
