use std::path::Path;

use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};
use unity_asset_yaml::{BudgetedYamlError, load_budgeted_yaml_path};

fn fixture_path() -> &'static Path {
    Path::new("tests/fixtures/MinimalGameObjectTransform.prefab")
}

#[test]
fn synchronous_path_load_accepts_exact_budget_and_rejects_one_short() {
    let path = fixture_path();
    let mut probe = AssetLoadBudget::default();
    let expected = load_budgeted_yaml_path(path, &mut probe).unwrap();
    let required = probe.usage().bytes;
    assert!(!expected.document().entries().is_empty());
    assert_eq!(expected.document().file_path(), Some(path));

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let parsed = load_budgeted_yaml_path(path, &mut exact).unwrap();
    assert_eq!(
        parsed.document().entries().len(),
        expected.document().entries().len()
    );
    assert_eq!(exact.usage().bytes, required);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        load_budgeted_yaml_path(path, &mut one_short),
        Err(BudgetedYamlError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        })) if limit == required - 1 && requested == required
    ));
}

#[test]
fn oversized_path_is_rejected_before_read_allocation() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    assert!(matches!(
        load_budgeted_yaml_path(fixture_path(), &mut budget),
        Err(BudgetedYamlError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    assert_eq!(budget.usage().bytes, 0);
}

#[cfg(feature = "async")]
#[tokio::test]
async fn asynchronous_path_load_has_the_same_exact_budget_boundary() {
    use unity_asset_core::document::AsyncUnityDocument;
    use unity_asset_yaml::{YamlDocument, load_budgeted_yaml_path_async};

    let path = fixture_path();
    let mut probe = AssetLoadBudget::default();
    let expected = load_budgeted_yaml_path_async(path, &mut probe)
        .await
        .unwrap();
    let required = probe.usage().bytes;
    assert!(!expected.document().entries().is_empty());

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    load_budgeted_yaml_path_async(path, &mut exact)
        .await
        .unwrap();
    assert_eq!(exact.usage().bytes, required);

    let mut trait_budget = AssetLoadBudget::default();
    let document =
        <YamlDocument as AsyncUnityDocument>::load_from_path_async(path, &mut trait_budget)
            .await
            .unwrap();
    assert!(!document.entries().is_empty());
    assert_eq!(trait_budget.usage().bytes, required);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: required - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        load_budgeted_yaml_path_async(path, &mut one_short).await,
        Err(BudgetedYamlError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit,
            requested,
        })) if limit == required - 1 && requested == required
    ));
}
