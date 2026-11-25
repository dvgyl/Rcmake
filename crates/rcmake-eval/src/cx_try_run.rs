use std::path::Path;

use rcmake_host::{Follow, WriteMode};

use crate::{CmdError, Cx};

impl Cx<'_> {
    /// Write a cross-compiled probe result block.
    /// Replace the old file with the header and block on the first write.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exceeds the size limit or the host cannot write it.
    pub fn write_try_run_result(
        &mut self,
        path: &Path,
        header: &[u8],
        block: &[u8],
    ) -> Result<(), CmdError> {
        let started = self.interp.try_run_results_started;
        let previous = if started {
            self.host()
                .metadata(path, Follow::Yes)
                .map_err(|error| CmdError::status(error.to_string()))?
                .len
        } else {
            header.len() as u64
        };
        if previous.saturating_add(block.len() as u64) > self.limits().max_file_read {
            return Err(CmdError::status(
                "TryRunResults.cmake exceeds the size limit.",
            ));
        }
        let result = if started {
            self.host().write(path, block, WriteMode::Append)
        } else {
            self.host()
                .write(path, &[header, block].concat(), WriteMode::Truncate)
        };
        result.map_err(|error| CmdError::status(error.to_string()))?;
        self.interp.try_run_results_started = true;
        Ok(())
    }
}
