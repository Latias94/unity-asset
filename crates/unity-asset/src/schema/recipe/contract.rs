use serde::Serialize;
use thiserror::Error;
use unity_asset_binary::{typetree::TypeTreeSemanticDigestError, unity_version::UnityVersion};
use unity_asset_core::{
    BudgetError, DigestBuildError, DigestV1, FieldPath, FieldPathError, ObjectAddress, ObjectKind,
    SemanticDigestError, ValuePathError,
};

use crate::workspace::{MutationPlanError, MutationPlanFragment, WorkspaceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeId {
    ReferenceRetargetV1,
    TransformV1,
    MaterialTextureEnvironmentV1,
    UnityEventPersistentCallsV1,
    HierarchyReparentV1,
    AudioClipStreamedResourceV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeMutationKind {
    FieldReplace,
    ReferenceReplace,
    ResourceReplace,
    SequenceEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipePrecondition {
    TrustedSchemaProvenance,
    MatchingClass,
    ExistingField,
    MatchingFieldShape,
    ResolvedLogicalReferences,
    ConsistentHierarchy,
    DeclaredPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeClassConstraint {
    AnySchemaMatchedObject,
    TransformOrRectTransform,
    Material,
    AudioClip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeObjectFormat {
    Binary,
    Yaml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RecipeCapability {
    id: RecipeId,
    class_constraint: RecipeClassConstraint,
    formats: &'static [RecipeObjectFormat],
    preconditions: &'static [RecipePrecondition],
    outputs: &'static [RecipeMutationKind],
}

impl RecipeCapability {
    #[must_use]
    pub const fn id(self) -> RecipeId {
        self.id
    }

    #[must_use]
    pub const fn class_constraint(self) -> RecipeClassConstraint {
        self.class_constraint
    }

    #[must_use]
    pub const fn formats(self) -> &'static [RecipeObjectFormat] {
        self.formats
    }

    #[must_use]
    pub const fn preconditions(self) -> &'static [RecipePrecondition] {
        self.preconditions
    }

    #[must_use]
    pub const fn outputs(self) -> &'static [RecipeMutationKind] {
        self.outputs
    }
}

const BOTH_FORMATS: &[RecipeObjectFormat] = &[RecipeObjectFormat::Binary, RecipeObjectFormat::Yaml];
const SCHEMA_FIELD_REFERENCE: &[RecipePrecondition] = &[
    RecipePrecondition::TrustedSchemaProvenance,
    RecipePrecondition::ExistingField,
    RecipePrecondition::MatchingFieldShape,
    RecipePrecondition::ResolvedLogicalReferences,
];
const SCHEMA_CLASS_FIELD: &[RecipePrecondition] = &[
    RecipePrecondition::TrustedSchemaProvenance,
    RecipePrecondition::MatchingClass,
    RecipePrecondition::ExistingField,
    RecipePrecondition::MatchingFieldShape,
];
const REFERENCE_OUTPUT: &[RecipeMutationKind] = &[RecipeMutationKind::ReferenceReplace];
const FIELD_REFERENCE_OUTPUT: &[RecipeMutationKind] = &[
    RecipeMutationKind::FieldReplace,
    RecipeMutationKind::ReferenceReplace,
];
const RESOURCE_OUTPUT: &[RecipeMutationKind] = &[RecipeMutationKind::ResourceReplace];

pub(super) const CAPABILITIES: &[RecipeCapability] = &[
    RecipeCapability {
        id: RecipeId::ReferenceRetargetV1,
        class_constraint: RecipeClassConstraint::AnySchemaMatchedObject,
        formats: BOTH_FORMATS,
        preconditions: SCHEMA_FIELD_REFERENCE,
        outputs: REFERENCE_OUTPUT,
    },
    RecipeCapability {
        id: RecipeId::TransformV1,
        class_constraint: RecipeClassConstraint::TransformOrRectTransform,
        formats: BOTH_FORMATS,
        preconditions: SCHEMA_CLASS_FIELD,
        outputs: &[RecipeMutationKind::FieldReplace],
    },
    RecipeCapability {
        id: RecipeId::MaterialTextureEnvironmentV1,
        class_constraint: RecipeClassConstraint::Material,
        formats: BOTH_FORMATS,
        preconditions: SCHEMA_FIELD_REFERENCE,
        outputs: FIELD_REFERENCE_OUTPUT,
    },
    RecipeCapability {
        id: RecipeId::UnityEventPersistentCallsV1,
        class_constraint: RecipeClassConstraint::AnySchemaMatchedObject,
        formats: BOTH_FORMATS,
        preconditions: SCHEMA_FIELD_REFERENCE,
        outputs: &[RecipeMutationKind::SequenceEdit],
    },
    RecipeCapability {
        id: RecipeId::HierarchyReparentV1,
        class_constraint: RecipeClassConstraint::TransformOrRectTransform,
        formats: BOTH_FORMATS,
        preconditions: &[
            RecipePrecondition::TrustedSchemaProvenance,
            RecipePrecondition::MatchingClass,
            RecipePrecondition::MatchingFieldShape,
            RecipePrecondition::ResolvedLogicalReferences,
            RecipePrecondition::ConsistentHierarchy,
        ],
        outputs: &[
            RecipeMutationKind::ReferenceReplace,
            RecipeMutationKind::SequenceEdit,
        ],
    },
    RecipeCapability {
        id: RecipeId::AudioClipStreamedResourceV1,
        class_constraint: RecipeClassConstraint::AudioClip,
        formats: BOTH_FORMATS,
        preconditions: &[
            RecipePrecondition::TrustedSchemaProvenance,
            RecipePrecondition::MatchingClass,
            RecipePrecondition::ExistingField,
            RecipePrecondition::MatchingFieldShape,
            RecipePrecondition::DeclaredPayload,
        ],
        outputs: RESOURCE_OUTPUT,
    },
];

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RecipeCapabilityCatalog {
    version: u8,
    recipes: &'static [RecipeCapability],
}

impl RecipeCapabilityCatalog {
    #[must_use]
    pub const fn version(self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn recipes(self) -> &'static [RecipeCapability] {
        self.recipes
    }
}

#[must_use]
pub const fn recipe_capabilities() -> RecipeCapabilityCatalog {
    RecipeCapabilityCatalog {
        version: 1,
        recipes: CAPABILITIES,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaOrigin {
    EmbeddedTypeTree,
    FrozenRegistry,
    YamlShape,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeclaredUnityVersion {
    Parsed { version: UnityVersion },
    Absent,
    Unparseable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BinarySchemaVersion {
    unity: DeclaredUnityVersion,
    serialized_file_format: u32,
}

impl BinarySchemaVersion {
    #[must_use]
    pub const fn new(unity: DeclaredUnityVersion, serialized_file_format: u32) -> Self {
        Self {
            unity,
            serialized_file_format,
        }
    }

    #[must_use]
    pub const fn declared_unity(&self) -> &DeclaredUnityVersion {
        &self.unity
    }

    #[must_use]
    pub const fn unity(&self) -> Option<&UnityVersion> {
        match &self.unity {
            DeclaredUnityVersion::Parsed { version } => Some(version),
            DeclaredUnityVersion::Absent | DeclaredUnityVersion::Unparseable => None,
        }
    }

    #[must_use]
    pub const fn serialized_file_format(&self) -> u32 {
        self.serialized_file_format
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaProvenance {
    object_kind: ObjectKind,
    class_id: i32,
    origin: SchemaOrigin,
    schema_digest: Option<DigestV1>,
    binary_version: Option<BinarySchemaVersion>,
    script_id: Option<[u8; 16]>,
}

impl SchemaProvenance {
    pub(crate) fn binary(
        class_id: i32,
        origin: SchemaOrigin,
        schema_digest: Option<DigestV1>,
        binary_version: BinarySchemaVersion,
        script_id: Option<[u8; 16]>,
    ) -> Self {
        Self {
            object_kind: ObjectKind::Binary,
            class_id,
            origin,
            schema_digest,
            binary_version: Some(binary_version),
            script_id,
        }
    }

    pub(crate) fn yaml(class_id: i32, schema_digest: DigestV1) -> Self {
        Self {
            object_kind: ObjectKind::Yaml,
            class_id,
            origin: SchemaOrigin::YamlShape,
            schema_digest: Some(schema_digest),
            binary_version: None,
            script_id: None,
        }
    }

    #[must_use]
    pub const fn object_kind(&self) -> ObjectKind {
        self.object_kind
    }

    #[must_use]
    pub const fn class_id(&self) -> i32 {
        self.class_id
    }

    #[must_use]
    pub const fn origin(&self) -> SchemaOrigin {
        self.origin
    }

    #[must_use]
    pub const fn schema_digest(&self) -> Option<DigestV1> {
        self.schema_digest
    }

    #[must_use]
    pub const fn binary_version(&self) -> Option<&BinarySchemaVersion> {
        self.binary_version.as_ref()
    }

    #[must_use]
    pub const fn script_id(&self) -> Option<[u8; 16]> {
        self.script_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaVariantId {
    GenericReference,
    Transform,
    RectTransformLegacyPosition,
    RectTransformAnchoredPosition,
    MaterialFastPropertyName,
    MaterialStringPropertyName,
    MaterialYamlPropertyName,
    UnityEventPersistentCalls,
    HierarchyLocalReferences,
    AudioClipResource,
    AudioClipStreamDataCompatibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeApplicabilityStatus {
    Applicable,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipeApplicability {
    recipe: RecipeId,
    status: RecipeApplicabilityStatus,
    variant: Option<SchemaVariantId>,
    rejection: Option<RecipeRejectionCode>,
}

impl RecipeApplicability {
    pub(crate) const fn applicable(recipe: RecipeId, variant: SchemaVariantId) -> Self {
        Self {
            recipe,
            status: RecipeApplicabilityStatus::Applicable,
            variant: Some(variant),
            rejection: None,
        }
    }

    pub(crate) const fn rejected(recipe: RecipeId, rejection: RecipeRejectionCode) -> Self {
        Self {
            recipe,
            status: RecipeApplicabilityStatus::Rejected,
            variant: None,
            rejection: Some(rejection),
        }
    }

    #[must_use]
    pub const fn recipe(&self) -> RecipeId {
        self.recipe
    }

    #[must_use]
    pub const fn status(&self) -> RecipeApplicabilityStatus {
        self.status
    }

    #[must_use]
    pub const fn variant(&self) -> Option<SchemaVariantId> {
        self.variant
    }

    #[must_use]
    pub const fn rejection(&self) -> Option<RecipeRejectionCode> {
        self.rejection
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipeLoweringReport {
    recipe: RecipeId,
    variant: SchemaVariantId,
    operation_count: usize,
    payload_count: usize,
}

impl RecipeLoweringReport {
    #[must_use]
    pub const fn recipe(&self) -> RecipeId {
        self.recipe
    }

    #[must_use]
    pub const fn variant(&self) -> SchemaVariantId {
        self.variant
    }

    #[must_use]
    pub const fn operation_count(&self) -> usize {
        self.operation_count
    }

    #[must_use]
    pub const fn payload_count(&self) -> usize {
        self.payload_count
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeLowering {
    Changed {
        fragment: MutationPlanFragment,
        report: RecipeLoweringReport,
    },
    Unchanged {
        report: RecipeLoweringReport,
    },
}

impl RecipeLowering {
    pub(crate) fn changed(
        recipe: RecipeId,
        variant: SchemaVariantId,
        fragment: MutationPlanFragment,
    ) -> Result<Self, RecipeError> {
        let report = RecipeLoweringReport {
            recipe,
            variant,
            operation_count: fragment.actions().len(),
            payload_count: fragment.payloads().len(),
        };
        Ok(Self::Changed { fragment, report })
    }

    pub(crate) const fn unchanged(recipe: RecipeId, variant: SchemaVariantId) -> Self {
        Self::Unchanged {
            report: RecipeLoweringReport {
                recipe,
                variant,
                operation_count: 0,
                payload_count: 0,
            },
        }
    }

    #[must_use]
    pub const fn report(&self) -> &RecipeLoweringReport {
        match self {
            Self::Changed { report, .. } | Self::Unchanged { report } => report,
        }
    }

    #[must_use]
    pub const fn fragment(&self) -> Option<&MutationPlanFragment> {
        match self {
            Self::Changed { fragment, .. } => Some(fragment),
            Self::Unchanged { .. } => None,
        }
    }

    #[must_use]
    pub fn into_fragment(self) -> Option<MutationPlanFragment> {
        match self {
            Self::Changed { fragment, .. } => Some(fragment),
            Self::Unchanged { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeRejectionCode {
    TargetUnloaded,
    TargetMissing,
    TargetAmbiguous,
    TargetInvalid,
    MissingSchemaProvenance,
    WrongClass,
    UnsupportedVersion,
    UnsupportedSchema,
    MissingField,
    InvalidFieldPath,
    ProtectedSemanticField,
    WrongFieldShape,
    AmbiguousFieldVariant,
    PropertyNotFound,
    DuplicateProperty,
    InvalidReference,
    UnresolvedReference,
    CallIndexOutOfBounds,
    ListenerArgumentMismatch,
    CrossSourceHierarchy,
    SelfParent,
    HierarchyCycle,
    MissingParent,
    MissingChild,
    ParentChildMismatch,
    DuplicateHierarchyNode,
    DuplicateChildMembership,
    MultipleParents,
    ChildPlacementOutOfBounds,
    InvalidPayload,
    NonFiniteValue,
}

#[derive(Debug, Error)]
pub enum RecipeError {
    #[error("recipe target is not loaded")]
    TargetUnloaded,
    #[error("recipe target is missing")]
    TargetMissing,
    #[error("recipe target is ambiguous across {candidates} candidates")]
    TargetAmbiguous { candidates: usize },
    #[error("recipe target is invalid: diagnostic code {code}")]
    TargetInvalid { code: String },
    #[error("workspace recipe inspection returned inconsistent identity metadata")]
    InspectionContractMismatch,
    #[error("the object has no trusted schema provenance")]
    MissingSchemaProvenance,
    #[error(
        "recipe requires class {expected_id} ({expected_name}), found {actual_id} ({actual_name})"
    )]
    WrongClass {
        expected_id: i32,
        expected_name: &'static str,
        actual_id: i32,
        actual_name: String,
    },
    #[error("the object's declared Unity version is unavailable or unparseable")]
    UnsupportedVersion,
    #[error("the observed schema or Unity version is unsupported: {variant}")]
    UnsupportedSchema { variant: &'static str },
    #[error("required field is missing: {path}")]
    MissingField { path: FieldPath },
    #[error("field path {path} cannot be resolved: {source}")]
    InvalidFieldPath {
        path: FieldPath,
        #[source]
        source: ValuePathError,
    },
    #[error("field {path} is owned by the {owner} semantic recipe")]
    ProtectedSemanticField {
        path: FieldPath,
        owner: &'static str,
    },
    #[error("field {path} has {actual:?}; expected {expected}")]
    WrongFieldShape {
        path: FieldPath,
        expected: &'static str,
        actual: RecipeValueKind,
    },
    #[error("field variant is ambiguous between {first} and {second}")]
    AmbiguousFieldVariant {
        first: &'static str,
        second: &'static str,
    },
    #[error("property {name:?} was not found")]
    PropertyNotFound { name: String },
    #[error("property {name:?} occurs {occurrences} times")]
    DuplicateProperty { name: String, occurrences: usize },
    #[error("field {path} is not a valid logical reference")]
    InvalidReference { path: FieldPath },
    #[error("reference required by field {path} is unresolved")]
    UnresolvedReference { path: FieldPath },
    #[error("persistent call index {index} is outside {len} calls")]
    CallIndexOutOfBounds { index: usize, len: usize },
    #[error("persistent call snapshot does not match the observed event field")]
    ListenerArgumentMismatch,
    #[error("hierarchy nodes do not share one source and object format")]
    CrossSourceHierarchy,
    #[error("a hierarchy node cannot parent itself: {child:?}")]
    SelfParent { child: ObjectAddress },
    #[error("hierarchy contains a cycle through {at:?}")]
    HierarchyCycle { at: ObjectAddress },
    #[error("hierarchy parent is missing: {parent:?}")]
    MissingParent { parent: ObjectAddress },
    #[error("hierarchy child is missing from the supplied topology: {child:?}")]
    MissingChild { child: ObjectAddress },
    #[error("hierarchy parent/child edges disagree for {child:?}")]
    ParentChildMismatch { child: ObjectAddress },
    #[error("hierarchy contains duplicate node {node:?}")]
    DuplicateHierarchyNode { node: ObjectAddress },
    #[error("hierarchy parent contains child {child:?} more than once")]
    DuplicateChildMembership { child: ObjectAddress },
    #[error("hierarchy child {child:?} belongs to more than one parent")]
    MultipleParents { child: ObjectAddress },
    #[error("child placement index {index} exceeds maximum {maximum}")]
    ChildPlacementOutOfBounds { index: usize, maximum: usize },
    #[error("non-finite values are not accepted by this recipe")]
    NonFiniteValue,
    #[error("invalid recipe payload: {reason}")]
    InvalidPayload { reason: &'static str },
    #[error("canonical recipe digest length overflow")]
    DigestLengthOverflow,
    #[error("failed to allocate {resource} capacity for {requested} elements: {message}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error(transparent)]
    FieldPath(#[from] FieldPathError),
    #[error(transparent)]
    Plan(#[from] MutationPlanError),
    #[error(transparent)]
    Workspace(Box<WorkspaceError>),
}

impl From<WorkspaceError> for RecipeError {
    fn from(error: WorkspaceError) -> Self {
        if let WorkspaceError::Budget(budget) = &error {
            return Self::Budget(budget.clone());
        }

        let mut source: Option<&(dyn std::error::Error + 'static)> = match &error {
            WorkspaceError::Operation { source, .. } => Some(source.as_ref()),
            _ => Some(&error),
        };
        while let Some(current) = source {
            if let Some(budget) = current.downcast_ref::<BudgetError>() {
                return Self::Budget(budget.clone());
            }
            if let Some(semantic) = current.downcast_ref::<SemanticDigestError>() {
                return Self::from(semantic.clone());
            }
            if let Some(semantic) = current.downcast_ref::<TypeTreeSemanticDigestError>() {
                return match semantic {
                    TypeTreeSemanticDigestError::Budget(error) => Self::Budget(error.clone()),
                    TypeTreeSemanticDigestError::Digest(error) => Self::Digest(error.clone()),
                };
            }
            if let Some(Self::Budget(budget)) = current.downcast_ref::<Self>() {
                return Self::Budget(budget.clone());
            }
            source = current.source();
        }
        Self::Workspace(Box::new(error))
    }
}

impl From<SemanticDigestError> for RecipeError {
    fn from(error: SemanticDigestError) -> Self {
        match error {
            SemanticDigestError::LengthOverflow => Self::DigestLengthOverflow,
            SemanticDigestError::ValueDepthExceeded { maximum, actual } => {
                Self::Plan(MutationPlanError::ValueDepthExceeded { maximum, actual })
            }
            SemanticDigestError::AllocationFailed {
                resource,
                requested,
                message,
            } => Self::AllocationFailed {
                resource,
                requested,
                message,
            },
            SemanticDigestError::Budget(error) => Self::Budget(error),
            SemanticDigestError::Digest(error) => Self::Digest(error),
        }
    }
}

impl RecipeError {
    #[must_use]
    pub const fn code(&self) -> Option<RecipeRejectionCode> {
        match self {
            Self::TargetUnloaded => Some(RecipeRejectionCode::TargetUnloaded),
            Self::TargetMissing => Some(RecipeRejectionCode::TargetMissing),
            Self::TargetAmbiguous { .. } => Some(RecipeRejectionCode::TargetAmbiguous),
            Self::TargetInvalid { .. } | Self::InspectionContractMismatch => {
                Some(RecipeRejectionCode::TargetInvalid)
            }
            Self::MissingSchemaProvenance => Some(RecipeRejectionCode::MissingSchemaProvenance),
            Self::WrongClass { .. } => Some(RecipeRejectionCode::WrongClass),
            Self::UnsupportedVersion => Some(RecipeRejectionCode::UnsupportedVersion),
            Self::UnsupportedSchema { .. } => Some(RecipeRejectionCode::UnsupportedSchema),
            Self::MissingField { .. } => Some(RecipeRejectionCode::MissingField),
            Self::InvalidFieldPath { .. } => Some(RecipeRejectionCode::InvalidFieldPath),
            Self::ProtectedSemanticField { .. } => {
                Some(RecipeRejectionCode::ProtectedSemanticField)
            }
            Self::WrongFieldShape { .. } => Some(RecipeRejectionCode::WrongFieldShape),
            Self::AmbiguousFieldVariant { .. } => Some(RecipeRejectionCode::AmbiguousFieldVariant),
            Self::PropertyNotFound { .. } => Some(RecipeRejectionCode::PropertyNotFound),
            Self::DuplicateProperty { .. } => Some(RecipeRejectionCode::DuplicateProperty),
            Self::InvalidReference { .. } => Some(RecipeRejectionCode::InvalidReference),
            Self::UnresolvedReference { .. } => Some(RecipeRejectionCode::UnresolvedReference),
            Self::CallIndexOutOfBounds { .. } => Some(RecipeRejectionCode::CallIndexOutOfBounds),
            Self::ListenerArgumentMismatch => Some(RecipeRejectionCode::ListenerArgumentMismatch),
            Self::CrossSourceHierarchy => Some(RecipeRejectionCode::CrossSourceHierarchy),
            Self::SelfParent { .. } => Some(RecipeRejectionCode::SelfParent),
            Self::HierarchyCycle { .. } => Some(RecipeRejectionCode::HierarchyCycle),
            Self::MissingParent { .. } => Some(RecipeRejectionCode::MissingParent),
            Self::MissingChild { .. } => Some(RecipeRejectionCode::MissingChild),
            Self::ParentChildMismatch { .. } => Some(RecipeRejectionCode::ParentChildMismatch),
            Self::DuplicateHierarchyNode { .. } => {
                Some(RecipeRejectionCode::DuplicateHierarchyNode)
            }
            Self::DuplicateChildMembership { .. } => {
                Some(RecipeRejectionCode::DuplicateChildMembership)
            }
            Self::MultipleParents { .. } => Some(RecipeRejectionCode::MultipleParents),
            Self::ChildPlacementOutOfBounds { .. } => {
                Some(RecipeRejectionCode::ChildPlacementOutOfBounds)
            }
            Self::NonFiniteValue => Some(RecipeRejectionCode::NonFiniteValue),
            Self::InvalidPayload { .. } => Some(RecipeRejectionCode::InvalidPayload),
            Self::DigestLengthOverflow
            | Self::AllocationFailed { .. }
            | Self::Budget(_)
            | Self::Digest(_)
            | Self::FieldPath(_)
            | Self::Plan(_)
            | Self::Workspace(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeValueKind {
    Null,
    Bool,
    Signed,
    Unsigned,
    Float,
    String,
    Bytes,
    Array,
    Object,
}
