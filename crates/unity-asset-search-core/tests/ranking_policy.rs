use std::cell::Cell;

use unity_asset_search_core::{
    CandidateFacts, CandidateField, FuzzyFallbackPolicy, MatchCountRelation, MatchField, MatchKind,
    RetrievalEvidence, RetrievalStage, SearchDiagnostic, SearchKind, SearchLimits, SearchOutcome,
    SearchPolicy, SearchRequest, highlight_html,
};

#[test]
fn search_kind_catalog_owns_canonical_names_and_filter_aliases() {
    let canonical_names = SearchKind::ALL
        .iter()
        .copied()
        .map(SearchKind::canonical_name)
        .collect::<Vec<_>>();
    assert_eq!(
        canonical_names,
        vec![
            "Prefab",
            "Scene",
            "Material",
            "Script",
            "AnimationClip",
            "AnimatorController",
            "Asset",
            "Shader",
            "Texture",
            "Audio",
            "BundleContainer",
            "File",
        ]
    );
    for &kind in SearchKind::ALL {
        assert_eq!(SearchKind::from_filter(kind.canonical_name()), Some(kind));
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::json!(kind.canonical_name())
        );
        let prepared = SearchPolicy::default().prepare(SearchRequest::new(
            format!("t:{}", kind.canonical_name()),
            1,
        ));
        assert_eq!(prepared.query().type_filter_kind(), Some(kind));
        assert_eq!(
            serde_json::to_value(prepared.query()).unwrap()["type_filter"],
            kind.canonical_name()
        );
    }
    assert_eq!(SearchKind::from_filter("mat"), Some(SearchKind::Material));
    assert_eq!(
        SearchKind::from_filter("bundle-container"),
        Some(SearchKind::BundleContainer)
    );
    assert_eq!(SearchKind::from_filter("unknown-kind"), None);
}

fn candidate(
    stable_key: &str,
    name: &str,
    path: &str,
    kind: &str,
    retrieval_score: i64,
) -> CandidateFacts {
    CandidateFacts::new(stable_key, name, path, kind, retrieval_score)
}

fn execute(query: &str, candidates: Vec<CandidateFacts>) -> SearchOutcome {
    SearchPolicy::default()
        .prepare(SearchRequest::new(query, 20))
        .execute(candidates)
}

#[test]
fn freezes_exact_prefix_token_and_fuzzy_corpus() {
    let exact = execute(
        "button",
        vec![candidate(
            "exact",
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            10,
        )],
    );
    assert_eq!(exact.matches[0].match_kind, MatchKind::Exact);
    assert!(!exact.fallback_used);

    let prefix = execute(
        "but",
        vec![candidate(
            "prefix",
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            10,
        )],
    );
    assert_eq!(prefix.matches[0].match_kind, MatchKind::Prefix);
    assert!(!prefix.fallback_used);

    let token = execute(
        "button",
        vec![candidate(
            "token",
            "Primary Button Icon",
            "Assets/UI/PrimaryButtonIcon.prefab",
            "Prefab",
            10,
        )],
    );
    assert_eq!(token.matches[0].match_kind, MatchKind::Token);
    assert!(!token.fallback_used);

    let fuzzy = execute(
        "buton",
        vec![candidate(
            "fuzzy",
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            10,
        )],
    );
    assert_eq!(fuzzy.matches[0].match_kind, MatchKind::Fuzzy);
    assert!(fuzzy.fallback_used);
}

#[test]
fn quoted_phrase_is_one_term_and_filters_are_policy_owned() {
    let outcome = execute(
        r#"type:prefab in:"Assets/UI" "Start Button""#,
        vec![
            candidate(
                "phrase",
                "Start Button",
                "Assets/UI/StartButton.prefab",
                "Prefab",
                1,
            ),
            candidate(
                "not-a-phrase",
                "Start Blue Button",
                "Assets/UI/StartBlueButton.prefab",
                "Prefab",
                100,
            ),
            candidate(
                "wrong-type",
                "Start Button",
                "Assets/UI/StartButton.mat",
                "Material",
                100,
            ),
            candidate(
                "wrong-path",
                "Start Button",
                "Packages/UI/StartButton.prefab",
                "Prefab",
                100,
            ),
        ],
    );

    assert!(outcome.diagnostics.is_empty());
    assert_eq!(outcome.query.type_filter(), Some("Prefab"));
    assert_eq!(outcome.query.type_filter_kind(), Some(SearchKind::Prefab));
    assert_eq!(
        serde_json::to_value(&outcome.query).unwrap()["type_filter"],
        "Prefab"
    );
    assert_eq!(outcome.query.path_prefix(), Some("Assets/UI"));
    assert_eq!(outcome.query.terms().len(), 1);
    assert!(outcome.query.terms()[0].is_quoted());
    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].stable_key, "phrase");
}

#[test]
fn filters_are_applied_before_the_candidate_bound() {
    let policy = SearchPolicy {
        max_candidates: 1,
        ..SearchPolicy::default()
    };
    let outcome = policy
        .prepare(SearchRequest::new("in:Assets/Target button", 1))
        .execute([
            candidate(
                "wrong-path",
                "Button",
                "Packages/UI/Button.prefab",
                "Prefab",
                100,
            ),
            candidate(
                "right-path",
                "Button",
                "Assets/Target/Button.prefab",
                "Prefab",
                1,
            ),
        ]);

    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].stable_key, "right-path");
}

#[test]
fn zero_limit_does_not_poll_the_candidate_iterator() {
    let consumed = Cell::new(0usize);
    let candidates = (0..3).map(|index| {
        consumed.set(consumed.get() + 1);
        candidate(
            &format!("candidate-{index}"),
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            1,
        )
    });

    let outcome = SearchPolicy::default()
        .prepare(SearchRequest::new("button", 0))
        .execute(candidates);

    assert_eq!(consumed.get(), 0);
    assert!(outcome.matches.is_empty());
    assert!(!outcome.fallback_used);
    assert_eq!(outcome.match_count.value, 0);
    assert_eq!(outcome.match_count.relation, MatchCountRelation::LowerBound);
}

#[test]
fn wholly_quoted_filter_syntax_is_a_literal_term() {
    let type_literal = execute(
        r#""type:Prefab""#,
        vec![candidate(
            "type-literal",
            "type:Prefab",
            "Assets/type-prefab.txt",
            "File",
            1,
        )],
    );
    assert_eq!(type_literal.query.type_filter(), None);
    assert_eq!(type_literal.matches[0].stable_key, "type-literal");

    let path_literal = execute(
        r#""in:Assets/UI""#,
        vec![candidate(
            "path-literal",
            "in:Assets/UI",
            "Packages/path-literal.txt",
            "File",
            1,
        )],
    );
    assert_eq!(path_literal.query.path_prefix(), None);
    assert_eq!(path_literal.matches[0].stable_key, "path-literal");
}

#[test]
fn fuzzy_is_a_low_confidence_fallback_not_an_always_on_matcher() {
    let strict = execute(
        "button",
        vec![
            candidate("exact", "Button", "Assets/UI/Button.prefab", "Prefab", 1),
            candidate(
                "would-be-fuzzy",
                "Buton",
                "Assets/UI/Buton.prefab",
                "Prefab",
                100,
            ),
        ],
    );

    assert!(!strict.fallback_used);
    assert_eq!(strict.matches.len(), 1);
    assert_eq!(strict.matches[0].stable_key, "exact");

    let fallback = execute(
        "button",
        vec![candidate(
            "fuzzy",
            "Buton",
            "Assets/UI/Buton.prefab",
            "Prefab",
            1,
        )],
    );
    assert!(fallback.fallback_used);
    assert_eq!(fallback.matches[0].match_kind, MatchKind::Fuzzy);
}

#[test]
fn fuzzy_candidate_retrieval_is_only_enabled_after_core_requests_fallback() {
    let prepared = SearchPolicy::default().prepare(SearchRequest::new("button", 20));
    assert!(
        prepared
            .retrieval_terms(RetrievalStage::Strict)
            .iter()
            .all(|term| term.fuzzy_distance.is_none())
    );
    assert!(
        prepared
            .retrieval_terms(RetrievalStage::FuzzyFallback)
            .iter()
            .all(|term| term.fuzzy_distance == Some(1))
    );

    let exact = [candidate(
        "exact",
        "Button",
        "Assets/UI/Button.prefab",
        "Prefab",
        1,
    )];
    let exact_fallback_called = Cell::new(false);
    let exact_outcome = prepared
        .execute_with_fallback(exact, |_| {
            exact_fallback_called.set(true);
            Ok::<_, ()>(Vec::new())
        })
        .unwrap();
    assert!(!exact_fallback_called.get());
    assert!(!exact_outcome.fallback_used);

    let typo = [candidate(
        "typo",
        "Buton",
        "Assets/UI/Buton.prefab",
        "Prefab",
        1,
    )];
    let typo_fallback_called = Cell::new(false);
    let typo_outcome = prepared
        .execute_with_fallback(typo, |_| {
            typo_fallback_called.set(true);
            Ok::<_, ()>([candidate(
                "fallback",
                "Button",
                "Assets/UI/Button.prefab",
                "Prefab",
                1,
            )])
        })
        .unwrap();
    assert!(typo_fallback_called.get());
    assert!(typo_outcome.fallback_used);

    let mapped = SearchPolicy::default()
        .prepare(SearchRequest::new(r#""SpawnEnemy" icon"#, 20))
        .retrieval_terms(RetrievalStage::FuzzyFallback);
    assert_eq!(
        mapped
            .iter()
            .map(|term| term.term_index)
            .collect::<Vec<_>>(),
        [0, 0, 1]
    );
    assert!(mapped[..2].iter().all(|term| term.fuzzy_distance.is_none()));
    assert_eq!(mapped[2].fuzzy_distance, Some(1));
}

#[test]
fn fuzzy_confidence_count_only_includes_matches_meeting_the_kind_threshold() {
    let policy = SearchPolicy {
        fuzzy_fallback: FuzzyFallbackPolicy {
            minimum_confident_matches: 2,
            minimum_confident_kind: MatchKind::Prefix,
            ..FuzzyFallbackPolicy::default()
        },
        ..SearchPolicy::default()
    };
    let prepared = policy.prepare(SearchRequest::new("button", 20));
    let candidates = [
        candidate("exact", "Button", "Assets/UI/Button.prefab", "Prefab", 1),
        candidate(
            "substring",
            "PrimaryButtonIcon",
            "Assets/UI/PrimaryButtonIcon.prefab",
            "Prefab",
            1,
        ),
    ];

    let fallback_called = Cell::new(false);
    let outcome = prepared
        .execute_with_fallback(candidates, |_| {
            fallback_called.set(true);
            Ok::<_, ()>(Vec::new())
        })
        .unwrap();
    assert!(fallback_called.get());
    assert!(outcome.fallback_used);
}

#[test]
fn fallback_receives_only_bounded_strict_match_keys() {
    let policy = SearchPolicy {
        max_candidates: 2,
        fuzzy_fallback: FuzzyFallbackPolicy {
            minimum_confident_matches: 2,
            ..FuzzyFallbackPolicy::default()
        },
        ..SearchPolicy::default()
    };
    let prepared = policy.prepare(SearchRequest::new("button", 10));

    let outcome = prepared
        .execute_with_fallback(
            [
                candidate("exact", "Button", "Assets/UI/Button.prefab", "Prefab", 100),
                candidate("retrieved-only", "Other", "Assets/Other.asset", "Asset", 90),
                candidate(
                    "outside-bound",
                    "Button",
                    "Assets/UI/OtherButton.prefab",
                    "Prefab",
                    1,
                ),
            ],
            |strict_match_keys| {
                assert_eq!(
                    strict_match_keys
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    ["exact"]
                );
                Ok::<_, ()>(Vec::new())
            },
        )
        .unwrap();

    assert!(outcome.fallback_used);
}

#[test]
fn rejected_strict_candidates_cannot_suppress_fallback() {
    let prepared = SearchPolicy::default().prepare(SearchRequest::new("button", 20));
    let invalid =
        candidate("invalid", "Button", "Assets/UI/Button.prefab", "Prefab", 10).with_evidence([
            RetrievalEvidence::new(usize::MAX, MatchField::Content, MatchKind::Token),
        ]);
    let fallback_called = Cell::new(false);

    let outcome = prepared
        .execute_with_fallback([invalid], |_| {
            fallback_called.set(true);
            Ok::<_, ()>([candidate(
                "fallback",
                "Button",
                "Assets/UI/Fallback.prefab",
                "Prefab",
                1,
            )])
        })
        .unwrap();

    assert!(fallback_called.get());
    assert!(outcome.fallback_used);
    assert_eq!(outcome.matches[0].stable_key, "fallback");
    assert!(outcome.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic,
        SearchDiagnostic::InvalidRetrievalEvidence {
            term_index: usize::MAX
        }
    )));
}

#[test]
fn typed_retrieval_evidence_preserves_content_only_matches() {
    let content_only = candidate(
        "script",
        "EnemySpawner.cs",
        "Assets/Scripts/EnemySpawner.cs",
        "Script",
        10,
    )
    .with_evidence([RetrievalEvidence::new(
        0,
        MatchField::Content,
        MatchKind::Token,
    )]);

    let outcome = execute("SpawnEnemy", vec![content_only]);

    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].stable_key, "script");
    assert_eq!(outcome.matches[0].match_kind, MatchKind::Token);
    assert_eq!(
        outcome.matches[0].explanation.terms[0].field,
        MatchField::Content
    );

    assert!(
        RetrievalEvidence::new(0, MatchField::Content, MatchKind::Prefix).is_better_than(
            RetrievalEvidence::new(0, MatchField::Content, MatchKind::Token,)
        )
    );
}

#[test]
fn kind_matches_are_classified_by_core_without_adapter_evidence() {
    let outcome = execute(
        "bund",
        vec![candidate(
            "bundle-kind",
            "Other",
            "Assets/Other.asset",
            "BundleContainer",
            1,
        )],
    );

    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].match_kind, MatchKind::Prefix);
    assert_eq!(
        outcome.matches[0].explanation.terms[0].field,
        MatchField::Kind
    );
}

#[test]
fn retrieval_scores_are_only_compared_within_the_same_stage() {
    let strict = candidate("strict", "Buton", "Assets/Strict.prefab", "Prefab", 1);
    let fallback = candidate(
        "fallback",
        "Buton",
        "Assets/Fallback.prefab",
        "Prefab",
        i64::MAX,
    );

    let outcome = SearchPolicy::default()
        .prepare(SearchRequest::new("button", 20))
        .execute_with_fallback([strict], |_| Ok::<_, ()>([fallback]))
        .unwrap();

    assert!(outcome.fallback_used);
    assert_eq!(outcome.matches[0].stable_key, "strict");
    assert_eq!(
        outcome.matches[0].ranking_signals.retrieval_stage,
        RetrievalStage::Strict
    );
}

#[test]
fn fuzzy_retrieval_evidence_cannot_bypass_short_term_policy() {
    let candidate = candidate(
        "invalid-short-fuzzy",
        "LongNeedle",
        "Assets/LongNeedle.asset",
        "Asset",
        10,
    )
    .with_evidence([RetrievalEvidence::new(
        0,
        MatchField::Content,
        MatchKind::Fuzzy,
    )]);

    let outcome = execute("q longneedle", vec![candidate]);

    assert!(outcome.fallback_used);
    assert!(outcome.matches.is_empty());
    assert!(
        outcome
            .diagnostics
            .contains(&SearchDiagnostic::InvalidRetrievalEvidence { term_index: 0 })
    );
}

#[test]
fn query_and_candidate_fields_are_rejected_before_normalization() {
    let limits = SearchLimits {
        max_query_bytes: 4,
        max_name_bytes: 4,
        ..SearchLimits::default()
    };
    let policy = SearchPolicy {
        limits,
        ..SearchPolicy::default()
    };

    let oversized_query = policy
        .prepare(SearchRequest::new("button", 1))
        .execute(std::iter::empty());
    assert!(oversized_query.matches.is_empty());
    assert!(
        oversized_query
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                SearchDiagnostic::QueryByteLimitExceeded {
                    actual: 6,
                    limit: 4
                }
            ))
    );

    let oversized_field = policy
        .prepare(SearchRequest::new("btn", 1))
        .execute([candidate("field", "Button", "btn", "File", 1)]);
    assert!(oversized_field.matches.is_empty());
    assert!(
        oversized_field
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(
                diagnostic,
                SearchDiagnostic::CandidateFieldByteLimitExceeded {
                    field: CandidateField::Name,
                    actual: 6,
                    limit: 4,
                }
            ))
    );
}

#[test]
fn candidate_input_and_total_bytes_are_hard_bounded() {
    let consumed = Cell::new(0usize);
    let limits = SearchLimits {
        max_candidate_inputs: 2,
        max_total_candidate_bytes: 20,
        ..SearchLimits::default()
    };
    let policy = SearchPolicy {
        limits,
        ..SearchPolicy::default()
    };
    let candidates = (0..10).map(|index| {
        consumed.set(consumed.get() + 1);
        candidate(
            &format!("id-{index}"),
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            1,
        )
    });

    let outcome = policy
        .prepare(SearchRequest::new("button", 10))
        .execute(candidates);

    assert_eq!(consumed.get(), 1);
    assert!(outcome.matches.is_empty());
    assert!(outcome.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic,
        SearchDiagnostic::CandidateTotalByteLimitExceeded { limit: 20, .. }
    )));
}

#[test]
fn fuzzy_work_budget_bounds_adversarial_candidates_without_hiding_strict_matches() {
    let work_limit = 10_000;
    let policy = SearchPolicy {
        fuzzy_fallback: FuzzyFallbackPolicy {
            minimum_confident_matches: 2,
            ..FuzzyFallbackPolicy::default()
        },
        limits: SearchLimits {
            max_fuzzy_work_units: work_limit,
            ..SearchLimits::default()
        },
        ..SearchPolicy::default()
    };
    let mut candidates: Vec<_> = (0..10)
        .map(|index| {
            candidate(
                &format!("noise-{index}"),
                &"x".repeat(512),
                &format!("Assets/Noise/{}/{}.asset", "x".repeat(480), index),
                "File",
                100 - index,
            )
        })
        .collect();
    candidates.push(candidate(
        "exact",
        "abcdefgh",
        "Assets/Exact.asset",
        "Asset",
        0,
    ));

    let outcome = policy
        .prepare(SearchRequest::new("abcdefgh", 20))
        .execute(candidates);

    assert!(outcome.fallback_used);
    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].stable_key, "exact");
    assert_eq!(outcome.matches[0].match_kind, MatchKind::Exact);
    assert!(outcome.fuzzy_work.consumed <= work_limit);
    assert_eq!(outcome.fuzzy_work.limit, work_limit);
    assert!(outcome.fuzzy_work.exhausted);
    assert_eq!(outcome.match_count.relation, MatchCountRelation::LowerBound);
    assert!(outcome.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic,
        SearchDiagnostic::FuzzyWorkLimitExceeded {
            attempted,
            limit,
        } if *attempted > work_limit && *limit == work_limit
    )));
}

#[test]
fn unknown_length_candidate_iterators_report_input_truncation() {
    let limits = SearchLimits {
        max_candidate_inputs: 2,
        ..SearchLimits::default()
    };
    let policy = SearchPolicy {
        limits,
        ..SearchPolicy::default()
    };
    let mut index = 0usize;
    let candidates = std::iter::from_fn(move || {
        let current = index;
        index += 1;
        (current < 3).then(|| {
            candidate(
                &format!("candidate-{current}"),
                "Button",
                &format!("Assets/UI/Button{current}.prefab"),
                "Prefab",
                1,
            )
        })
    });

    let outcome = policy
        .prepare(SearchRequest::new("button", 10))
        .execute(candidates);

    assert!(
        outcome
            .diagnostics
            .contains(&SearchDiagnostic::CandidateInputLimitExceeded { limit: 2 })
    );
}

#[test]
fn field_boosts_and_stable_secondary_keys_break_score_ties() {
    let outcome = execute(
        "button",
        vec![
            candidate("path-match", "Other", "button", "Prefab", 100),
            candidate("name-b", "Button", "Assets/UI/B.prefab", "Prefab", 5),
            candidate("name-a", "Button", "Assets/UI/A.prefab", "Prefab", 5),
        ],
    );

    let keys: Vec<_> = outcome
        .matches
        .iter()
        .map(|ranked| ranked.stable_key.as_str())
        .collect();
    assert_eq!(keys, ["name-a", "name-b", "path-match"]);
    assert_eq!(
        outcome.matches[0].explanation.terms[0].field,
        MatchField::Name
    );
    assert_eq!(
        outcome.matches[2].explanation.terms[0].field,
        MatchField::Path
    );
}

#[test]
fn ranking_is_independent_of_candidate_insertion_order() {
    let candidates = vec![
        candidate("c", "Button", "Assets/UI/C.prefab", "Prefab", 5),
        candidate("a", "Button", "Assets/UI/A.prefab", "Prefab", 5),
        candidate("b", "Button", "Assets/UI/B.prefab", "Prefab", 5),
    ];
    let mut reversed = candidates.clone();
    reversed.reverse();

    let forward = execute("button", candidates);
    let backward = execute("button", reversed);

    assert_eq!(forward.matches, backward.matches);
    assert_eq!(
        serde_json::to_vec(&forward.matches).unwrap(),
        serde_json::to_vec(&backward.matches).unwrap()
    );
}

#[test]
fn complete_match_kind_order_is_stable() {
    let policy = SearchPolicy {
        fuzzy_fallback: FuzzyFallbackPolicy {
            minimum_confident_matches: 10,
            minimum_query_chars: 2,
            ..FuzzyFallbackPolicy::default()
        },
        ..SearchPolicy::default()
    };
    let outcome = policy
        .prepare(SearchRequest::new("mm", 10))
        .execute_with_fallback(
            [
                candidate("substring", "ammb", "Assets/Substring.asset", "Asset", 1),
                candidate("token", "primary mm icon", "Assets/Token.asset", "Asset", 1),
                candidate("prefix", "mmenu", "Assets/Prefix.asset", "Asset", 1),
                candidate("exact", "mm", "Assets/Exact.asset", "Asset", 1),
                candidate(
                    "abbreviation",
                    "MainMenu",
                    "Assets/Abbreviation.asset",
                    "Asset",
                    1,
                ),
            ],
            |_| Ok::<_, ()>([candidate("fuzzy", "mn", "Assets/Fuzzy.asset", "Asset", 1)]),
        )
        .unwrap();

    assert!(outcome.fallback_used);
    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|ranked| (ranked.stable_key.as_str(), ranked.match_kind))
            .collect::<Vec<_>>(),
        [
            ("exact", MatchKind::Exact),
            ("prefix", MatchKind::Prefix),
            ("token", MatchKind::Token),
            ("substring", MatchKind::Substring),
            ("abbreviation", MatchKind::Abbreviation),
            ("fuzzy", MatchKind::Fuzzy),
        ]
    );
}

#[test]
fn candidate_expansion_is_bounded_and_reports_truncation() {
    let policy = SearchPolicy {
        max_candidates: 2,
        ..SearchPolicy::default()
    };
    let outcome = policy
        .prepare(SearchRequest::new("button", 20))
        .execute(vec![
            candidate("low", "Button", "Assets/Low.prefab", "Prefab", 1),
            candidate("high", "Button", "Assets/High.prefab", "Prefab", 3),
            candidate("mid", "Button", "Assets/Mid.prefab", "Prefab", 2),
        ]);

    assert_eq!(outcome.candidates_provided, 3);
    assert_eq!(outcome.candidates_considered, 2);
    assert!(
        outcome
            .diagnostics
            .contains(&SearchDiagnostic::CandidateLimitExceeded {
                stage: RetrievalStage::Strict,
                provided: 3,
                limit: 2,
            })
    );
    let keys: Vec<_> = outcome
        .matches
        .iter()
        .map(|ranked| ranked.stable_key.as_str())
        .collect();
    assert_eq!(keys, ["high", "mid"]);
    assert_eq!(outcome.match_count.value, 2);
    assert_eq!(outcome.match_count.relation, MatchCountRelation::LowerBound);
}

#[test]
fn match_count_is_exact_before_request_limit_truncation() {
    let outcome = SearchPolicy::default()
        .prepare(SearchRequest::new("button", 1))
        .execute([
            candidate("a", "Button A", "Assets/A.prefab", "Prefab", 3),
            candidate("b", "Button B", "Assets/B.prefab", "Prefab", 2),
            candidate("c", "Button C", "Assets/C.prefab", "Prefab", 1),
        ]);

    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.matches[0].rank, 1);
    assert_eq!(outcome.match_count.value, 3);
    assert_eq!(outcome.match_count.relation, MatchCountRelation::Exact);
}

#[test]
fn unicode_normalization_produces_original_byte_highlights() {
    let outcome = execute(
        "ui 按钮",
        vec![candidate(
            "unicode",
            "ＵＩ按钮",
            "Assets/界面/ＵＩ按钮.prefab",
            "Prefab",
            1,
        )],
    );

    let ranked = &outcome.matches[0];
    assert_eq!(ranked.highlight_name_ranges.len(), 2);
    for range in &ranked.highlight_name_ranges {
        assert!("ＵＩ按钮".is_char_boundary(range.start));
        assert!("ＵＩ按钮".is_char_boundary(range.end));
    }
    assert_eq!(
        ranked.highlight_name.as_deref(),
        Some("<em>ＵＩ</em><em>按钮</em>")
    );
}

#[test]
fn highlight_html_escapes_untrusted_asset_text() {
    assert_eq!(
        highlight_html(
            r#"Assets/<script data-x='1'>&Button".prefab"#,
            &["button".to_string()],
        )
        .as_deref(),
        Some(r#"Assets/&lt;script data-x=&#39;1&#39;&gt;&amp;<em>Button</em>&quot;.prefab"#)
    );
}

#[test]
fn canonically_reordered_combining_marks_keep_original_byte_highlights() {
    let text = "a\u{0315}\u{0300}";
    let query = "\u{00e0}\u{0315}";
    let outcome = execute(query, vec![candidate("combining", text, text, "File", 1)]);

    assert_eq!(outcome.matches[0].match_kind, MatchKind::Exact);
    assert_eq!(
        outcome.matches[0].highlight_name_ranges,
        [unity_asset_search_core::HighlightRange {
            start: 0,
            end: text.len(),
        }]
    );
    assert_eq!(
        outcome.matches[0].highlight_name.as_deref(),
        Some("<em>a\u{0315}\u{0300}</em>")
    );
}

#[test]
fn empty_and_invalid_queries_return_structured_diagnostics() {
    let empty = execute("   ", Vec::new());
    assert!(empty.matches.is_empty());
    assert!(empty.diagnostics.contains(&SearchDiagnostic::EmptyQuery));

    let unterminated = execute(
        r#""button"#,
        vec![candidate(
            "button",
            "Button",
            "Assets/UI/Button.prefab",
            "Prefab",
            1,
        )],
    );
    assert!(unterminated.matches.is_empty());
    assert!(
        unterminated
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic, SearchDiagnostic::UnterminatedQuote { .. }))
    );

    let invalid_type = execute("type:not-a-unity-kind button", Vec::new());
    assert!(invalid_type.matches.is_empty());
    assert!(invalid_type.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic,
        SearchDiagnostic::UnsupportedTypeFilter { value }
            if value == "not-a-unity-kind"
    )));
}

#[test]
fn query_and_candidate_contract_violations_have_typed_diagnostics() {
    let malformed = execute(r#""" type: type:prefab type:scene button"#, Vec::new());
    assert!(
        malformed
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic, SearchDiagnostic::EmptyQuotedTerm { .. }))
    );
    assert!(
        malformed
            .diagnostics
            .contains(&SearchDiagnostic::MissingFilterValue {
                field: "type".to_string(),
            })
    );
    assert!(
        malformed
            .diagnostics
            .contains(&SearchDiagnostic::DuplicateFilter {
                field: "type".to_string(),
            })
    );

    let policy = SearchPolicy {
        limits: SearchLimits {
            max_evidence_items: 0,
            ..SearchLimits::default()
        },
        ..SearchPolicy::default()
    };
    let evidence_limited = policy
        .prepare(SearchRequest::new("button", 10))
        .execute([
            candidate("evidence", "Other", "Assets/Other.asset", "Asset", 1).with_evidence([
                RetrievalEvidence::new(0, MatchField::Content, MatchKind::Token),
            ]),
        ]);
    assert!(evidence_limited.diagnostics.contains(
        &SearchDiagnostic::CandidateEvidenceLimitExceeded {
            actual: 1,
            limit: 0,
        }
    ));

    let duplicate = execute(
        "button",
        vec![
            candidate("same", "Button", "Assets/A.prefab", "Prefab", 2),
            candidate("same", "Button", "Assets/B.prefab", "Prefab", 1),
        ],
    );
    assert_eq!(duplicate.matches.len(), 1);
    assert!(
        duplicate
            .diagnostics
            .contains(&SearchDiagnostic::DuplicateCandidateKey {
                stable_key: "same".to_string(),
            })
    );
}

#[test]
fn duplicate_keys_are_merged_before_candidate_truncation() {
    let duplicate_without_evidence = candidate("same", "Other", "Assets/Other.asset", "Asset", 100);
    let duplicate_with_evidence =
        candidate("same", "Other", "Assets/Other.asset", "Asset", 99).with_evidence([
            RetrievalEvidence::new(0, MatchField::Content, MatchKind::Token),
        ]);
    let unique = candidate("unique", "Needle", "Assets/Needle.asset", "Asset", 50);
    let policy = SearchPolicy {
        max_candidates: 2,
        ..SearchPolicy::default()
    };
    let run = |candidates| {
        policy
            .prepare(SearchRequest::new("needle", 10))
            .execute(candidates)
    };

    let forward = run(vec![
        duplicate_without_evidence.clone(),
        duplicate_with_evidence.clone(),
        unique.clone(),
    ]);
    let reverse = run(vec![
        unique,
        duplicate_with_evidence,
        duplicate_without_evidence,
    ]);

    assert_eq!(forward.matches.len(), 2);
    assert_eq!(forward.candidates_eligible, 2);
    assert_eq!(forward.match_count.relation, MatchCountRelation::Exact);
    assert_eq!(
        serde_json::to_vec(&forward.matches).unwrap(),
        serde_json::to_vec(&reverse.matches).unwrap()
    );
    assert!(
        forward
            .diagnostics
            .contains(&SearchDiagnostic::DuplicateCandidateKey {
                stable_key: "same".to_string(),
            })
    );
}

#[test]
fn duplicate_keys_cannot_consume_the_candidate_boundary() {
    let policy = SearchPolicy {
        max_candidates: 2,
        ..SearchPolicy::default()
    };
    let outcome = policy.prepare(SearchRequest::new("button", 10)).execute([
        candidate("same", "Button", "Assets/A.prefab", "Prefab", 100),
        candidate("same", "Button", "Assets/B.prefab", "Prefab", 99),
        candidate("unique", "Button", "Assets/C.prefab", "Prefab", 1),
    ]);

    assert_eq!(
        outcome
            .matches
            .iter()
            .map(|ranked| ranked.stable_key.as_str())
            .collect::<Vec<_>>(),
        ["same", "unique"]
    );
    assert!(
        outcome
            .diagnostics
            .contains(&SearchDiagnostic::DuplicateCandidateKey {
                stable_key: "same".to_string(),
            })
    );
}

#[test]
fn every_known_diagnostic_has_an_independent_golden_wire_contract() {
    let cases = vec![
        (
            SearchDiagnostic::EmptyQuery,
            serde_json::json!({
                "contract_version": 1,
                "code": "empty_query",
                "severity": "error",
                "blocks_execution": true,
                "details": {},
            }),
        ),
        (
            SearchDiagnostic::UnterminatedQuote { byte_offset: 7 },
            serde_json::json!({
                "contract_version": 1,
                "code": "unterminated_quote",
                "severity": "error",
                "blocks_execution": true,
                "details": { "byte_offset": 7 },
            }),
        ),
        (
            SearchDiagnostic::EmptyQuotedTerm { byte_offset: 11 },
            serde_json::json!({
                "contract_version": 1,
                "code": "empty_quoted_term",
                "severity": "error",
                "blocks_execution": true,
                "details": { "byte_offset": 11 },
            }),
        ),
        (
            SearchDiagnostic::MissingFilterValue {
                field: "type".to_string(),
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "missing_filter_value",
                "severity": "error",
                "blocks_execution": true,
                "details": { "field": "type" },
            }),
        ),
        (
            SearchDiagnostic::DuplicateFilter {
                field: "in".to_string(),
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "duplicate_filter",
                "severity": "error",
                "blocks_execution": true,
                "details": { "field": "in" },
            }),
        ),
        (
            SearchDiagnostic::UnsupportedTypeFilter {
                value: "FutureKind".to_string(),
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "unsupported_type_filter",
                "severity": "error",
                "blocks_execution": true,
                "details": { "value": "FutureKind" },
            }),
        ),
        (
            SearchDiagnostic::CandidateLimitExceeded {
                stage: RetrievalStage::Strict,
                provided: 6,
                limit: 5,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "candidate_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "stage": "strict", "provided": 6, "limit": 5 },
            }),
        ),
        (
            SearchDiagnostic::QueryByteLimitExceeded {
                actual: 4_097,
                limit: 4_096,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "query_byte_limit_exceeded",
                "severity": "error",
                "blocks_execution": true,
                "details": { "actual": 4097, "limit": 4096 },
            }),
        ),
        (
            SearchDiagnostic::QueryTermLimitExceeded {
                actual: 129,
                limit: 128,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "query_term_limit_exceeded",
                "severity": "error",
                "blocks_execution": true,
                "details": { "actual": 129, "limit": 128 },
            }),
        ),
        (
            SearchDiagnostic::RetrievalTermLimitExceeded {
                actual: 257,
                limit: 256,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "retrieval_term_limit_exceeded",
                "severity": "error",
                "blocks_execution": true,
                "details": { "actual": 257, "limit": 256 },
            }),
        ),
        (
            SearchDiagnostic::CandidateFieldByteLimitExceeded {
                field: CandidateField::ContainerSourcePath,
                actual: 33,
                limit: 32,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "candidate_field_byte_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": {
                    "field": "container_source_path",
                    "actual": 33,
                    "limit": 32,
                },
            }),
        ),
        (
            SearchDiagnostic::CandidateTotalByteLimitExceeded {
                consumed: 1_025,
                limit: 1_024,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "candidate_total_byte_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "consumed": 1025, "limit": 1024 },
            }),
        ),
        (
            SearchDiagnostic::CandidateInputLimitExceeded { limit: 4_096 },
            serde_json::json!({
                "contract_version": 1,
                "code": "candidate_input_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "limit": 4096 },
            }),
        ),
        (
            SearchDiagnostic::CandidateEvidenceLimitExceeded {
                actual: 257,
                limit: 256,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "candidate_evidence_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "actual": 257, "limit": 256 },
            }),
        ),
        (
            SearchDiagnostic::FuzzyWorkLimitExceeded {
                attempted: 10_240,
                limit: 10_000,
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "fuzzy_work_limit_exceeded",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "attempted": 10240, "limit": 10000 },
            }),
        ),
        (
            SearchDiagnostic::InvalidRetrievalEvidence { term_index: 3 },
            serde_json::json!({
                "contract_version": 1,
                "code": "invalid_retrieval_evidence",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "term_index": 3 },
            }),
        ),
        (
            SearchDiagnostic::DuplicateCandidateKey {
                stable_key: "candidate-v1:abc".to_string(),
            },
            serde_json::json!({
                "contract_version": 1,
                "code": "duplicate_candidate_key",
                "severity": "warning",
                "blocks_execution": false,
                "details": { "stable_key": "candidate-v1:abc" },
            }),
        ),
    ];

    for (diagnostic, expected_wire) in cases {
        let actual_wire = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(
            actual_wire,
            expected_wire,
            "wire drift for diagnostic {}",
            diagnostic.code()
        );
        assert_eq!(
            serde_json::from_value::<SearchDiagnostic>(actual_wire).unwrap(),
            diagnostic
        );
    }
}

#[test]
fn diagnostic_wire_contract_is_versioned_and_unknown_codes_fail_closed() {
    let diagnostic = SearchDiagnostic::CandidateLimitExceeded {
        stage: RetrievalStage::Strict,
        provided: 6,
        limit: 5,
    };
    let wire = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({
            "contract_version": 1,
            "code": "candidate_limit_exceeded",
            "severity": "warning",
            "blocks_execution": false,
            "details": {
                "stage": "strict",
                "provided": 6,
                "limit": 5,
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<SearchDiagnostic>(wire).unwrap(),
        diagnostic
    );

    let exhausted = SearchDiagnostic::FuzzyWorkLimitExceeded {
        attempted: 10_240,
        limit: 10_000,
    };
    let exhausted_wire = serde_json::to_value(&exhausted).unwrap();
    assert_eq!(exhausted_wire["code"], "fuzzy_work_limit_exceeded");
    assert_eq!(exhausted_wire["severity"], "warning");
    assert_eq!(exhausted_wire["blocks_execution"], false);
    assert_eq!(exhausted_wire["details"]["attempted"], 10_240);
    assert_eq!(
        serde_json::from_value::<SearchDiagnostic>(exhausted_wire).unwrap(),
        exhausted
    );

    let unknown = serde_json::json!({
        "contract_version": 2,
        "code": "future_policy_signal",
        "severity": "warning",
        "blocks_execution": false,
        "details": { "future": true },
    });
    let decoded = serde_json::from_value::<SearchDiagnostic>(unknown).unwrap();
    assert_eq!(decoded.code(), "future_policy_signal");
    assert!(!decoded.blocks_execution());
    assert_eq!(
        decoded.severity(),
        unity_asset_search_core::SearchDiagnosticSeverity::Warning
    );

    let inconsistent = serde_json::json!({
        "contract_version": 1,
        "code": "empty_query",
        "severity": "warning",
        "blocks_execution": false,
        "details": {},
    });
    assert!(serde_json::from_value::<SearchDiagnostic>(inconsistent).is_err());
}

#[test]
fn query_complexity_limits_block_retrieval_before_polling_candidates() {
    let consumed = Cell::new(0usize);
    let candidates = std::iter::from_fn(|| {
        consumed.set(consumed.get() + 1);
        Some(candidate(
            "never",
            "One Two Three",
            "Assets/Never.asset",
            "Asset",
            1,
        ))
    });
    let policy = SearchPolicy {
        limits: SearchLimits {
            max_query_terms: 2,
            ..SearchLimits::default()
        },
        ..SearchPolicy::default()
    };
    let outcome = policy
        .prepare(SearchRequest::new("one two three", 10))
        .execute(candidates);

    assert_eq!(consumed.get(), 0);
    assert!(outcome.matches.is_empty());
    assert!(
        outcome
            .diagnostics
            .contains(&SearchDiagnostic::QueryTermLimitExceeded {
                actual: 3,
                limit: 2,
            })
    );

    let normalized = SearchPolicy {
        limits: SearchLimits {
            max_retrieval_terms: 2,
            ..SearchLimits::default()
        },
        ..SearchPolicy::default()
    }
    .prepare(SearchRequest::new("oneTwoThree", 10))
    .execute(Vec::new());
    assert!(
        normalized
            .diagnostics
            .contains(&SearchDiagnostic::RetrievalTermLimitExceeded {
                actual: 3,
                limit: 2,
            })
    );
}
