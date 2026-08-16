//! Schema-aware mutation recipes over immutable workspace observations.

mod event;
mod hierarchy;
mod material;
mod recipe;
mod resource;

pub use event::{
    PersistentArgument, PersistentCall, PersistentCallShape, PersistentCallState, UnityEventEdit,
    UnityEventRecipe,
};
pub use hierarchy::{
    HierarchyDestinationV1, HierarchyIntentV1, HierarchyPlacementV1, HierarchyRecipe, Quaternion,
    RectTransformChange, TransformChange, TransformRecipe, Vector2, Vector3,
};
pub use material::{MaterialRecipe, MaterialTextureChange};
pub use recipe::{
    BinarySchemaVersion, DeclaredUnityVersion, RecipeApplicability, RecipeApplicabilityStatus,
    RecipeCapability, RecipeCapabilityCatalog, RecipeClassConstraint, RecipeError, RecipeId,
    RecipeLowering, RecipeLoweringReport, RecipeMutationKind, RecipeObject, RecipeObjectFormat,
    RecipePrecondition, RecipeRejectionCode, SchemaOrigin, SchemaProvenance, SchemaRecipePlanner,
    SchemaVariantId, recipe_capabilities,
};
pub use resource::AudioClipResourceRecipe;

pub(crate) use recipe::protected_plain_field_owner;
