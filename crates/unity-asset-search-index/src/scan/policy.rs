use std::collections::TryReserveError;
use std::fmt;

use regex_automata::dfa::{Automaton as _, OverlappingState, StartKind, dense};
use regex_automata::nfa::thompson;
use regex_automata::util::syntax;
use regex_automata::{Anchored, Input, MatchKind};
use unity_asset_core::{AssetLoadBudget, BudgetError};

use crate::SearchIgnoreV1Limits;

pub(super) use crate::SEARCH_IGNORE_V1_FILE_NAME as SEARCH_IGNORE_V1_FILE;
const PARSER_PASSES: u64 = 2;
const MAX_COMPILED_PATTERNS_PER_RULE: usize = 2;
const REGEX_CAPACITY_MULTIPLIER: usize = 16;
const REGEX_CAPACITY_BASE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyDecision {
    Include,
    ExcludeButDescend,
    ExcludeAndPrune,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PolicyMatchBudget {
    observed: u64,
    limit: u64,
}

impl PolicyMatchBudget {
    pub(super) const fn new(limit: u64) -> Self {
        Self { observed: 0, limit }
    }

    fn observe_match(&mut self) -> Result<(), PolicyError> {
        let observed_at_least = self.observed.saturating_add(1);
        if observed_at_least > self.limit {
            return Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::MatchWork,
                observed_at_least,
                limit: self.limit,
            });
        }
        self.observed = observed_at_least;
        Ok(())
    }

    #[cfg(test)]
    const fn usage(self) -> u64 {
        self.observed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleAction {
    Include,
    Exclude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleScope {
    Exact,
    Subtree,
    Glob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternRole {
    Primary,
    MayDescend,
}

#[derive(Debug, Clone, Copy)]
struct PatternMeta {
    ordinal: u32,
    action: RuleAction,
    scope: RuleScope,
    role: PatternRole,
}

#[derive(Debug)]
pub(super) struct SearchIgnoreV1 {
    automaton: Option<dense::DFA<Vec<u32>>>,
    patterns: Vec<PatternMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyLimitResource {
    FileBytes,
    LineBytes,
    Rules,
    ParserWork,
    AutomatonBytes,
    CompilationBytes,
    MatchWork,
}

impl fmt::Display for PolicyLimitResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FileBytes => "file bytes",
            Self::LineBytes => "line bytes",
            Self::Rules => "rules",
            Self::ParserWork => "parser work",
            Self::AutomatonBytes => "automaton bytes",
            Self::CompilationBytes => "compilation bytes",
            Self::MatchWork => "match work",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicySyntaxReason {
    InvalidUtf8,
    UnknownDirective,
    EmptyPath,
    AbsolutePath,
    EmptySegment,
    DotSegment,
    ControlCharacter,
    WildcardInLiteralRule,
    GlobWithoutWildcard,
    UnsupportedGlobSyntax,
    DoubleStarMustBeCompleteSegment,
}

impl fmt::Display for PolicySyntaxReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUtf8 => "rule is not valid UTF-8",
            Self::UnknownDirective => "expected exact:, subtree:, or glob:",
            Self::EmptyPath => "rule path is empty",
            Self::AbsolutePath => "rule path must be project-root relative",
            Self::EmptySegment => "rule path contains an empty segment",
            Self::DotSegment => "rule path contains . or ..",
            Self::ControlCharacter => "rule path contains a control character",
            Self::WildcardInLiteralRule => "exact and subtree rules cannot contain wildcards",
            Self::GlobWithoutWildcard => "glob rule must contain at least one wildcard",
            Self::UnsupportedGlobSyntax => "glob uses syntax outside SearchIgnoreV1",
            Self::DoubleStarMustBeCompleteSegment => {
                "** is allowed only as a complete path segment"
            }
        })
    }
}

#[derive(Debug)]
pub(crate) enum PolicyError {
    Budget(BudgetError),
    Allocation {
        allocation: &'static str,
        requested: usize,
        source: TryReserveError,
    },
    LimitExceeded {
        resource: PolicyLimitResource,
        observed_at_least: u64,
        limit: u64,
    },
    Syntax {
        line: u64,
        reason: PolicySyntaxReason,
    },
    CompilationFailed,
    MatchFailed,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => error.fmt(formatter),
            Self::Allocation {
                allocation,
                requested,
                source,
            } => write!(
                formatter,
                "failed to reserve {requested} bytes for {allocation}: {source}"
            ),
            Self::LimitExceeded {
                resource,
                observed_at_least,
                limit,
            } => write!(
                formatter,
                "SearchIgnoreV1 {resource} limit exceeded: observed at least \
                 {observed_at_least}, limit {limit}"
            ),
            Self::Syntax { line, reason } => {
                write!(
                    formatter,
                    "invalid SearchIgnoreV1 rule at line {line}: {reason}"
                )
            }
            Self::CompilationFailed => {
                formatter.write_str("failed to compile the bounded SearchIgnoreV1 automaton")
            }
            Self::MatchFailed => formatter.write_str("SearchIgnoreV1 automaton match failed"),
        }
    }
}

impl std::error::Error for PolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::LimitExceeded { .. }
            | Self::Syntax { .. }
            | Self::CompilationFailed
            | Self::MatchFailed => None,
        }
    }
}

impl From<BudgetError> for PolicyError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl SearchIgnoreV1 {
    pub(super) fn compile(
        source: &[u8],
        limits: SearchIgnoreV1Limits,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PolicyError> {
        check_limit(
            PolicyLimitResource::FileBytes,
            usize_to_u64(source.len(), "SearchIgnoreV1 file bytes")?,
            limits.max_file_bytes,
        )?;
        let parser_work = usize_to_u64(source.len(), "SearchIgnoreV1 parser work")?
            .checked_mul(PARSER_PASSES)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 parser work",
            })?;
        check_limit(
            PolicyLimitResource::ParserWork,
            parser_work,
            limits.max_parser_work,
        )?;
        budget.consume_bytes(parser_work)?;

        let source = source.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(source);
        let rule_count = validate_rules(source, limits)?;
        if rule_count == 0 {
            return Ok(Self {
                automaton: None,
                patterns: Vec::new(),
            });
        }

        let pattern_capacity = rule_count
            .checked_mul(MAX_COMPILED_PATTERNS_PER_RULE)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 compiled pattern count",
            })?;
        let mut regexes = Vec::new();
        reserve_vec(
            &mut regexes,
            pattern_capacity,
            "SearchIgnoreV1 regex list",
            budget,
        )?;
        let mut patterns = Vec::new();
        reserve_vec(
            &mut patterns,
            pattern_capacity,
            "SearchIgnoreV1 pattern metadata",
            budget,
        )?;

        let mut ordinal = 0_u32;
        visit_rule_lines(source, limits.max_line_bytes, |line_number, line| {
            let Some(parsed) = parse_rule(line_number, line, budget)? else {
                return Ok(());
            };
            let primary = compile_primary_pattern(&parsed, budget)?;
            regexes.push(primary);
            patterns.push(PatternMeta {
                ordinal,
                action: parsed.action,
                scope: parsed.scope,
                role: PatternRole::Primary,
            });
            if parsed.action == RuleAction::Include
                && let Some(descend) = compile_may_descend_pattern(&parsed, budget)?
            {
                regexes.push(descend);
                patterns.push(PatternMeta {
                    ordinal,
                    action: RuleAction::Include,
                    scope: parsed.scope,
                    role: PatternRole::MayDescend,
                });
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "SearchIgnoreV1 rule ordinal",
                })?;
            Ok(())
        })?;

        let compilation_limit = usize::try_from(limits.max_compilation_bytes).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 compilation bytes",
            }
        })?;
        let compilation_peak =
            limits
                .max_compilation_bytes
                .checked_mul(3)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "SearchIgnoreV1 compilation peak bytes",
                })?;
        budget.consume_bytes(compilation_peak)?;

        let syntax_config = syntax::Config::new()
            .unicode(false)
            .utf8(false)
            .nest_limit(64);
        let mut nfa_compiler = thompson::Compiler::new();
        nfa_compiler
            .configure(
                thompson::Config::new()
                    .nfa_size_limit(Some(compilation_limit))
                    .which_captures(thompson::WhichCaptures::None)
                    .shrink(true),
            )
            .syntax(syntax_config);
        let nfa = nfa_compiler.build_many(&regexes).map_err(|error| {
            if error.size_limit().is_some() {
                compilation_limit_error(limits.max_compilation_bytes)
            } else {
                PolicyError::CompilationFailed
            }
        })?;

        let mut builder = dense::Builder::new();
        builder.configure(
            dense::Config::new()
                .match_kind(MatchKind::All)
                .start_kind(StartKind::Anchored)
                .dfa_size_limit(Some(compilation_limit))
                .determinize_size_limit(Some(compilation_limit)),
        );
        let automaton = builder.build_from_nfa(&nfa).map_err(|error| {
            if error.is_size_limit_exceeded() {
                compilation_limit_error(limits.max_compilation_bytes)
            } else {
                PolicyError::CompilationFailed
            }
        })?;
        let actual = usize_to_u64(automaton.memory_usage(), "SearchIgnoreV1 automaton bytes")?;
        check_limit(
            PolicyLimitResource::AutomatonBytes,
            actual,
            limits.max_automaton_bytes,
        )?;

        Ok(Self {
            automaton: Some(automaton),
            patterns,
        })
    }

    pub(super) fn decide(
        &self,
        normalized_relative_path: &str,
        is_directory: bool,
        match_budget: &mut PolicyMatchBudget,
    ) -> Result<PolicyDecision, PolicyError> {
        let Some(automaton) = self.automaton.as_ref() else {
            return Ok(PolicyDecision::Include);
        };
        let input = Input::new(normalized_relative_path.as_bytes()).anchored(Anchored::Yes);
        let mut state = OverlappingState::start();
        let mut primary: Option<PatternMeta> = None;
        let mut may_descend_ordinal: Option<u32> = None;
        loop {
            automaton
                .try_search_overlapping_fwd(&input, &mut state)
                .map_err(|_| PolicyError::MatchFailed)?;
            let Some(found) = state.get_match() else {
                break;
            };
            match_budget.observe_match()?;
            let meta = self.patterns[found.pattern().as_usize()];
            match meta.role {
                PatternRole::Primary
                    if primary.is_none_or(|current| meta.ordinal > current.ordinal) =>
                {
                    primary = Some(meta);
                }
                PatternRole::MayDescend => {
                    may_descend_ordinal = Some(
                        may_descend_ordinal
                            .map_or(meta.ordinal, |current| current.max(meta.ordinal)),
                    );
                }
                PatternRole::Primary => {}
            }
        }

        let Some(primary) = primary else {
            return Ok(PolicyDecision::Include);
        };
        if primary.action == RuleAction::Include {
            return Ok(PolicyDecision::Include);
        }
        if !is_directory || primary.scope != RuleScope::Subtree {
            return Ok(PolicyDecision::ExcludeButDescend);
        }
        if may_descend_ordinal.is_some_and(|ordinal| ordinal > primary.ordinal) {
            Ok(PolicyDecision::ExcludeButDescend)
        } else {
            Ok(PolicyDecision::ExcludeAndPrune)
        }
    }
}

fn compilation_limit_error(limit: u64) -> PolicyError {
    PolicyError::LimitExceeded {
        resource: PolicyLimitResource::CompilationBytes,
        observed_at_least: limit.saturating_add(1),
        limit,
    }
}

#[derive(Debug)]
struct ParsedRule {
    action: RuleAction,
    scope: RuleScope,
    normalized: String,
}

fn validate_rules(source: &[u8], limits: SearchIgnoreV1Limits) -> Result<usize, PolicyError> {
    let mut count = 0_usize;
    visit_rule_lines(source, limits.max_line_bytes, |line_number, line| {
        if parse_rule_view(line_number, line)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "SearchIgnoreV1 rule count",
                })?;
            check_limit(
                PolicyLimitResource::Rules,
                usize_to_u64(count, "SearchIgnoreV1 rule count")?,
                usize_to_u64(limits.max_rules, "SearchIgnoreV1 rule limit")?,
            )?;
        }
        Ok(())
    })?;
    Ok(count)
}

fn visit_rule_lines(
    source: &[u8],
    max_line_bytes: usize,
    mut visit: impl FnMut(u64, &[u8]) -> Result<(), PolicyError>,
) -> Result<(), PolicyError> {
    for (index, raw) in source.split(|byte| *byte == b'\n').enumerate() {
        let line_number = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 line number",
            })?;
        check_limit(
            PolicyLimitResource::LineBytes,
            usize_to_u64(raw.len(), "SearchIgnoreV1 line bytes")?,
            usize_to_u64(max_line_bytes, "SearchIgnoreV1 line byte limit")?,
        )?;
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        visit(line_number, line)?;
    }
    Ok(())
}

fn parse_rule(
    line_number: u64,
    line: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<Option<ParsedRule>, PolicyError> {
    let Some(view) = parse_rule_view(line_number, line)? else {
        return Ok(None);
    };
    budget.check_bytes(usize_to_u64(
        view.path.len(),
        "SearchIgnoreV1 normalized rule bytes",
    )?)?;
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(view.path.len())
        .map_err(|source| PolicyError::Allocation {
            allocation: "SearchIgnoreV1 normalized rule",
            requested: view.path.len(),
            source,
        })?;
    budget.consume_bytes(usize_to_u64(
        normalized.capacity(),
        "SearchIgnoreV1 normalized rule capacity",
    )?)?;
    for character in view.path.chars() {
        normalized.push(if character == '\\' { '/' } else { character });
    }
    Ok(Some(ParsedRule {
        action: view.action,
        scope: view.scope,
        normalized,
    }))
}

#[derive(Debug, Clone, Copy)]
struct ParsedRuleView<'line> {
    action: RuleAction,
    scope: RuleScope,
    path: &'line str,
}

fn parse_rule_view(
    line_number: u64,
    line: &[u8],
) -> Result<Option<ParsedRuleView<'_>>, PolicyError> {
    if line.is_empty() || line[0] == b'#' {
        return Ok(None);
    }
    let line = std::str::from_utf8(line).map_err(|_| PolicyError::Syntax {
        line: line_number,
        reason: PolicySyntaxReason::InvalidUtf8,
    })?;
    let (action, rule) = line
        .strip_prefix('!')
        .map_or((RuleAction::Exclude, line), |rule| {
            (RuleAction::Include, rule)
        });
    let (scope, path) = if let Some(path) = rule.strip_prefix("exact:") {
        (RuleScope::Exact, path)
    } else if let Some(path) = rule.strip_prefix("subtree:") {
        (RuleScope::Subtree, path)
    } else if let Some(path) = rule.strip_prefix("glob:") {
        (RuleScope::Glob, path)
    } else {
        return Err(PolicyError::Syntax {
            line: line_number,
            reason: PolicySyntaxReason::UnknownDirective,
        });
    };
    validate_rule_path(line_number, scope, path)?;
    Ok(Some(ParsedRuleView {
        action,
        scope,
        path,
    }))
}

fn validate_rule_path(line: u64, scope: RuleScope, path: &str) -> Result<(), PolicyError> {
    let fail = |reason| PolicyError::Syntax { line, reason };
    if path.is_empty() {
        return Err(fail(PolicySyntaxReason::EmptyPath));
    }
    if path
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(*byte, b'/' | b'\\'))
        || path
            .as_bytes()
            .last()
            .is_some_and(|byte| matches!(*byte, b'/' | b'\\'))
        || path.as_bytes().get(1).is_some_and(|byte| *byte == b':')
    {
        return Err(fail(PolicySyntaxReason::AbsolutePath));
    }
    if path.chars().any(char::is_control) {
        return Err(fail(PolicySyntaxReason::ControlCharacter));
    }
    let mut has_wildcard = false;
    let mut previous_double_star = false;
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() {
            return Err(fail(PolicySyntaxReason::EmptySegment));
        }
        if matches!(segment, "." | "..") {
            return Err(fail(PolicySyntaxReason::DotSegment));
        }
        if segment
            .chars()
            .any(|character| matches!(character, '?' | '[' | ']' | '{' | '}'))
        {
            return Err(fail(PolicySyntaxReason::UnsupportedGlobSyntax));
        }
        if segment.contains("**") && segment != "**" {
            return Err(fail(PolicySyntaxReason::DoubleStarMustBeCompleteSegment));
        }
        if segment == "**" && previous_double_star {
            return Err(fail(PolicySyntaxReason::UnsupportedGlobSyntax));
        }
        previous_double_star = segment == "**";
        has_wildcard |= segment.contains('*');
    }
    match scope {
        RuleScope::Exact | RuleScope::Subtree if has_wildcard => {
            Err(fail(PolicySyntaxReason::WildcardInLiteralRule))
        }
        RuleScope::Glob if !has_wildcard => Err(fail(PolicySyntaxReason::GlobWithoutWildcard)),
        _ => Ok(()),
    }
}

fn compile_primary_pattern(
    rule: &ParsedRule,
    budget: &mut AssetLoadBudget,
) -> Result<String, PolicyError> {
    let mut regex = allocated_regex(&rule.normalized, budget)?;
    match rule.scope {
        RuleScope::Exact => push_escaped_path(&mut regex, &rule.normalized),
        RuleScope::Subtree => {
            push_escaped_path(&mut regex, &rule.normalized);
            regex.push_str("(?:/[^/]+)*");
        }
        RuleScope::Glob => push_glob_regex(&mut regex, &rule.normalized),
    }
    regex.push_str("\\z");
    Ok(regex)
}

fn compile_may_descend_pattern(
    rule: &ParsedRule,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, PolicyError> {
    let segment_count = rule.normalized.split('/').count();
    let fixed_segments = rule
        .normalized
        .split('/')
        .take_while(|segment| !segment.contains('*'))
        .count();
    let ancestor_count = match rule.scope {
        RuleScope::Exact | RuleScope::Subtree => segment_count.saturating_sub(1),
        RuleScope::Glob => fixed_segments,
    };
    if ancestor_count == 0 && rule.scope != RuleScope::Glob {
        return Ok(None);
    }

    let mut regex = allocated_regex(&rule.normalized, budget)?;
    if rule.scope == RuleScope::Glob && fixed_segments == 0 {
        regex.push_str("[^/]+(?:/[^/]+)*");
    } else {
        push_prefix_chain(
            &mut regex,
            &rule.normalized,
            ancestor_count,
            rule.scope == RuleScope::Glob,
        );
    }
    regex.push_str("\\z");
    Ok(Some(regex))
}

fn allocated_regex(normalized: &str, budget: &mut AssetLoadBudget) -> Result<String, PolicyError> {
    let capacity = normalized
        .len()
        .checked_mul(REGEX_CAPACITY_MULTIPLIER)
        .and_then(|bytes| bytes.checked_add(REGEX_CAPACITY_BASE))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "SearchIgnoreV1 generated regex bytes",
        })?;
    budget.check_bytes(usize_to_u64(
        capacity,
        "SearchIgnoreV1 generated regex bytes",
    )?)?;
    let mut regex = String::new();
    regex
        .try_reserve_exact(capacity)
        .map_err(|source| PolicyError::Allocation {
            allocation: "SearchIgnoreV1 generated regex",
            requested: capacity,
            source,
        })?;
    budget.consume_bytes(usize_to_u64(
        regex.capacity(),
        "SearchIgnoreV1 generated regex capacity",
    )?)?;
    Ok(regex)
}

fn push_prefix_chain(regex: &mut String, path: &str, count: usize, descend_from_full: bool) {
    let mut segments = path.split('/').take(count);
    if let Some(first) = segments.next() {
        push_escaped_path(regex, first);
    }
    let mut optional_segments = 0_usize;
    for segment in segments {
        regex.push_str("(?:/");
        push_escaped_path(regex, segment);
        optional_segments += 1;
    }
    if descend_from_full {
        regex.push_str("(?:/[^/]+)*");
    }
    for _ in 0..optional_segments {
        regex.push_str(")?");
    }
}

fn push_escaped_path(regex: &mut String, path: &str) {
    for character in path.chars() {
        push_escaped_character(regex, character);
    }
}

fn push_escaped_character(regex: &mut String, character: char) {
    if matches!(
        character,
        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\'
    ) {
        regex.push('\\');
    }
    regex.push(character);
}

fn push_glob_regex(regex: &mut String, glob: &str) {
    let mut segments = glob.split('/').peekable();
    let mut index = 0_usize;
    let mut previous_double_star = false;
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        if segment == "**" {
            match (index == 0, last) {
                (true, true) => regex.push_str("[^/]+(?:/[^/]+)*"),
                (true, false) => regex.push_str("(?:[^/]+/)*"),
                (false, true) => regex.push_str("(?:/[^/]+)*"),
                (false, false) => regex.push_str("(?:/[^/]+)*/"),
            }
            previous_double_star = true;
            index += 1;
            continue;
        }
        if index > 0 && !previous_double_star {
            regex.push('/');
        }
        for character in segment.chars() {
            if character == '*' {
                regex.push_str("[^/]*");
            } else {
                push_escaped_character(regex, character);
            }
        }
        previous_double_star = false;
        index += 1;
    }
}

fn reserve_vec<T>(
    values: &mut Vec<T>,
    capacity: usize,
    allocation: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), PolicyError> {
    let requested =
        std::mem::size_of::<T>()
            .checked_mul(capacity)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "SearchIgnoreV1 vector bytes",
            })?;
    budget.check_bytes(usize_to_u64(requested, "SearchIgnoreV1 vector bytes")?)?;
    values
        .try_reserve_exact(capacity)
        .map_err(|source| PolicyError::Allocation {
            allocation,
            requested,
            source,
        })?;
    let actual = std::mem::size_of::<T>()
        .checked_mul(values.capacity())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "SearchIgnoreV1 vector capacity bytes",
        })?;
    budget.consume_bytes(usize_to_u64(
        actual,
        "SearchIgnoreV1 vector capacity bytes",
    )?)?;
    Ok(())
}

fn check_limit(
    resource: PolicyLimitResource,
    observed_at_least: u64,
    limit: u64,
) -> Result<(), PolicyError> {
    if observed_at_least > limit {
        return Err(PolicyError::LimitExceeded {
            resource,
            observed_at_least,
            limit,
        });
    }
    Ok(())
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, PolicyError> {
    u64::try_from(value)
        .map_err(|_| PolicyError::Budget(BudgetError::ArithmeticOverflow { resource }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(source: &[u8]) -> SearchIgnoreV1 {
        SearchIgnoreV1::compile(
            source,
            SearchIgnoreV1Limits::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap()
    }

    fn decide(
        policy: &SearchIgnoreV1,
        normalized_relative_path: &str,
        is_directory: bool,
    ) -> PolicyDecision {
        policy
            .decide(
                normalized_relative_path,
                is_directory,
                &mut PolicyMatchBudget::new(u64::MAX),
            )
            .unwrap()
    }

    #[test]
    fn exact_subtree_and_glob_use_last_match_wins() {
        let policy = compile(
            b"exact:Assets/Drop.asset\n\
              subtree:Assets/Generated\n\
              !exact:Assets/Generated/Keep.asset\n\
              glob:Assets/**/Generated*.asset\n\
              !glob:Assets/**/GeneratedKeep*.asset\n",
        );

        assert_eq!(
            decide(&policy, "Assets/Drop.asset", false),
            PolicyDecision::ExcludeButDescend
        );
        assert_eq!(
            decide(&policy, "Assets/Generated", true),
            PolicyDecision::ExcludeButDescend
        );
        assert_eq!(
            decide(&policy, "Assets/Generated/Keep.asset", false),
            PolicyDecision::Include
        );
        assert_eq!(
            decide(&policy, "Assets/Nested/Generated.asset", false),
            PolicyDecision::ExcludeButDescend
        );
        assert_eq!(
            decide(&policy, "Assets/Nested/GeneratedKeep.asset", false),
            PolicyDecision::Include
        );
    }

    #[test]
    fn subtree_without_later_reinclude_prunes() {
        let policy = compile(b"subtree:Assets/Generated\n");
        assert_eq!(
            decide(&policy, "Assets/Generated", true),
            PolicyDecision::ExcludeAndPrune
        );
    }

    #[test]
    fn bom_crlf_comments_and_windows_separators_are_normalized() {
        let policy =
            compile(b"\xEF\xBB\xBF# policy\r\n\r\nexact:Assets\\Generated\\Drop.asset\r\n");
        assert_ne!(
            decide(&policy, "Assets/Generated/Drop.asset", false),
            PolicyDecision::Include
        );
    }

    #[test]
    fn invalid_syntax_is_rejected_before_compilation() {
        let invalid: &[(&[u8], u64, PolicySyntaxReason)] = &[
            (
                b"# valid comment\nAssets/Foo.asset",
                2,
                PolicySyntaxReason::UnknownDirective,
            ),
            (b"exact:", 1, PolicySyntaxReason::EmptyPath),
            (
                b"exact:/Assets/Foo.asset",
                1,
                PolicySyntaxReason::AbsolutePath,
            ),
            (
                b"exact:Assets//Foo.asset",
                1,
                PolicySyntaxReason::EmptySegment,
            ),
            (
                b"exact:Assets/../Foo.asset",
                1,
                PolicySyntaxReason::DotSegment,
            ),
            (
                b"exact:Assets/Foo\x01.asset",
                1,
                PolicySyntaxReason::ControlCharacter,
            ),
            (
                b"exact:Assets/*.asset",
                1,
                PolicySyntaxReason::WildcardInLiteralRule,
            ),
            (
                b"glob:Assets/Foo.asset",
                1,
                PolicySyntaxReason::GlobWithoutWildcard,
            ),
            (
                b"glob:Assets/**foo.asset",
                1,
                PolicySyntaxReason::DoubleStarMustBeCompleteSegment,
            ),
            (
                b"glob:Assets/?.asset",
                1,
                PolicySyntaxReason::UnsupportedGlobSyntax,
            ),
            (
                b"glob:Assets/**/**/*.asset",
                1,
                PolicySyntaxReason::UnsupportedGlobSyntax,
            ),
            (
                b"exact:Assets/\xFF.asset",
                1,
                PolicySyntaxReason::InvalidUtf8,
            ),
        ];
        for &(source, expected_line, expected_reason) in invalid {
            assert!(
                matches!(
                    SearchIgnoreV1::compile(
                        source,
                        SearchIgnoreV1Limits::default(),
                        &mut AssetLoadBudget::default()
                    ),
                    Err(PolicyError::Syntax { line, reason })
                        if line == expected_line && reason == expected_reason
                ),
                "unexpected syntax result for {source:?}"
            );
        }
    }

    #[test]
    fn file_line_rule_and_parser_limits_accept_exact_and_reject_one_over() {
        let source = b"exact:Assets/Foo.asset\n";
        let defaults = SearchIgnoreV1Limits::default();
        let cases = [
            (
                SearchIgnoreV1Limits {
                    max_file_bytes: source.len() as u64,
                    ..defaults
                },
                SearchIgnoreV1Limits {
                    max_file_bytes: source.len() as u64 - 1,
                    ..defaults
                },
                PolicyLimitResource::FileBytes,
            ),
            (
                SearchIgnoreV1Limits {
                    max_line_bytes: "exact:Assets/Foo.asset".len(),
                    ..defaults
                },
                SearchIgnoreV1Limits {
                    max_line_bytes: "exact:Assets/Foo.asset".len() - 1,
                    ..defaults
                },
                PolicyLimitResource::LineBytes,
            ),
            (
                SearchIgnoreV1Limits {
                    max_rules: 2,
                    ..defaults
                },
                SearchIgnoreV1Limits {
                    max_rules: 1,
                    ..defaults
                },
                PolicyLimitResource::Rules,
            ),
            (
                SearchIgnoreV1Limits {
                    max_parser_work: source.len() as u64 * PARSER_PASSES,
                    ..defaults
                },
                SearchIgnoreV1Limits {
                    max_parser_work: (source.len() as u64 * PARSER_PASSES) - 1,
                    ..defaults
                },
                PolicyLimitResource::ParserWork,
            ),
        ];
        for (exact, one_under, expected) in cases {
            let input = if expected == PolicyLimitResource::Rules {
                b"exact:Assets/A.asset\nexact:Assets/B.asset\n".as_slice()
            } else {
                source.as_slice()
            };
            SearchIgnoreV1::compile(input, exact, &mut AssetLoadBudget::default()).unwrap();
            assert!(matches!(
                SearchIgnoreV1::compile(input, one_under, &mut AssetLoadBudget::default()),
                Err(PolicyError::LimitExceeded { resource, .. }) if resource == expected
            ));
        }
    }

    #[test]
    fn automaton_and_compilation_limits_have_exact_boundaries() {
        let source = b"glob:Assets/**/*.asset\n";
        let defaults = SearchIgnoreV1Limits::default();

        let automaton_boundary =
            minimum_successful_limit(1, defaults.max_automaton_bytes, |limit| {
                SearchIgnoreV1Limits {
                    max_automaton_bytes: limit,
                    ..defaults
                }
            });
        SearchIgnoreV1::compile(
            source,
            SearchIgnoreV1Limits {
                max_automaton_bytes: automaton_boundary,
                ..defaults
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(matches!(
            SearchIgnoreV1::compile(
                source,
                SearchIgnoreV1Limits {
                    max_automaton_bytes: automaton_boundary - 1,
                    ..defaults
                },
                &mut AssetLoadBudget::default(),
            ),
            Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::AutomatonBytes,
                observed_at_least,
                limit,
            }) if observed_at_least == automaton_boundary && limit == automaton_boundary - 1
        ));

        let compilation_boundary =
            minimum_successful_limit(1, defaults.max_compilation_bytes, |limit| {
                SearchIgnoreV1Limits {
                    max_compilation_bytes: limit,
                    ..defaults
                }
            });
        SearchIgnoreV1::compile(
            source,
            SearchIgnoreV1Limits {
                max_compilation_bytes: compilation_boundary,
                ..defaults
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(matches!(
            SearchIgnoreV1::compile(
                source,
                SearchIgnoreV1Limits {
                    max_compilation_bytes: compilation_boundary - 1,
                    ..defaults
                },
                &mut AssetLoadBudget::default(),
            ),
            Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::CompilationBytes,
                observed_at_least,
                limit,
            }) if observed_at_least == compilation_boundary && limit == compilation_boundary - 1
        ));
    }

    #[test]
    fn caller_budget_accepts_exact_compilation_bytes_and_rejects_one_under() {
        let source = b"glob:Assets/**/*.asset\n!exact:Assets/Keep.asset\n";
        let limits = SearchIgnoreV1Limits::default();
        let mut measured = AssetLoadBudget::default();
        SearchIgnoreV1::compile(source, limits, &mut measured).unwrap();
        let required = measured.usage().bytes;

        let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: required,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();
        SearchIgnoreV1::compile(source, limits, &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, required);

        let mut one_under = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: required - 1,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            SearchIgnoreV1::compile(source, limits, &mut one_under),
            Err(PolicyError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == required - 1 && requested == required
        ));
    }

    #[test]
    fn automaton_and_compilation_failures_report_distinct_resources() {
        let source = b"glob:Assets/**/*.asset\n";
        let defaults = SearchIgnoreV1Limits::default();

        let automaton = SearchIgnoreV1::compile(
            source,
            SearchIgnoreV1Limits {
                max_automaton_bytes: 1,
                ..defaults
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(
            automaton,
            PolicyError::LimitExceeded {
                resource: PolicyLimitResource::AutomatonBytes,
                observed_at_least,
                limit: 1,
            } if observed_at_least > 1
        ));

        let compilation = SearchIgnoreV1::compile(
            source,
            SearchIgnoreV1Limits {
                max_compilation_bytes: 1,
                ..defaults
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(
            compilation,
            PolicyError::LimitExceeded {
                resource: PolicyLimitResource::CompilationBytes,
                observed_at_least: 2,
                limit: 1,
            }
        ));
    }

    fn minimum_successful_limit(
        mut lower: u64,
        mut upper: u64,
        limits: impl Fn(u64) -> SearchIgnoreV1Limits,
    ) -> u64 {
        let source = b"glob:Assets/**/*.asset\n";
        assert!(
            SearchIgnoreV1::compile(source, limits(upper), &mut AssetLoadBudget::default()).is_ok()
        );
        while lower < upper {
            let candidate = lower + (upper - lower) / 2;
            if SearchIgnoreV1::compile(source, limits(candidate), &mut AssetLoadBudget::default())
                .is_ok()
            {
                upper = candidate;
            } else {
                lower = candidate + 1;
            }
        }
        lower
    }

    #[test]
    fn large_extension_rule_set_uses_one_bounded_automaton() {
        let defaults = SearchIgnoreV1Limits::default();
        let mut source = String::new();
        let rule_count = defaults.max_rules;
        for ordinal in 0..rule_count {
            use std::fmt::Write as _;
            writeln!(source, "glob:Assets/*.required{ordinal:04}").unwrap();
        }

        let policy =
            SearchIgnoreV1::compile(source.as_bytes(), defaults, &mut AssetLoadBudget::default())
                .unwrap();
        let automaton = policy.automaton.as_ref().unwrap();

        assert_eq!(policy.patterns.len(), rule_count);
        assert!(automaton.memory_usage() as u64 <= defaults.max_automaton_bytes);
        assert_eq!(
            decide(&policy, "Assets/Foo.required0000", false),
            PolicyDecision::ExcludeButDescend
        );
        assert_eq!(
            decide(&policy, "Assets/Foo.required1023", false),
            PolicyDecision::ExcludeButDescend
        );
    }

    #[test]
    fn matching_does_not_consume_caller_budget() {
        let mut budget = AssetLoadBudget::default();
        let policy = SearchIgnoreV1::compile(
            b"glob:Assets/**/*.asset\n",
            SearchIgnoreV1Limits::default(),
            &mut budget,
        )
        .unwrap();
        let before = budget.usage();
        let mut match_budget = PolicyMatchBudget::new(1_000);
        for _ in 0..1_000 {
            let _ = policy
                .decide("Assets/Nested/Foo.asset", false, &mut match_budget)
                .unwrap();
        }
        assert_eq!(budget.usage(), before);
        assert_eq!(match_budget.usage(), 1_000);
    }

    #[test]
    fn matching_accepts_the_exact_work_limit_and_rejects_one_over() {
        let policy = compile(b"glob:Assets/**/*.asset\n");
        let mut match_budget = PolicyMatchBudget::new(1);

        assert_eq!(
            policy
                .decide("Assets/Nested/Foo.asset", false, &mut match_budget)
                .unwrap(),
            PolicyDecision::ExcludeButDescend
        );
        assert!(matches!(
            policy.decide("Assets/Second.asset", false, &mut match_budget),
            Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::MatchWork,
                observed_at_least: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn maximum_overlapping_rule_set_has_a_deterministic_match_work_cutoff() {
        let defaults = SearchIgnoreV1Limits::default();
        let source = "glob:Assets/**/*.asset\n".repeat(defaults.max_rules);
        let policy =
            SearchIgnoreV1::compile(source.as_bytes(), defaults, &mut AssetLoadBudget::default())
                .unwrap();
        let exact_limit = u64::try_from(defaults.max_rules).unwrap();
        let mut exact = PolicyMatchBudget::new(exact_limit);

        assert_eq!(
            policy
                .decide("Assets/Nested/Foo.asset", false, &mut exact)
                .unwrap(),
            PolicyDecision::ExcludeButDescend
        );
        assert_eq!(exact.usage(), exact_limit);

        let mut one_under = PolicyMatchBudget::new(exact_limit - 1);
        assert!(matches!(
            policy.decide("Assets/Nested/Foo.asset", false, &mut one_under),
            Err(PolicyError::LimitExceeded {
                resource: PolicyLimitResource::MatchWork,
                observed_at_least,
                limit,
            }) if observed_at_least == exact_limit && limit == exact_limit - 1
        ));
    }
}
