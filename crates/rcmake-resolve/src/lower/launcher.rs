//! Add the CTest instrumentation launcher to build commands.

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel, Target};

use super::{
    definition,
    shell::{command, direct, join_words},
};
use crate::usage::UsageResolver;
use crate::{Invocation, ResolveError};
use rcmake_base::shell::quote;

/// Return the launcher prefix when the project has an instrumentation query.
///
/// # Errors
///
/// This function does not return errors.
pub fn instrumentation_launcher(
    model: &ProjectModel,
) -> Result<Option<Vec<BString>>, ResolveError> {
    Ok(prefix(model))
}

fn prefix(model: &ProjectModel) -> Option<Vec<BString>> {
    if model.global.instrumentation.is_empty()
        && model
            .global
            .props
            .get(b"_RCMAKE_INSTRUMENTATION_QUERIES")
            .is_none()
    {
        return None;
    }
    let ctest = definition(model, DirId::ROOT, b"CMAKE_CTEST_COMMAND")
        .cloned()
        .unwrap_or_else(|| "ctest".into());
    Some(vec![ctest, "--instrument".into()])
}

pub(super) struct Command<'a> {
    pub role: &'static str,
    pub config: &'a ConfigId,
    pub output: &'a BString,
    pub source: Option<&'a BString>,
    pub language: &'a BString,
}

impl Command<'_> {
    pub(super) fn launch_tool(
        &self,
        model: &ProjectModel,
        target: &Target,
        suffix: &[u8],
        invocation: Invocation,
        usage: &mut UsageResolver<'_>,
    ) -> Result<Invocation, ResolveError> {
        let mut args = self.tool_prefix(model, target, suffix, usage)?;
        if args.is_empty() {
            return Ok(invocation);
        }
        let prefix = join_words(args.iter().map(|arg| quote(arg)));
        let shell = join_words([prefix, invocation.shell]);
        if invocation.argv.is_empty() {
            return Ok(command(shell));
        }
        args.extend(invocation.argv);
        Ok(Invocation {
            argv: args,
            shell,
            working_dir: invocation.working_dir,
            link_response: None,
        })
    }

    pub(super) fn tool_prefix(
        &self,
        model: &ProjectModel,
        target: &Target,
        suffix: &[u8],
        usage: &mut UsageResolver<'_>,
    ) -> Result<Vec<BString>, ResolveError> {
        let Some(id) = model.find_target(target.dir, &target.name) else {
            return Ok(Vec::new());
        };
        let handle = crate::usage::handle(id);
        let ctx = EvalContext {
            config: self.config.clone(),
            current: Some(handle),
            head: Some(handle),
            language: Some(self.language.clone()),
            link_language: self.source.is_none().then(|| self.language.clone()),
            ..EvalContext::default()
        };
        let kind = if self.source.is_some() {
            b"COMPILE".as_slice()
        } else {
            b"LINK"
        };
        let mut args = usage.tool_wrappers(id, kind, &ctx)?;
        let property = [self.language.as_slice(), suffix].concat();
        if let Some(value) = target.props.get(&property) {
            let value = usage.eval_ide_property(id, &property, value, &ctx)?;
            let launcher = list::split(&value, list::Empty::Keep);
            if launcher.first().is_some_and(|program| !program.is_empty()) {
                args.extend(launcher);
            }
        }
        Ok(args)
    }

    pub(super) fn wrap(
        self,
        model: &ProjectModel,
        target: &Target,
        invocation: Invocation,
    ) -> Invocation {
        let Some(mut args) = prefix(model) else {
            return invocation;
        };
        args.extend([
            "--command-type".into(),
            self.role.into(),
            "--target-name".into(),
            target.name.clone(),
            "--config".into(),
            self.config.0.clone(),
            "--build-dir".into(),
            model.dir(DirId::ROOT).binary_dir.clone(),
            "--output".into(),
            self.output.clone(),
        ]);
        if let Some(source) = self.source {
            args.extend(["--source".into(), source.clone()]);
        }
        args.extend(["--language".into(), self.language.clone(), "--".into()]);
        args.extend(invocation.argv);
        direct(args, invocation.working_dir)
    }
}
