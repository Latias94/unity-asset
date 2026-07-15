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
fn every_budget_configuration_limit_is_nonzero() {
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
            max_depth: 0,
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
