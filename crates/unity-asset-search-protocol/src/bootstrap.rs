use serde::{Deserialize, Deserializer, Serialize};

use crate::validation::{ContractValidationError, ValidateContract, ensure_version};
use crate::{DaemonInstanceId, ProjectId};

pub const BOOTSTRAP_VERSION: u16 = 1;
pub const MAX_BOOTSTRAP_REVISIONS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapHelloV1 {
    bootstrap_version: u16,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    supported_revisions: Vec<u16>,
}

impl BootstrapHelloV1 {
    pub fn new(
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        supported_revisions: Vec<u16>,
    ) -> Result<Self, ContractValidationError> {
        let hello = Self {
            bootstrap_version: BOOTSTRAP_VERSION,
            project_id,
            daemon_instance_id,
            supported_revisions,
        };
        hello.validate()?;
        Ok(hello)
    }

    #[must_use]
    pub const fn bootstrap_version(&self) -> u16 {
        self.bootstrap_version
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    #[must_use]
    pub fn supported_revisions(&self) -> &[u16] {
        &self.supported_revisions
    }
}

impl ValidateContract for BootstrapHelloV1 {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_version("bootstrap", self.bootstrap_version, BOOTSTRAP_VERSION)?;
        if self.supported_revisions.is_empty() {
            return Err(ContractValidationError::Empty {
                field: "supported_revisions",
            });
        }
        if self.supported_revisions.len() > MAX_BOOTSTRAP_REVISIONS {
            return Err(ContractValidationError::EntryLimit {
                field: "supported_revisions",
                actual: self.supported_revisions.len(),
                maximum: MAX_BOOTSTRAP_REVISIONS,
            });
        }
        if self.supported_revisions.contains(&0)
            || self
                .supported_revisions
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(ContractValidationError::NotStrictlyIncreasing {
                field: "supported_revisions",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for BootstrapHelloV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            bootstrap_version: u16,
            project_id: ProjectId,
            daemon_instance_id: DaemonInstanceId,
            supported_revisions: Vec<u16>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let hello = Self {
            bootstrap_version: wire.bootstrap_version,
            project_id: wire.project_id,
            daemon_instance_id: wire.daemon_instance_id,
            supported_revisions: wire.supported_revisions,
        };
        hello.validate().map_err(serde::de::Error::custom)?;
        Ok(hello)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapErrorCode {
    ProjectMismatch,
    InstanceMismatch,
    NoCommonRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapReplyV1 {
    Accepted {
        bootstrap_version: u16,
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        selected_revision: u16,
    },
    Rejected {
        bootstrap_version: u16,
        code: BootstrapErrorCode,
    },
}

impl BootstrapReplyV1 {
    #[must_use]
    pub fn negotiate(
        hello: &BootstrapHelloV1,
        expected_project: ProjectId,
        expected_instance: DaemonInstanceId,
        supported_revisions: &[u16],
    ) -> Self {
        if hello.project_id != expected_project {
            return Self::rejected(BootstrapErrorCode::ProjectMismatch);
        }
        if hello.daemon_instance_id != expected_instance {
            return Self::rejected(BootstrapErrorCode::InstanceMismatch);
        }
        let selected = hello
            .supported_revisions
            .iter()
            .rev()
            .copied()
            .find(|candidate| supported_revisions.contains(candidate));
        match selected {
            Some(selected_revision) => Self::Accepted {
                bootstrap_version: BOOTSTRAP_VERSION,
                project_id: expected_project,
                daemon_instance_id: expected_instance,
                selected_revision,
            },
            None => Self::rejected(BootstrapErrorCode::NoCommonRevision),
        }
    }

    #[must_use]
    pub const fn selected_revision(&self) -> Option<u16> {
        match self {
            Self::Accepted {
                selected_revision, ..
            } => Some(*selected_revision),
            Self::Rejected { .. } => None,
        }
    }

    pub fn validate_for(&self, hello: &BootstrapHelloV1) -> Result<(), ContractValidationError> {
        self.validate()?;
        if let Self::Accepted {
            project_id,
            daemon_instance_id,
            selected_revision,
            ..
        } = self
            && (*project_id != hello.project_id
                || *daemon_instance_id != hello.daemon_instance_id
                || hello
                    .supported_revisions
                    .binary_search(selected_revision)
                    .is_err())
        {
            return Err(ContractValidationError::Inconsistent {
                field: "bootstrap reply binding",
            });
        }
        Ok(())
    }

    const fn rejected(code: BootstrapErrorCode) -> Self {
        Self::Rejected {
            bootstrap_version: BOOTSTRAP_VERSION,
            code,
        }
    }
}

impl ValidateContract for BootstrapReplyV1 {
    fn validate(&self) -> Result<(), ContractValidationError> {
        let (bootstrap_version, selected_revision) = match self {
            Self::Accepted {
                bootstrap_version,
                selected_revision,
                ..
            } => (*bootstrap_version, Some(*selected_revision)),
            Self::Rejected {
                bootstrap_version, ..
            } => (*bootstrap_version, None),
        };
        ensure_version("bootstrap", bootstrap_version, BOOTSTRAP_VERSION)?;
        if selected_revision == Some(0) {
            return Err(ContractValidationError::Empty {
                field: "selected_revision",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for BootstrapReplyV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Accepted {
                bootstrap_version: u16,
                project_id: ProjectId,
                daemon_instance_id: DaemonInstanceId,
                selected_revision: u16,
            },
            Rejected {
                bootstrap_version: u16,
                code: BootstrapErrorCode,
            },
        }

        let reply = match Wire::deserialize(deserializer)? {
            Wire::Accepted {
                bootstrap_version,
                project_id,
                daemon_instance_id,
                selected_revision,
            } => Self::Accepted {
                bootstrap_version,
                project_id,
                daemon_instance_id,
                selected_revision,
            },
            Wire::Rejected {
                bootstrap_version,
                code,
            } => Self::Rejected {
                bootstrap_version,
                code,
            },
        };
        reply.validate().map_err(serde::de::Error::custom)?;
        Ok(reply)
    }
}
