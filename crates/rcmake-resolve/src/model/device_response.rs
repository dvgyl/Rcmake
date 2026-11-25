//! Response files for compiler-driver device linking.

use rcmake_base::BString;

use super::{DeviceLinkResponseFile, Invocation};

impl DeviceLinkResponseFile {
    /// Move the object and library arguments of a simple command to a response file.
    ///
    /// Return the revised invocation and the response arguments in command order.
    /// Return no result if a shell expression prevents a safe rewrite.
    #[must_use]
    pub fn rewrite(
        &self,
        invocation: &Invocation,
        path: &[u8],
    ) -> Option<(Invocation, Vec<BString>)> {
        if invocation.argv.len() < 2 || self.objects.is_empty() {
            return None;
        }
        let mut ranges = Vec::new();
        for words in [&self.objects, &self.libraries]
            .into_iter()
            .filter(|words| !words.is_empty())
        {
            let start = invocation
                .argv
                .windows(words.len())
                .position(|args| args == words)?;
            ranges.push(start..start + words.len());
        }
        let first = ranges.first()?;
        if first.start == 0 || ranges.get(1).is_some_and(|second| second.start < first.end) {
            return None;
        }
        let flag = self.flag.trim_ascii_end();
        let mut argv = Vec::with_capacity(invocation.argv.len());
        let mut arguments = Vec::new();
        for (index, argument) in invocation.argv.iter().enumerate() {
            if index == first.start {
                if flag.len() == self.flag.len() {
                    argv.push([flag, path].concat().into());
                } else {
                    argv.extend([BString::from(flag), BString::from(path)]);
                }
            }
            if ranges.iter().any(|range| range.contains(&index)) {
                arguments.push(argument.clone());
            } else {
                argv.push(argument.clone());
            }
        }
        Some((
            Invocation {
                argv,
                shell: BString::default(),
                working_dir: invocation.working_dir.clone(),
                link_response: None,
            },
            arguments,
        ))
    }
}
