use std::io::Cursor;
use std::str::FromStr;

use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, BudgetedJsonError, DigestBuildError,
    DigestParseError, DigestV1, DigestV1Builder,
};

fn constrained_limits() -> AssetLoadLimits {
    AssetLoadLimits {
        max_entries: 4,
        max_bytes: 16,
        max_depth: 3,
        max_members: 2,
        max_compressed_bytes: 8,
        max_decompressed_bytes: 32,
        max_expansion_ratio: 4,
    }
}

#[test]
fn digest_is_identical_for_contiguous_and_streamed_bytes() {
    let bytes = b"a deterministic unity asset payload";
    let contiguous = DigestV1::hash_bytes(bytes);
    let streamed = DigestV1::hash_reader(Cursor::new(bytes), bytes.len() as u64).unwrap();

    assert_eq!(streamed, contiguous);
    assert_eq!(contiguous.to_string().len(), "blake3-v1:".len() + 64);
    assert_eq!(
        contiguous.to_string().parse::<DigestV1>().unwrap(),
        contiguous
    );
}

#[test]
fn digest_serialization_has_an_explicit_version_tag() {
    let digest = DigestV1::hash_bytes(b"versioned");
    let json = serde_json::to_string(&digest).unwrap();

    assert!(json.contains("blake3-v1:"));
    assert_eq!(serde_json::from_str::<DigestV1>(&json).unwrap(), digest);
    assert!(serde_json::from_str::<DigestV1>("\"sha256-v1:00\"").is_err());
}

#[test]
fn digest_v1_empty_input_conformance_vector_is_stable() {
    assert_eq!(
        DigestV1::hash_bytes(b"").to_string(),
        "blake3-v1:6912679da874d099305916f5a49fa0bbb1d86072905013b1aaaca9d61cefabd1"
    );
}

#[test]
fn streamed_digest_rejects_short_and_trailing_input() {
    let short = DigestV1::hash_reader(Cursor::new(b"short"), 12).unwrap_err();
    assert_eq!(short.kind(), std::io::ErrorKind::UnexpectedEof);

    let trailing = DigestV1::hash_reader(Cursor::new(b"payload-plus"), 7).unwrap_err();
    assert_eq!(trailing.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn digest_builder_is_exact_and_failure_atomic() {
    let bytes = b"chunked catalog bytes";
    let mut builder = DigestV1Builder::new(bytes.len() as u64);
    builder.update(&bytes[..7]).unwrap();
    let before = builder.consumed_bytes();
    assert!(matches!(
        builder.update(&[0; 64]),
        Err(DigestBuildError::DeclaredLengthExceeded { .. })
    ));
    assert_eq!(builder.consumed_bytes(), before);
    builder.update(&bytes[7..]).unwrap();
    assert_eq!(builder.finalize().unwrap(), DigestV1::hash_bytes(bytes));

    let mut short = DigestV1Builder::new(2);
    short.update(b"a").unwrap();
    assert!(matches!(
        short.finalize(),
        Err(DigestBuildError::DeclaredLengthMismatch { .. })
    ));
}

#[test]
fn framed_digest_builder_is_unambiguous_and_failure_atomic() {
    let first_components = [b"ab".as_slice(), b"c".as_slice()];
    let second_components = [b"a".as_slice(), b"bc".as_slice()];

    let hash_components = |components: &[&[u8]]| {
        let declared_length = components
            .iter()
            .try_fold(0_u64, |total, component| {
                total
                    .checked_add(DigestV1Builder::framed_len(component)?)
                    .ok_or(DigestBuildError::LengthOverflow)
            })
            .unwrap();
        let mut builder = DigestV1Builder::new(declared_length);
        for component in components {
            builder.update_framed(component).unwrap();
        }
        builder.finalize().unwrap()
    };

    assert_ne!(
        hash_components(&first_components),
        hash_components(&second_components)
    );

    let framed_length = DigestV1Builder::framed_len(b"payload").unwrap();
    let mut short = DigestV1Builder::new(framed_length - 1);
    assert!(matches!(
        short.update_framed(b"payload"),
        Err(DigestBuildError::DeclaredLengthExceeded { .. })
    ));
    assert_eq!(short.consumed_bytes(), 0);
}

#[test]
fn digest_parser_checks_fixed_length_before_hex_allocation() {
    let oversized = format!("blake3-v1:{}", "00".repeat(1_000_000));
    assert!(matches!(
        DigestV1::from_str(&oversized),
        Err(DigestParseError::InvalidEncodedLength { .. })
    ));
}

#[test]
fn budget_charges_every_resource_at_its_limit() {
    let mut budget = AssetLoadBudget::new(constrained_limits()).unwrap();

    budget.consume_entries(4).unwrap();
    budget.consume_bytes(16).unwrap();
    budget.observe_depth(3).unwrap();
    budget.consume_members(2).unwrap();
    budget.begin_decompression().consume(8, 32).unwrap();

    assert!(matches!(
        budget.consume_entries(1),
        Err(BudgetError::Exceeded {
            resource: "entries",
            ..
        })
    ));
    assert!(matches!(
        budget.observe_depth(4),
        Err(BudgetError::Exceeded {
            resource: "depth",
            ..
        })
    ));
    assert!(matches!(
        budget.begin_decompression().consume(1, 0),
        Err(BudgetError::Exceeded {
            resource: "compressed_bytes",
            ..
        })
    ));
}

#[test]
fn budget_failures_leave_usage_unchanged() {
    let mut budget = AssetLoadBudget::new(constrained_limits()).unwrap();
    budget.consume_entries(3).unwrap();
    budget.begin_decompression().consume(2, 8).unwrap();
    let before = budget.usage();

    assert!(budget.consume_entries(2).is_err());
    assert_eq!(budget.usage(), before);
    assert!(budget.begin_decompression().consume(0, 1).is_err());
    assert_eq!(budget.usage(), before);
}

#[test]
fn compressed_input_preflight_is_cumulative_and_does_not_charge_usage() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_compressed_bytes: 4,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    budget.begin_decompression().consume(3, 0).unwrap();
    let before = budget.usage();

    budget.check_compressed_bytes(1).unwrap();
    assert_eq!(budget.usage(), before);
    assert!(matches!(
        budget.check_compressed_bytes(2),
        Err(BudgetError::Exceeded {
            resource: "compressed_bytes",
            limit: 4,
            requested: 5,
        })
    ));
    assert_eq!(budget.usage(), before);
}

#[test]
fn byte_allocation_preflight_is_cumulative_and_does_not_charge_usage() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 4,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    budget.consume_bytes(3).unwrap();
    let before = budget.usage();

    budget.check_bytes(1).unwrap();
    assert_eq!(budget.usage(), before);
    assert!(matches!(
        budget.check_bytes(2),
        Err(BudgetError::Exceeded {
            resource: "bytes",
            limit: 4,
            requested: 5,
        })
    ));
    assert_eq!(budget.usage(), before);
}

#[test]
fn structural_preflights_are_cumulative_and_do_not_charge_usage() {
    let mut budget = AssetLoadBudget::new(constrained_limits()).unwrap();
    budget.consume_entries(3).unwrap();
    budget.consume_members(1).unwrap();
    budget.observe_depth(2).unwrap();
    let before = budget.usage();

    budget.check_entries(1).unwrap();
    budget.check_members(1).unwrap();
    budget.check_depth(3).unwrap();
    assert_eq!(budget.usage(), before);

    assert!(budget.check_entries(2).is_err());
    assert!(budget.check_members(2).is_err());
    assert!(budget.check_depth(4).is_err());
    assert_eq!(budget.usage(), before);
}

#[test]
fn depth_scopes_compose_and_restore_the_outer_base() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 8,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    {
        let mut outer = budget.enter_depth(2).unwrap();
        outer.observe_depth(1).unwrap();
        assert_eq!(outer.usage().max_observed_depth, 3);

        {
            let mut inner = outer.enter_depth(3).unwrap();
            inner.observe_depth(1).unwrap();
            assert_eq!(inner.usage().max_observed_depth, 6);
        }

        outer.observe_depth(3).unwrap();
        assert_eq!(outer.usage().max_observed_depth, 6);
    }

    budget.observe_depth(5).unwrap();
    assert_eq!(budget.usage().max_observed_depth, 6);
}

#[test]
fn rejected_depth_scopes_and_observations_are_failure_atomic() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 5,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    budget.consume_entries(1).unwrap();
    budget.consume_bytes(2).unwrap();

    {
        let mut outer = budget.enter_depth(4).unwrap();
        let before = outer.usage();
        assert!(matches!(
            outer.enter_depth(2),
            Err(BudgetError::Exceeded {
                resource: "depth",
                limit: 5,
                requested: 6,
            })
        ));
        assert_eq!(outer.usage(), before);

        outer.observe_depth(1).unwrap();
        let before = outer.usage();
        assert!(matches!(
            outer.observe_depth(2),
            Err(BudgetError::Exceeded {
                resource: "depth",
                limit: 5,
                requested: 6,
            })
        ));
        assert_eq!(outer.usage(), before);
    }

    budget.observe_depth(3).unwrap();
    assert_eq!(budget.usage().max_observed_depth, 5);
    assert_eq!(budget.usage().entries, 1);
    assert_eq!(budget.usage().bytes, 2);
}

#[test]
fn overflowing_nested_depth_scope_preserves_the_active_base() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: u32::MAX,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    {
        let mut outer = budget.enter_depth(u32::MAX).unwrap();
        let before = outer.usage();
        assert!(matches!(
            outer.enter_depth(1),
            Err(BudgetError::ArithmeticOverflow { resource: "depth" })
        ));
        assert_eq!(outer.usage(), before);
        outer.observe_depth(0).unwrap();
    }

    budget.observe_depth(1).unwrap();
    assert_eq!(budget.usage().max_observed_depth, u32::MAX);
}

#[test]
fn decompression_preflight_checks_cumulative_limits_without_charging() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_compressed_bytes: 5,
        max_decompressed_bytes: 8,
        max_expansion_ratio: 2,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    budget.begin_decompression().consume(3, 6).unwrap();
    let before = budget.usage();

    budget.check_decompression(2, 2).unwrap();
    assert_eq!(budget.usage(), before);

    assert!(matches!(
        budget.check_decompression(2, 3),
        Err(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            limit: 8,
            requested: 9,
        })
    ));
    assert_eq!(budget.usage(), before);
}

#[test]
fn decompression_preflight_checks_each_stream_ratio_without_charging() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_compressed_bytes: 1_000,
        max_decompressed_bytes: 1_000,
        max_expansion_ratio: 4,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    budget.begin_decompression().consume(100, 100).unwrap();
    let before = budget.usage();

    assert!(matches!(
        budget.check_decompression(2, 9),
        Err(BudgetError::ExpansionRatioExceeded {
            compressed_bytes: 2,
            decompressed_bytes: 9,
            max_ratio: 4,
        })
    ));
    assert_eq!(budget.usage(), before);
}

#[test]
fn budgeted_json_bounds_the_encoded_document_before_serde_allocations() {
    let mut budget = AssetLoadBudget::new(constrained_limits()).unwrap();
    let value: String = budget.deserialize_json(Cursor::new(br#""small""#)).unwrap();
    assert_eq!(value, "small");
    assert_eq!(budget.usage().bytes, 7);

    let mut budget = AssetLoadBudget::new(constrained_limits()).unwrap();
    let oversized = format!(r#""{}""#, "x".repeat(17));
    assert!(matches!(
        budget.deserialize_json::<String>(Cursor::new(oversized)),
        Err(BudgetedJsonError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));
    assert_eq!(budget.usage().bytes, 0);
}

#[test]
fn decompression_ratio_is_enforced_across_chunks() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: 1_000,
        max_expansion_ratio: 4,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    {
        let mut stream = budget.begin_decompression();
        stream.consume(10, 20).unwrap();
        stream.consume(1, 20).unwrap();
        let before = stream.usage();
        assert!(matches!(
            stream.consume(1, 9),
            Err(BudgetError::ExpansionRatioExceeded {
                compressed_bytes: 12,
                decompressed_bytes: 49,
                ..
            })
        ));
        assert_eq!(stream.usage(), before);
    }
    assert_eq!(budget.usage().compressed_bytes, 11);
    assert_eq!(budget.usage().decompressed_bytes, 40);
}

#[test]
fn decompression_streams_cannot_borrow_ratio_allowance() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: 2_000,
        max_expansion_ratio: 4,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    budget.begin_decompression().consume(250, 0).unwrap();
    let before = budget.usage();
    assert!(matches!(
        budget.begin_decompression().consume(1, 1_000),
        Err(BudgetError::ExpansionRatioExceeded {
            compressed_bytes: 1,
            decompressed_bytes: 1_000,
            ..
        })
    ));
    assert_eq!(budget.usage(), before);
}

#[test]
fn budget_uses_checked_arithmetic() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: u64::MAX,
        max_bytes: u64::MAX,
        max_depth: u32::MAX,
        max_members: u64::MAX,
        max_compressed_bytes: u64::MAX,
        max_decompressed_bytes: u64::MAX,
        max_expansion_ratio: u32::MAX,
    })
    .unwrap();

    budget.consume_bytes(u64::MAX).unwrap();
    assert!(matches!(
        budget.consume_bytes(1),
        Err(BudgetError::ArithmeticOverflow { resource: "bytes" })
    ));
    budget
        .begin_decompression()
        .consume(u64::MAX, u64::MAX)
        .unwrap();
    assert!(matches!(
        budget.begin_decompression().consume(1, 0),
        Err(BudgetError::ArithmeticOverflow {
            resource: "compressed_bytes"
        })
    ));
}

#[test]
fn all_non_depth_budget_limits_must_be_nonzero() {
    let invalid = [
        AssetLoadLimits {
            max_entries: 0,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_bytes: 0,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_members: 0,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_compressed_bytes: 0,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_decompressed_bytes: 0,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_expansion_ratio: 0,
            ..AssetLoadLimits::default()
        },
    ];

    for limits in invalid {
        assert!(matches!(
            AssetLoadBudget::new(limits),
            Err(BudgetError::InvalidLimit { .. })
        ));
    }
}

#[test]
fn zero_depth_budget_allows_only_local_depth_zero() {
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 0,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    budget.observe_depth(0).unwrap();
    assert!(matches!(
        budget.observe_depth(1),
        Err(BudgetError::Exceeded {
            resource: "depth",
            limit: 0,
            requested: 1,
        })
    ));
}
