use std::fs;

use unity_asset::schema::{RecipeId, recipe_capabilities};
use unity_asset::workspace::{
    AssetWorkspace, COMMIT_REPORT_VERSION, CommitReport, FieldGuard, GenericMutation,
    MUTATION_PLAN_VERSION, MutationPlan, MutationValue, PREPARE_REPORT_VERSION, PrepareOptions,
    PublicationTarget, RECOVERY_LOCATOR_VERSION, RECOVERY_OUTCOME_VERSION, RecoveryLocator,
    RecoveryOutcome, SourceExpectation, SourceOpenRequest, WORKSPACE_CAPABILITY_CATALOG_CONTRACT,
    WORKSPACE_CAPABILITY_CATALOG_VERSION, WorkspaceCapability, WorkspaceLookup, WorkspaceView,
    workspace_capabilities,
};
use unity_asset::{
    AssetLoadBudget, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint, SourceKind,
    SourceLocator, UnityClass, UnityValue,
};
use unity_asset_core::{semantic_value_digest, yaml_field_schema_digest};

const SOURCE_ALIAS: &str = "agent-native.prefab";
const YAML: &[u8] =
    b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn object_address() -> ObjectAddress {
    ObjectAddress::yaml(
        SourceLocator::path(SOURCE_ALIAS).expect("source locator"),
        "1",
    )
    .expect("object address")
}

fn name_path() -> FieldPath {
    FieldPath::root()
        .push_field("m_Name")
        .expect("name field path")
}

fn name_guard() -> FieldGuard {
    let class = UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let path = name_path();
    let value = UnityValue::String("Before".to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &path, &value, &mut budget).expect("field schema digest"),
        semantic_value_digest(&value, &mut budget).expect("semantic value digest"),
    )
}

fn mutation_plan(workspace: &AssetWorkspace) -> MutationPlan {
    MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            SourceLocator::path(SOURCE_ALIAS).expect("source locator"),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: object_address(),
            path: name_path(),
            guard: name_guard(),
            replacement: MutationValue::string("After").expect("replacement value"),
        }],
    )
    .expect("mutation plan")
}

fn read_name(view: &impl WorkspaceView) -> String {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(&object_address(), &mut budget)
        .expect("resolve object")
    else {
        panic!("fixture object must resolve");
    };
    view.read_object(&handle, &mut budget)
        .expect("read object")
        .class()
        .value_at_path(&name_path())
        .expect("name value")
        .as_str()
        .expect("string name")
        .to_owned()
}

fn assert_field_order(encoded: &str, fields: &[&str]) {
    let mut cursor = 0;
    for field in fields {
        let marker = format!("\"{field}\":");
        let offset = encoded[cursor..]
            .find(&marker)
            .unwrap_or_else(|| panic!("missing structured field {field}"));
        cursor += offset + marker.len();
    }
}

#[test]
fn public_capabilities_are_machine_discoverable_without_a_command_bus() {
    let catalog = workspace_capabilities();

    assert_eq!(catalog.contract(), WORKSPACE_CAPABILITY_CATALOG_CONTRACT);
    assert_eq!(
        catalog.contract_version(),
        WORKSPACE_CAPABILITY_CATALOG_VERSION
    );
    assert!(catalog.capabilities().contains(&WorkspaceCapability::Plan));
    assert!(
        catalog
            .capabilities()
            .contains(&WorkspaceCapability::Prepare)
    );
    assert!(
        catalog
            .capabilities()
            .contains(&WorkspaceCapability::Preview)
    );
    assert!(
        catalog
            .capabilities()
            .contains(&WorkspaceCapability::Commit)
    );
    assert!(
        catalog
            .capabilities()
            .contains(&WorkspaceCapability::Recover)
    );
    assert_eq!(catalog.contracts().mutation_plan(), MUTATION_PLAN_VERSION);
    assert_eq!(catalog.contracts().prepare_report(), PREPARE_REPORT_VERSION);
    assert_eq!(catalog.contracts().commit_report(), COMMIT_REPORT_VERSION);
    assert_eq!(
        catalog.contracts().recovery_locator(),
        RECOVERY_LOCATOR_VERSION
    );
    assert_eq!(
        catalog.contracts().recovery_outcome(),
        RECOVERY_OUTCOME_VERSION
    );
    assert!(!catalog.prepared_authority().serializable());
    assert!(!catalog.prepared_authority().reconstructible_from_report());
    assert!(catalog.prepared_authority().single_use());
    assert!(catalog.prepared_authority().commit_consumes());
    assert!(catalog.prepared_authority().preview_available());
    assert!(catalog.automation().structured_input());
    assert!(!catalog.automation().display_text_input());
    assert!(!catalog.automation().generic_command_bus());

    let wire = serde_json::to_value(catalog).expect("serialize workspace capabilities");
    assert_eq!(
        wire["contract"],
        serde_json::json!(WORKSPACE_CAPABILITY_CATALOG_CONTRACT)
    );
    assert!(wire.get("command").is_none());
    assert_eq!(
        wire["automation"],
        serde_json::json!({
            "structured_input": true,
            "display_text_input": false,
            "generic_command_bus": false,
        })
    );

    let recipes = recipe_capabilities();
    assert_eq!(recipes.version(), 1);
    assert!(
        recipes
            .recipes()
            .iter()
            .any(|recipe| recipe.id() == RecipeId::ReferenceRetargetV1)
    );
    assert!(
        recipes
            .recipes()
            .iter()
            .any(|recipe| recipe.id() == RecipeId::AudioClipStreamedResourceV1)
    );
    let recipe_wire = serde_json::to_value(recipes).expect("serialize recipe capabilities");
    for recipe in recipe_wire["recipes"]
        .as_array()
        .expect("recipe capability array")
    {
        assert!(recipe["id"].is_string());
        assert!(
            !recipe["formats"]
                .as_array()
                .expect("recipe formats")
                .is_empty()
        );
        assert!(
            !recipe["preconditions"]
                .as_array()
                .expect("recipe preconditions")
                .is_empty()
        );
        assert!(
            !recipe["outputs"]
                .as_array()
                .expect("recipe outputs")
                .is_empty()
        );
    }
}

#[test]
fn structured_plan_prepare_preview_commit_and_recovery_form_one_public_workflow() {
    let directory = tempfile::tempdir().expect("temporary project");
    let path = directory.path().join(SOURCE_ALIAS);
    fs::write(&path, YAML).expect("write YAML fixture");

    let mut workspace = AssetWorkspace::new().expect("workspace");
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(SOURCE_ALIAS).expect("source alias"))
                .with_kind_hint(SourceKind::Yaml),
            &mut AssetLoadBudget::default(),
        )
        .expect("load source");
    let base = workspace.snapshot();

    let plan = mutation_plan(&workspace);
    let plan_digest = plan.digest().expect("plan digest");
    let plan_json = plan.canonical_json().expect("canonical mutation plan");
    let plan_value =
        serde_json::from_slice::<serde_json::Value>(&plan_json).expect("structured plan JSON");
    assert_eq!(
        plan_value["version"],
        serde_json::json!(MUTATION_PLAN_VERSION)
    );
    assert_eq!(
        plan_value["workspace_id"],
        serde_json::json!(workspace.workspace_id())
    );
    assert!(plan_value["operations"][0]["action"]["target"].is_object());
    assert!(plan_value.get("command").is_none());
    let mut untyped_plan = plan_value;
    untyped_plan
        .as_object_mut()
        .expect("plan object")
        .insert("command".to_owned(), serde_json::json!("prepare"));
    let untyped_plan = serde_json::to_vec(&untyped_plan).expect("serialize invalid command bus");
    assert!(MutationPlan::from_json_slice(&untyped_plan, &mut AssetLoadBudget::default()).is_err());

    let plan = MutationPlan::from_json_slice(&plan_json, &mut AssetLoadBudget::default())
        .expect("read canonical mutation plan");
    let prepared = workspace
        .prepare(
            plan,
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .expect("prepare mutation");
    let prepare_report = prepared.report();
    let prepare_wire = serde_json::to_value(prepare_report).expect("serialize prepare report");
    assert_eq!(
        prepare_wire["version"],
        serde_json::json!(PREPARE_REPORT_VERSION)
    );
    assert_eq!(
        prepare_wire["workspace_id"],
        serde_json::json!(workspace.workspace_id())
    );
    assert_eq!(prepare_wire["operation_count"], serde_json::json!(1));
    assert_eq!(prepare_report.plan_digest(), plan_digest);

    let prepared_view = prepared.view();
    assert_eq!(prepared_view.workspace_id(), workspace.workspace_id());
    assert_eq!(prepared_view.base_revision(), base.revision());
    assert_eq!(prepared_view.revision(), prepare_report.prepared_revision());
    assert_eq!(prepared_view.plan_digest(), plan_digest);
    assert_eq!(read_name(&base), "Before");
    assert_eq!(read_name(&prepared_view), "After");
    assert_eq!(read_name(&workspace.snapshot()), "Before");

    let prepared_revision = prepared_view.revision();
    let commit_report = workspace
        .commit(
            prepared,
            PublicationTarget::in_place(directory.path()).expect("publication target"),
            &mut AssetLoadBudget::default(),
        )
        .expect("commit prepared change");
    assert_eq!(commit_report.version(), COMMIT_REPORT_VERSION);
    assert_eq!(commit_report.workspace_id(), workspace.workspace_id());
    assert_eq!(commit_report.base_revision(), base.revision());
    assert_eq!(commit_report.committed_revision(), prepared_revision);
    assert_eq!(commit_report.plan_digest(), plan_digest);
    assert_eq!(
        commit_report.changes().transaction(),
        commit_report.transaction()
    );
    assert_eq!(workspace.revision(), prepared_revision);
    assert_eq!(read_name(&prepared_view), "After");
    assert_eq!(read_name(&workspace.snapshot()), "After");
    assert!(
        String::from_utf8(fs::read(&path).expect("read committed YAML"))
            .expect("UTF-8 YAML")
            .contains("m_Name: After")
    );

    let commit_json = serde_json::to_vec(&commit_report).expect("serialize commit report");
    let decoded_report =
        serde_json::from_slice::<CommitReport>(&commit_json).expect("deserialize commit report");
    assert_eq!(decoded_report, commit_report);

    let locator_json =
        serde_json::to_string(commit_report.recovery()).expect("serialize recovery locator");
    assert_field_order(
        &locator_json,
        &["version", "root", "transaction", "root_identity"],
    );
    let locator_value =
        serde_json::from_str::<serde_json::Value>(&locator_json).expect("locator JSON");
    assert_eq!(
        locator_value["version"],
        serde_json::json!(RECOVERY_LOCATOR_VERSION)
    );
    assert_eq!(locator_value.as_object().expect("locator object").len(), 4);
    let locator = serde_json::from_value::<RecoveryLocator>(locator_value.clone())
        .expect("deserialize recovery locator");
    assert_eq!(&locator, commit_report.recovery());

    let mut unsupported_locator = locator_value.clone();
    unsupported_locator["version"] = serde_json::json!(RECOVERY_LOCATOR_VERSION + 1);
    assert!(serde_json::from_value::<RecoveryLocator>(unsupported_locator).is_err());
    let mut untyped_locator = locator_value;
    untyped_locator
        .as_object_mut()
        .expect("locator object")
        .insert("command".to_owned(), serde_json::json!("recover"));
    assert!(serde_json::from_value::<RecoveryLocator>(untyped_locator).is_err());

    drop(prepared_view);
    drop(base);
    drop(workspace);
    let outcome = AssetWorkspace::recover_at(&locator, &mut AssetLoadBudget::default())
        .expect("read historical recovery outcome");
    assert_eq!(
        outcome,
        RecoveryOutcome::HistoricalCommitReceipt(Box::new(commit_report.clone()))
    );

    let outcome_json = serde_json::to_string(&outcome).expect("serialize recovery outcome");
    assert_field_order(&outcome_json, &["version", "outcome"]);
    let outcome_value =
        serde_json::from_str::<serde_json::Value>(&outcome_json).expect("outcome JSON");
    assert_eq!(
        outcome_value["version"],
        serde_json::json!(RECOVERY_OUTCOME_VERSION)
    );
    assert_eq!(
        outcome_value["outcome"]["status"],
        serde_json::json!("historical_commit_receipt")
    );
    assert_eq!(
        outcome_value["outcome"]["report"]["recovery"]["version"],
        serde_json::json!(RECOVERY_LOCATOR_VERSION)
    );
    assert_eq!(
        serde_json::from_value::<RecoveryOutcome>(outcome_value.clone())
            .expect("deserialize historical recovery outcome"),
        outcome
    );

    let mut unsupported_outcome = outcome_value.clone();
    unsupported_outcome["version"] = serde_json::json!(RECOVERY_OUTCOME_VERSION + 1);
    assert!(serde_json::from_value::<RecoveryOutcome>(unsupported_outcome).is_err());
    let mut untyped_outcome = outcome_value;
    untyped_outcome["outcome"]
        .as_object_mut()
        .expect("tagged outcome")
        .insert("command".to_owned(), serde_json::json!("finalize"));
    assert!(serde_json::from_value::<RecoveryOutcome>(untyped_outcome).is_err());
}
