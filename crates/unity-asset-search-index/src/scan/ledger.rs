use crate::ScanTraversalLimits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanLimitResource {
    Entries,
    PathBytes,
    Depth,
    Directories,
    Files,
    Diagnostics,
}

impl std::fmt::Display for ScanLimitResource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Entries => "entries",
            Self::PathBytes => "path bytes",
            Self::Depth => "depth",
            Self::Directories => "directories",
            Self::Files => "files",
            Self::Diagnostics => "diagnostics",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScanLimitError {
    pub(super) resource: ScanLimitResource,
    pub(super) observed_at_least: u64,
    pub(super) limit: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ScanLedgerUsage {
    pub(super) entries: u64,
    pub(super) path_bytes: u64,
    pub(super) max_depth: u32,
    pub(super) directories: u64,
    pub(super) files: u64,
    pub(super) diagnostics: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScanLedger {
    limits: ScanTraversalLimits,
    usage: ScanLedgerUsage,
}

impl ScanLedger {
    pub(super) const fn new(limits: ScanTraversalLimits) -> Self {
        Self {
            limits,
            usage: ScanLedgerUsage {
                entries: 0,
                path_bytes: 0,
                max_depth: 0,
                directories: 0,
                files: 0,
                diagnostics: 0,
            },
        }
    }

    #[cfg(test)]
    pub(super) fn observe_entry(
        &mut self,
        path_bytes: u64,
        depth: u32,
    ) -> Result<(), ScanLimitError> {
        self.observe_entries(1, path_bytes, depth)
    }

    pub(super) fn check_additional_entries(&self, amount: u64) -> Result<(), ScanLimitError> {
        checked_charge(
            ScanLimitResource::Entries,
            self.usage.entries,
            amount,
            self.limits.max_entries,
        )
        .map(|_| ())
    }

    pub(super) fn check_additional_path_bytes(&self, amount: u64) -> Result<(), ScanLimitError> {
        checked_charge(
            ScanLimitResource::PathBytes,
            self.usage.path_bytes,
            amount,
            self.limits.max_path_bytes,
        )
        .map_err(|error| ScanLimitError {
            observed_at_least: error.limit.saturating_add(1),
            ..error
        })
        .map(|_| ())
    }

    pub(super) fn check_depth(&self, depth: u32) -> Result<(), ScanLimitError> {
        if depth > self.limits.max_depth {
            return Err(ScanLimitError {
                resource: ScanLimitResource::Depth,
                observed_at_least: u64::from(depth),
                limit: u64::from(self.limits.max_depth),
            });
        }
        Ok(())
    }

    pub(super) fn observe_entries(
        &mut self,
        entries: u64,
        path_bytes: u64,
        depth: u32,
    ) -> Result<(), ScanLimitError> {
        if entries == 0 {
            return Ok(());
        }
        let observed_entries = checked_charge(
            ScanLimitResource::Entries,
            self.usage.entries,
            entries,
            self.limits.max_entries,
        )?;
        let observed_path_bytes = checked_charge(
            ScanLimitResource::PathBytes,
            self.usage.path_bytes,
            path_bytes,
            self.limits.max_path_bytes,
        )?;
        self.check_depth(depth)?;
        self.usage.entries = observed_entries;
        self.usage.path_bytes = observed_path_bytes;
        self.usage.max_depth = self.usage.max_depth.max(depth);
        Ok(())
    }

    /// Validates a configured scan root depth without charging it as a discovered child entry.
    pub(super) fn observe_root_depth(&mut self, depth: u32) -> Result<(), ScanLimitError> {
        self.check_depth(depth)?;
        self.usage.max_depth = self.usage.max_depth.max(depth);
        Ok(())
    }

    /// Charges an entry kind only after a no-follow reopen confirms it.
    pub(super) fn observe_kind(&mut self, kind: ScanEntryKind) -> Result<(), ScanLimitError> {
        match kind {
            ScanEntryKind::Directory => {
                self.usage.directories = checked_charge(
                    ScanLimitResource::Directories,
                    self.usage.directories,
                    1,
                    self.limits.max_directories,
                )?;
            }
            ScanEntryKind::File => {
                self.usage.files = checked_charge(
                    ScanLimitResource::Files,
                    self.usage.files,
                    1,
                    self.limits.max_files,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn observe_diagnostic(&mut self) -> Result<(), ScanLimitError> {
        self.usage.diagnostics = checked_charge(
            ScanLimitResource::Diagnostics,
            self.usage.diagnostics,
            1,
            self.limits.max_diagnostics,
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) const fn usage(self) -> ScanLedgerUsage {
        self.usage
    }
}

fn checked_charge(
    resource: ScanLimitResource,
    current: u64,
    amount: u64,
    limit: u64,
) -> Result<u64, ScanLimitError> {
    let observed_at_least = current.saturating_add(amount);
    if observed_at_least > limit {
        return Err(ScanLimitError {
            resource,
            observed_at_least,
            limit,
        });
    }
    Ok(observed_at_least)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ScanTraversalLimits {
        ScanTraversalLimits {
            max_entries: 1,
            max_path_bytes: 3,
            max_depth: 2,
            max_directories: 1,
            max_files: 1,
            max_diagnostics: 1,
            max_policy_matches: 1,
        }
    }

    #[test]
    fn exact_limits_are_accepted() {
        let mut ledger = ScanLedger::new(limits());

        ledger.observe_entry(3, 2).unwrap();
        ledger.observe_kind(ScanEntryKind::File).unwrap();
        ledger.observe_diagnostic().unwrap();

        assert_eq!(
            ledger.usage(),
            ScanLedgerUsage {
                entries: 1,
                path_bytes: 3,
                max_depth: 2,
                directories: 0,
                files: 1,
                diagnostics: 1,
            }
        );
    }

    #[test]
    fn each_dimension_rejects_one_over() {
        let mut entries = ScanLedger::new(limits());
        entries.observe_entry(0, 0).unwrap();
        assert_eq!(
            entries.observe_entry(0, 0).unwrap_err().resource,
            ScanLimitResource::Entries
        );

        let mut path_bytes = ScanLedger::new(ScanTraversalLimits {
            max_entries: 2,
            max_path_bytes: 1,
            ..limits()
        });
        path_bytes.observe_entry(1, 0).unwrap();
        assert_eq!(
            path_bytes.observe_entry(1, 0).unwrap_err().resource,
            ScanLimitResource::PathBytes
        );

        let mut depth = ScanLedger::new(ScanTraversalLimits {
            max_depth: 0,
            ..limits()
        });
        assert_eq!(
            depth.observe_entry(0, 1).unwrap_err().resource,
            ScanLimitResource::Depth
        );

        let mut directories = ScanLedger::new(ScanTraversalLimits {
            max_entries: 2,
            ..limits()
        });
        directories.observe_kind(ScanEntryKind::Directory).unwrap();
        assert_eq!(
            directories
                .observe_kind(ScanEntryKind::Directory)
                .unwrap_err()
                .resource,
            ScanLimitResource::Directories
        );

        let mut files = ScanLedger::new(ScanTraversalLimits {
            max_entries: 2,
            ..limits()
        });
        files.observe_kind(ScanEntryKind::File).unwrap();
        assert_eq!(
            files
                .observe_kind(ScanEntryKind::File)
                .unwrap_err()
                .resource,
            ScanLimitResource::Files
        );

        let mut ledger = ScanLedger::new(limits());
        ledger.observe_diagnostic().unwrap();
        let error = ledger.observe_diagnostic().unwrap_err();
        assert_eq!(error.resource, ScanLimitResource::Diagnostics);
    }

    #[test]
    fn failed_entry_batch_does_not_partially_commit_usage() {
        let mut ledger = ScanLedger::new(ScanTraversalLimits {
            max_entries: 2,
            max_path_bytes: 1,
            ..limits()
        });

        let error = ledger.observe_entries(2, 2, 0).unwrap_err();

        assert_eq!(error.resource, ScanLimitResource::PathBytes);
        assert_eq!(ledger.usage(), ScanLedgerUsage::default());
    }
}
