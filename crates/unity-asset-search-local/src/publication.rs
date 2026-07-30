use std::ffi::OsStr;
use std::io::{self, Write as _};

use crate::EndpointNamespaceV1;

/// Private names for one crash-recoverable namespace artifact.
///
/// The caller must hold the artifact's authority before it invokes recovery. A daemon lease owns
/// endpoint and rendezvous recovery; the namespace binding lock owns binding recovery.
#[derive(Clone, Copy)]
pub(crate) struct PublicationSlots {
    current: &'static str,
    staging: &'static str,
    quarantine: Option<&'static str>,
}

impl PublicationSlots {
    pub(crate) const fn new(
        current: &'static str,
        staging: &'static str,
        quarantine: Option<&'static str>,
    ) -> Self {
        Self {
            current,
            staging,
            quarantine,
        }
    }
}

/// A complete staging file that has not yet reached its atomic-replace commit point.
#[must_use = "a prepared publication must be committed or left for authority-owned recovery"]
pub(crate) struct PreparedPublication {
    slots: PublicationSlots,
}

impl PreparedPublication {
    pub(crate) fn commit(self, namespace: &EndpointNamespaceV1) -> io::Result<PublicationCommit> {
        namespace.replace_file(
            OsStr::new(self.slots.staging),
            OsStr::new(self.slots.current),
        )?;
        Ok(PublicationCommit {
            durability_unconfirmed: namespace.sync().is_err(),
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicationCommit {
    durability_unconfirmed: bool,
}

impl PublicationCommit {
    pub(crate) const fn durability_unconfirmed(self) -> bool {
        self.durability_unconfirmed
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RecoveryOutcome {
    staging_removed: bool,
    quarantine_removed: bool,
}

impl RecoveryOutcome {
    pub(crate) const fn removed_any(self) -> bool {
        self.staging_removed || self.quarantine_removed
    }
}

/// A current artifact atomically moved out of discovery before conditional cleanup.
#[must_use = "a claimed publication must be restored or removed"]
pub(crate) struct QuarantinedPublication {
    slots: PublicationSlots,
}

impl QuarantinedPublication {
    pub(crate) fn name(&self) -> &OsStr {
        OsStr::new(
            self.slots
                .quarantine
                .expect("only quarantine-capable publication slots can be claimed"),
        )
    }

    pub(crate) fn restore(self, namespace: &EndpointNamespaceV1) -> io::Result<()> {
        match namespace.rename_file(self.name(), OsStr::new(self.slots.current), false) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                // The current leaf advanced while this old record was quarantined. Preserve the
                // newer current artifact and reclaim the now-unreachable old record.
                self.remove(namespace)
            }
            Err(source) => Err(source),
        }
    }

    pub(crate) fn remove(self, namespace: &EndpointNamespaceV1) -> io::Result<()> {
        match namespace.remove_file(self.name()) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(source),
        }
    }
}

pub(crate) fn prepare(
    namespace: &EndpointNamespaceV1,
    slots: PublicationSlots,
    encoded: &[u8],
) -> io::Result<PreparedPublication> {
    let mut file = namespace.create_file(OsStr::new(slots.staging))?;
    file.write_all(encoded)?;
    file.sync_all()?;
    Ok(PreparedPublication { slots })
}

/// Remove records left by a prior owner after it has stopped or crashed.
///
/// This never removes the current artifact. The caller's authority makes deterministic staging and
/// quarantine names safe to reclaim without racing a live publisher.
pub(crate) fn recover_abandoned(
    namespace: &EndpointNamespaceV1,
    slots: PublicationSlots,
) -> io::Result<RecoveryOutcome> {
    let staging_removed = remove_if_present(namespace, OsStr::new(slots.staging))?;
    let quarantine_removed = match slots.quarantine {
        Some(quarantine) => remove_if_present(namespace, OsStr::new(quarantine))?,
        None => false,
    };
    if staging_removed || quarantine_removed {
        namespace.sync()?;
    }
    Ok(RecoveryOutcome {
        staging_removed,
        quarantine_removed,
    })
}

pub(crate) fn claim_current(
    namespace: &EndpointNamespaceV1,
    slots: PublicationSlots,
) -> io::Result<Option<QuarantinedPublication>> {
    let quarantine = slots.quarantine.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication artifact does not support conditional quarantine",
        )
    })?;
    match namespace.rename_file(OsStr::new(slots.current), OsStr::new(quarantine), false) {
        Ok(()) => Ok(Some(QuarantinedPublication { slots })),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

fn remove_if_present(namespace: &EndpointNamespaceV1, name: &OsStr) -> io::Result<bool> {
    match namespace.remove_file(name) {
        Ok(()) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(source),
    }
}
