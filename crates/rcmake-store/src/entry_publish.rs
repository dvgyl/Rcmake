// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

impl Store<'_> {
    /// Publish an entry. Keep the first valid entry for a key.
    ///
    /// All referenced blobs must already be present. Wait up to 250 milliseconds
    /// for the cache lock. Return the publication outcome if the store skips a write.
    pub fn put_entry<T: ToJson>(
        &self,
        domain: Domain,
        key: Digest,
        entry: &Entry<T>,
    ) -> HostResult<PublishOutcome> {
        self.publish_entry::<T, Value>(domain, key, entry, |_, _| true, || true)
    }

    /// Publish an entry after checking both the previous and new evidence.
    ///
    /// The previous-entry check receives a read-only store. Both checks run
    /// under the publication lock. A failed new-entry check preserves the
    /// previous entry and returns [PublishOutcome::Stale].
    pub fn put_entry_checked<T: ToJson + FromJson>(
        &self,
        domain: Domain,
        key: Digest,
        entry: &Entry<T>,
        previous_valid: impl FnOnce(&Entry<T>, &Store<'_>) -> bool,
        fresh_valid: impl FnOnce() -> bool,
    ) -> HostResult<PublishOutcome> {
        self.publish_entry(domain, key, entry, previous_valid, fresh_valid)
    }

    fn publish_entry<T: ToJson, P: FromJson>(
        &self,
        domain: Domain,
        key: Digest,
        entry: &Entry<T>,
        previous_valid: impl FnOnce(&Entry<P>, &Store<'_>) -> bool,
        fresh_valid: impl FnOnce() -> bool,
    ) -> HostResult<PublishOutcome> {
        if !self.writable() {
            return Ok(PublishOutcome::Disabled);
        }
        let path = self.entry_path(domain, key);
        let frame = entry_frame(domain, key, entry, &path)?;
        self.with_publication(|locked| {
            for &digest in &entry.blobs {
                let path = self.blob_path(digest);
                if self.file_metadata(&path)?.is_none() {
                    return Err(io_at(
                        io::Error::new(ErrorKind::NotFound, "A referenced cache blob is missing."),
                        &path,
                    ));
                }
            }
            let previous = self.read_entry::<Entry<P>>(domain, key, &path, false)?;
            let mut config = self.config.clone();
            config.mode = StoreMode::ReadOnly;
            let mut reader = match self.session {
                Some(session) => Self::with_session(self.host, config, session),
                None => Self::new(self.host, config)?,
            };
            reader.memo = self.memo;
            if let CacheRead::Found(previous) = &previous
                && reader.entry_matches(previous, blob_limit(), false)?
                && previous_valid(previous, &reader)
            {
                return Ok(PublishOutcome::AlreadyPresent);
            }
            if self.verify_inputs(&entry.trace) != VerifyOutcome::Hit || !fresh_valid() {
                return Ok(PublishOutcome::Stale);
            }
            locked.publish(&path, &frame, !matches!(previous, CacheRead::Missing))?;
            Ok(PublishOutcome::Published)
        })
    }
}
