use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use unity_asset_binary::error::BinaryError;
use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, InMemoryTypeTreeRegistry, JsonTypeTreeRegistry, TypeTreeRegistry,
};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError};

#[repr(C)]
struct ArcAllocation<T> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    value: T,
}

fn arc_allocation_bytes<T>() -> u64 {
    u64::try_from(size_of::<ArcAllocation<T>>()).unwrap()
}

fn minimal_json_registry() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), br#"{"schema":1,"entries":[]}"#).unwrap();
    file
}

fn limits_for(usage: AssetLoadUsage) -> AssetLoadLimits {
    AssetLoadLimits {
        max_entries: usage.entries.max(1),
        max_bytes: usage.bytes.max(1),
        max_depth: usage.max_observed_depth.max(1),
        max_members: usage.members.max(1),
        ..AssetLoadLimits::default()
    }
}

fn assert_bytes_budget(error: BinaryError, limit: u64, requested: u64) {
    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit: actual_limit,
            requested: actual_requested,
        }) if actual_limit == limit && actual_requested == requested
    ));
}

#[test]
fn single_path_factory_accounts_for_child_arc_without_a_wrapper() {
    let file = minimal_json_registry();
    let mut direct_budget = AssetLoadBudget::default();
    JsonTypeTreeRegistry::from_path(file.path(), &mut direct_budget).unwrap();
    let direct = direct_budget.usage();

    let child_arc_bytes = arc_allocation_bytes::<JsonTypeTreeRegistry>();
    let expected = AssetLoadUsage {
        members: direct.members + 1,
        bytes: direct.bytes + child_arc_bytes,
        ..direct
    };

    let mut budget = AssetLoadBudget::new(limits_for(expected)).unwrap();
    let registry = CompositeTypeTreeRegistry::from_paths(&[file.path()], &mut budget)
        .unwrap()
        .expect("one path produces a registry");

    assert_eq!(budget.usage(), expected);
    assert!(registry.resolve("2022.3.0f1", 28).is_none());
}

#[test]
fn single_path_factory_rejects_child_arc_one_byte_short() {
    let file = minimal_json_registry();
    let mut direct_budget = AssetLoadBudget::default();
    JsonTypeTreeRegistry::from_path(file.path(), &mut direct_budget).unwrap();
    let direct = direct_budget.usage();

    let child_arc_bytes = arc_allocation_bytes::<JsonTypeTreeRegistry>();
    let child_requested = direct.bytes + child_arc_bytes;

    let child_limits = AssetLoadLimits {
        max_bytes: child_requested - 1,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(child_limits).unwrap();
    let error = CompositeTypeTreeRegistry::from_paths(&[file.path()], &mut budget).unwrap_err();
    assert_bytes_budget(error, child_requested - 1, child_requested);
}

#[test]
fn multiple_path_factory_accounts_for_table_children_and_composite_arc() {
    let file = minimal_json_registry();
    let mut direct_budget = AssetLoadBudget::default();
    JsonTypeTreeRegistry::from_path(file.path(), &mut direct_budget).unwrap();
    let direct = direct_budget.usage();

    let table_bytes = u64::try_from(2 * size_of::<Arc<dyn TypeTreeRegistry>>()).unwrap();
    let child_arc_bytes = 2 * arc_allocation_bytes::<JsonTypeTreeRegistry>();
    let composite_arc_bytes = arc_allocation_bytes::<CompositeTypeTreeRegistry>();
    let expected = AssetLoadUsage {
        entries: direct.entries * 2,
        bytes: direct.bytes * 2 + table_bytes + child_arc_bytes + composite_arc_bytes,
        max_observed_depth: direct.max_observed_depth,
        members: direct.members * 2 + 2,
        compressed_bytes: direct.compressed_bytes * 2,
        decompressed_bytes: direct.decompressed_bytes * 2,
    };

    let mut budget = AssetLoadBudget::new(limits_for(expected)).unwrap();
    CompositeTypeTreeRegistry::from_paths(&[file.path(), file.path()], &mut budget).unwrap();
    assert_eq!(budget.usage(), expected);

    let limits = AssetLoadLimits {
        max_bytes: expected.bytes - 1,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let error = CompositeTypeTreeRegistry::from_paths(&[file.path(), file.path()], &mut budget)
        .unwrap_err();
    assert_bytes_budget(error, expected.bytes - 1, expected.bytes);
}

#[test]
fn compose_reuses_zero_or_one_registry_and_budgets_two() {
    let left: Arc<dyn TypeTreeRegistry> = Arc::new(InMemoryTypeTreeRegistry::default());
    let right: Arc<dyn TypeTreeRegistry> = Arc::new(InMemoryTypeTreeRegistry::default());

    let mut budget = AssetLoadBudget::default();
    assert!(
        CompositeTypeTreeRegistry::compose(&[], &mut budget)
            .unwrap()
            .is_none()
    );
    let single = CompositeTypeTreeRegistry::compose(std::slice::from_ref(&left), &mut budget)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&single, &left));
    assert_eq!(budget.usage(), AssetLoadUsage::default());

    let table_bytes = u64::try_from(2 * size_of::<Arc<dyn TypeTreeRegistry>>()).unwrap();
    let composite_arc_bytes = arc_allocation_bytes::<CompositeTypeTreeRegistry>();
    let required_bytes = table_bytes + composite_arc_bytes;
    let limits = AssetLoadLimits {
        max_bytes: required_bytes,
        max_members: 2,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let composite = CompositeTypeTreeRegistry::compose(&[left.clone(), right], &mut budget)
        .unwrap()
        .unwrap();
    assert_eq!(budget.usage().bytes, required_bytes);
    assert_eq!(budget.usage().members, 2);
    assert!(!Arc::ptr_eq(&composite, &left));

    let limits = AssetLoadLimits {
        max_bytes: required_bytes - 1,
        max_members: 2,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let error = CompositeTypeTreeRegistry::compose(&[left.clone(), left], &mut budget).unwrap_err();
    assert_bytes_budget(error, required_bytes - 1, required_bytes);
}
