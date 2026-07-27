//! Bounded decoding for untrusted JSON automation contracts.

use std::fmt;
use std::io::{self, Read};
use std::mem::size_of;

use serde::de::{DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::{AssetLoadBudget, BudgetError, BudgetedJsonError};

const READ_CHUNK_BYTES: usize = 4 * 1024;
const READ_CHUNK_BYTES_U64: u64 = 4 * 1024;
const MAX_CONSECUTIVE_INTERRUPTED_READS: u8 = 16;
const MAX_SAFE_DEPTH: u32 = 64;

/// Conservative parser and typed-materialization accounting selected by a contract owner.
///
/// Serde cannot report allocations performed by an arbitrary destination type. The fixed and
/// per-entry materialization values are therefore a proof obligation: they must cover the largest
/// retained wire value and any validation temporaries for that specific contract. The decoder
/// charges the resulting upper bound before typed deserialization begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractJsonResourceModel {
    parser_work_multiplier: u64,
    parser_fixed_work_bytes: u64,
    materialization_fixed_bytes: u64,
    materialization_bytes_per_entry: u64,
}

impl ContractJsonResourceModel {
    #[must_use]
    pub const fn new(
        parser_work_multiplier: u64,
        parser_fixed_work_bytes: u64,
        materialization_fixed_bytes: u64,
        materialization_bytes_per_entry: u64,
    ) -> Self {
        Self {
            parser_work_multiplier,
            parser_fixed_work_bytes,
            materialization_fixed_bytes,
            materialization_bytes_per_entry,
        }
    }
}

/// Contract-specific limits applied before typed JSON materialization.
///
/// There is deliberately no `Default`: every public contract must choose and name its own wire
/// limits. `max_depth` is zero-based, so a scalar root has depth zero and a root collection has
/// depth one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractJsonLimits {
    contract: &'static str,
    max_encoded_bytes: usize,
    max_depth: u32,
    max_entries: u64,
    max_members: u64,
    resources: ContractJsonResourceModel,
}

impl ContractJsonLimits {
    #[must_use]
    pub const fn new(
        contract: &'static str,
        max_encoded_bytes: usize,
        max_depth: u32,
        max_entries: u64,
        max_members: u64,
        resources: ContractJsonResourceModel,
    ) -> Self {
        Self {
            contract,
            max_encoded_bytes,
            max_depth,
            max_entries,
            max_members,
            resources,
        }
    }

    #[must_use]
    pub const fn contract(self) -> &'static str {
        self.contract
    }

    #[must_use]
    pub const fn max_encoded_bytes(self) -> usize {
        self.max_encoded_bytes
    }

    fn validate(self) -> Result<(), BudgetedJsonError> {
        if self.max_encoded_bytes == 0 {
            return Err(self.invalid_limit("encoded_bytes"));
        }
        if self.resources.parser_work_multiplier == 0 {
            return Err(self.invalid_limit("parser_work_multiplier"));
        }
        if self.resources.parser_fixed_work_bytes < READ_CHUNK_BYTES_U64 {
            return Err(self.invalid_limit("parser_fixed_work_bytes"));
        }
        if self.resources.materialization_fixed_bytes == 0 {
            return Err(self.invalid_limit("materialization_fixed_bytes"));
        }
        if self.resources.materialization_bytes_per_entry == 0 {
            return Err(self.invalid_limit("materialization_bytes_per_entry"));
        }
        if self.max_entries == 0 {
            return Err(self.invalid_limit("entries"));
        }
        if self.max_members == 0 {
            return Err(self.invalid_limit("members"));
        }
        if self.max_depth > MAX_SAFE_DEPTH {
            return Err(self.invalid_limit("depth"));
        }
        Ok(())
    }

    const fn invalid_limit(self, resource: &'static str) -> BudgetedJsonError {
        BudgetedJsonError::InvalidLimit {
            contract: self.contract,
            resource,
        }
    }
}

/// Reads and decodes one complete JSON contract using caller-owned resource budgets.
///
/// The encoded hard cap is independent of the caller budget. The byte ledger accounts for the
/// encoded input, conservative parser work, and fixed parser scratch before Serde can materialize
/// the destination type. A structure-only pass then enforces local and caller-owned depth, entry,
/// and member limits. A second JSON document is rejected.
pub fn read_contract_json<T: DeserializeOwned>(
    reader: impl Read,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<T, BudgetedJsonError> {
    limits.validate()?;
    let encoded = read_contract_bytes(reader, budget, limits)?;
    let structure = probe_contract_structure(&encoded, budget, limits)?;
    charge_materialization::<T>(structure, budget, limits)?;

    let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
    deserializer.disable_recursion_limit();
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Decodes a borrowed JSON contract with the same accounting and validation as a reader.
pub fn read_contract_json_slice<T: DeserializeOwned>(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<T, BudgetedJsonError> {
    read_contract_json(encoded, budget, limits)
}

fn read_contract_bytes(
    mut reader: impl Read,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<Vec<u8>, BudgetedJsonError> {
    budget.consume_bytes(limits.resources.parser_fixed_work_bytes)?;
    let mut encoded = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut interrupted_reads = 0_u8;

    loop {
        let remaining = limits.max_encoded_bytes.saturating_sub(encoded.len());
        let read_limit = chunk.len().min(remaining.saturating_add(1));
        let read = match reader.read(&mut chunk[..read_limit]) {
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted
                    && interrupted_reads < MAX_CONSECUTIVE_INTERRUPTED_READS =>
            {
                interrupted_reads += 1;
                continue;
            }
            result => result?,
        };
        interrupted_reads = 0;
        if read == 0 {
            break;
        }

        let requested = encoded
            .len()
            .checked_add(read)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "contract_json_bytes",
            })?;
        if requested > limits.max_encoded_bytes {
            return Err(BudgetedJsonError::EncodedLimitExceeded {
                contract: limits.contract,
                limit: limits.max_encoded_bytes,
                requested,
            });
        }

        let input_bytes = u64::try_from(read).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "contract_json_bytes",
        })?;
        let parser_work = input_bytes
            .checked_mul(limits.resources.parser_work_multiplier)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "contract_json_bytes",
            })?;
        let charged =
            input_bytes
                .checked_add(parser_work)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "contract_json_bytes",
                })?;

        budget.check_bytes(charged)?;
        ensure_capacity(&mut encoded, read, limits.max_encoded_bytes)?;
        budget.consume_bytes(charged)?;
        encoded.extend_from_slice(&chunk[..read]);
    }

    Ok(encoded)
}

fn ensure_capacity(
    encoded: &mut Vec<u8>,
    additional: usize,
    maximum: usize,
) -> Result<(), BudgetedJsonError> {
    let required =
        encoded
            .len()
            .checked_add(additional)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "contract_json_capacity",
            })?;
    if required <= encoded.capacity() {
        return Ok(());
    }
    let target = required
        .checked_next_power_of_two()
        .unwrap_or(maximum)
        .min(maximum);
    let reserve = target
        .checked_sub(encoded.len())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "contract_json_capacity",
        })?;
    encoded
        .try_reserve_exact(reserve)
        .map_err(|_| BudgetedJsonError::AllocationFailed { requested: target })?;
    Ok(())
}

fn probe_contract_structure(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<ContractJsonStructure, BudgetedJsonError> {
    budget.check_entries(1)?;
    budget.consume_entries(1)?;
    let mut state = ProbeState {
        budget,
        limits,
        entries: 1,
        members: 0,
        failure: None,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(encoded);
    deserializer.disable_recursion_limit();
    let result = ProbeSeed {
        state: &mut state,
        depth: 0,
        charge_value: false,
    }
    .deserialize(&mut deserializer);
    if let Some(failure) = state.failure {
        return Err(failure);
    }
    result?;
    deserializer.end()?;
    Ok(ContractJsonStructure {
        entries: state.entries,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContractJsonStructure {
    entries: u64,
}

fn charge_materialization<T>(
    structure: ContractJsonStructure,
    budget: &mut AssetLoadBudget,
    limits: ContractJsonLimits,
) -> Result<(), BudgetedJsonError> {
    let root_layout =
        u64::try_from(size_of::<T>()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "contract_json_materialization",
        })?;
    let entry_bytes = structure
        .entries
        .checked_mul(limits.resources.materialization_bytes_per_entry)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "contract_json_materialization",
        })?;
    let bytes = limits
        .resources
        .materialization_fixed_bytes
        .checked_add(root_layout)
        .and_then(|bytes| bytes.checked_add(entry_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "contract_json_materialization",
        })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

struct ProbeState<'budget> {
    budget: &'budget mut AssetLoadBudget,
    limits: ContractJsonLimits,
    entries: u64,
    members: u64,
    failure: Option<BudgetedJsonError>,
}

impl ProbeState<'_> {
    fn charge_value(&mut self) -> Result<(), BudgetedJsonError> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "contract_json_entries",
            })?;
        self.check_local_limit("entries", entries, self.limits.max_entries)?;
        let members = self
            .members
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "contract_json_members",
            })?;
        self.check_local_limit("members", members, self.limits.max_members)?;

        self.budget.check_entries(1)?;
        self.budget.check_members(1)?;
        self.budget.consume_entries(1)?;
        self.budget.consume_members(1)?;
        self.entries = entries;
        self.members = members;
        Ok(())
    }

    fn observe_depth(&mut self, depth: u32) -> Result<(), BudgetedJsonError> {
        self.check_local_limit("depth", u64::from(depth), u64::from(self.limits.max_depth))?;
        self.budget.check_depth(depth)?;
        self.budget.observe_depth(depth)?;
        Ok(())
    }

    fn check_local_limit(
        &self,
        resource: &'static str,
        requested: u64,
        limit: u64,
    ) -> Result<(), BudgetedJsonError> {
        if requested > limit {
            return Err(BudgetedJsonError::StructureLimitExceeded {
                contract: self.limits.contract,
                resource,
                limit,
                requested,
            });
        }
        Ok(())
    }
}

struct ProbeSeed<'state, 'budget> {
    state: &'state mut ProbeState<'budget>,
    depth: u32,
    charge_value: bool,
}

impl<'de> DeserializeSeed<'de> for ProbeSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.charge_value
            && let Err(error) = self.state.charge_value()
        {
            self.state.failure = Some(error);
            return Err(serde::de::Error::custom(
                "JSON contract structure limit exceeded",
            ));
        }
        deserializer.deserialize_any(ProbeVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}

struct ProbeVisitor<'state, 'budget> {
    state: &'state mut ProbeState<'budget>,
    depth: u32,
}

impl<'de> Visitor<'de> for ProbeVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON contract value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(mut self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let child_depth = self.enter_container::<A::Error>()?;
        while sequence
            .next_element_seed(ProbeSeed {
                state: &mut *self.state,
                depth: child_depth,
                charge_value: true,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(mut self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let child_depth = self.enter_container::<A::Error>()?;
        while mapping.next_key::<IgnoredAny>()?.is_some() {
            mapping.next_value_seed(ProbeSeed {
                state: &mut *self.state,
                depth: child_depth,
                charge_value: true,
            })?;
        }
        Ok(())
    }
}

impl ProbeVisitor<'_, '_> {
    fn enter_container<E>(&mut self) -> Result<u32, E>
    where
        E: serde::de::Error,
    {
        let depth = self.depth.saturating_add(1);
        if let Err(error) = self.state.observe_depth(depth) {
            self.state.failure = Some(error);
            return Err(E::custom("JSON contract nesting limit exceeded"));
        }
        Ok(depth)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use serde::Deserialize;

    use super::*;
    use crate::AssetLoadLimits;

    const TEST_RESOURCES: ContractJsonResourceModel =
        ContractJsonResourceModel::new(2, 4 * 1024, 16, 4);
    const TEST_LIMITS: ContractJsonLimits =
        ContractJsonLimits::new("test.contract", 64, 4, 16, 16, TEST_RESOURCES);

    fn budget(max_bytes: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            max_depth: 16,
            max_entries: 64,
            max_members: 64,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    #[test]
    fn charges_encoded_input_and_parser_work_before_materialization() {
        let mut exact = budget(4_128);
        let value: () = read_contract_json(b"null".as_slice(), &mut exact, TEST_LIMITS).unwrap();
        assert_eq!(value, ());
        assert_eq!(exact.usage().bytes, 4_128);

        let mut short = budget(4_127);
        assert!(matches!(
            read_contract_json::<()>(b"null".as_slice(), &mut short, TEST_LIMITS),
            Err(BudgetedJsonError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 4_127,
                requested: 4_128,
            }))
        ));
    }

    #[test]
    fn checks_materialization_budget_before_typed_deserialization() {
        struct DeserializationMustNotStart;

        impl<'de> Deserialize<'de> for DeserializationMustNotStart {
            fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                panic!("typed deserialization started before its budget was reserved");
            }
        }

        let mut short = budget(4_127);
        assert!(matches!(
            read_contract_json::<DeserializationMustNotStart>(
                b"null".as_slice(),
                &mut short,
                TEST_LIMITS,
            ),
            Err(BudgetedJsonError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 4_127,
                requested: 4_128,
            }))
        ));
    }

    #[test]
    fn rejects_encoded_input_above_the_contract_cap() {
        let limits = ContractJsonLimits::new("tiny", 4, 4, 16, 16, TEST_RESOURCES);
        let error = read_contract_json::<serde_json::Value>(
            b"[0,1]".as_slice(),
            &mut AssetLoadBudget::default(),
            limits,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BudgetedJsonError::EncodedLimitExceeded {
                contract: "tiny",
                limit: 4,
                requested: 5,
            }
        ));
    }

    #[test]
    fn rejects_local_depth_entry_and_member_overflow() {
        let depth_limits = ContractJsonLimits::new("depth", 64, 1, 16, 16, TEST_RESOURCES);
        let error = read_contract_json::<serde_json::Value>(
            b"[[0]]".as_slice(),
            &mut AssetLoadBudget::default(),
            depth_limits,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BudgetedJsonError::StructureLimitExceeded {
                contract: "depth",
                resource: "depth",
                limit: 1,
                requested: 2,
            }
        ));

        let entry_limits = ContractJsonLimits::new("wide", 64, 4, 2, 16, TEST_RESOURCES);
        let error = read_contract_json::<serde_json::Value>(
            b"[0,1]".as_slice(),
            &mut AssetLoadBudget::default(),
            entry_limits,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BudgetedJsonError::StructureLimitExceeded {
                contract: "wide",
                resource: "entries",
                limit: 2,
                requested: 3,
            }
        ));

        let member_limits = ContractJsonLimits::new("wide", 64, 4, 16, 1, TEST_RESOURCES);
        let error = read_contract_json::<serde_json::Value>(
            b"[0,1]".as_slice(),
            &mut AssetLoadBudget::default(),
            member_limits,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BudgetedJsonError::StructureLimitExceeded {
                contract: "wide",
                resource: "members",
                limit: 1,
                requested: 2,
            }
        ));
    }

    #[test]
    fn accepts_depth_sixty_four_and_rejects_sixty_five() {
        let limits = ContractJsonLimits::new("depth", 256, 64, 128, 128, TEST_RESOURCES);
        let at_limit = format!("{}0{}", "[".repeat(64), "]".repeat(64));
        read_contract_json::<serde_json::Value>(
            at_limit.as_bytes(),
            &mut AssetLoadBudget::default(),
            limits,
        )
        .unwrap();

        let above_limit = format!("{}0{}", "[".repeat(65), "]".repeat(65));
        assert!(matches!(
            read_contract_json::<serde_json::Value>(
                above_limit.as_bytes(),
                &mut AssetLoadBudget::default(),
                limits,
            ),
            Err(BudgetedJsonError::StructureLimitExceeded {
                contract: "depth",
                resource: "depth",
                limit: 64,
                requested: 65,
            })
        ));
    }

    #[test]
    fn rejects_a_second_json_document() {
        assert!(matches!(
            read_contract_json::<()>(
                b"null null".as_slice(),
                &mut AssetLoadBudget::default(),
                TEST_LIMITS,
            ),
            Err(BudgetedJsonError::Json(_))
        ));
    }

    #[test]
    fn rejects_profiles_that_disable_the_stack_safety_ceiling() {
        let limits = ContractJsonLimits::new("unsafe", 64, 65, 16, 16, TEST_RESOURCES);
        assert!(matches!(
            read_contract_json::<()>(b"null".as_slice(), &mut AssetLoadBudget::default(), limits,),
            Err(BudgetedJsonError::InvalidLimit {
                contract: "unsafe",
                resource: "depth",
            })
        ));

        let underaccounted = ContractJsonLimits::new(
            "unsafe",
            64,
            4,
            16,
            16,
            ContractJsonResourceModel::new(1, READ_CHUNK_BYTES_U64 - 1, 1, 1),
        );
        assert!(matches!(
            read_contract_json::<()>(
                b"null".as_slice(),
                &mut AssetLoadBudget::default(),
                underaccounted,
            ),
            Err(BudgetedJsonError::InvalidLimit {
                contract: "unsafe",
                resource: "parser_fixed_work_bytes",
            })
        ));
    }

    #[test]
    fn fragmented_readers_have_the_same_contract_semantics() {
        let required =
            4_096 + 15 + 16 + u64::try_from(std::mem::size_of::<Vec<u64>>()).unwrap() + 12;
        let mut budget = budget(required);
        let values: Vec<u64> =
            read_contract_json(OneByteReader::new(b"[1,2]"), &mut budget, TEST_LIMITS).unwrap();
        assert_eq!(values, [1, 2]);
        assert_eq!(budget.usage().bytes, required);
        assert_eq!(budget.usage().entries, 3);
        assert_eq!(budget.usage().members, 2);
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn fragmented_readers_enforce_the_exact_raw_cap() {
        let limits = ContractJsonLimits::new("fragmented", 4, 4, 16, 16, TEST_RESOURCES);
        read_contract_json::<()>(
            OneByteReader::new(b"null"),
            &mut AssetLoadBudget::default(),
            limits,
        )
        .unwrap();

        assert!(matches!(
            read_contract_json::<()>(
                OneByteReader::new(b"null "),
                &mut AssetLoadBudget::default(),
                limits,
            ),
            Err(BudgetedJsonError::EncodedLimitExceeded {
                contract: "fragmented",
                limit: 4,
                requested: 5,
            })
        ));
    }

    #[test]
    fn interrupted_reads_retry_and_other_io_errors_preserve_prior_charges() {
        let value: () = read_contract_json(
            InterruptOnceReader::new(b"null"),
            &mut AssetLoadBudget::default(),
            TEST_LIMITS,
        )
        .unwrap();
        assert_eq!(value, ());

        let mut budget = AssetLoadBudget::default();
        assert!(matches!(
            read_contract_json::<()>(FailAfterReader::new(b"nu"), &mut budget, TEST_LIMITS),
            Err(BudgetedJsonError::Io(error))
                if error.kind() == io::ErrorKind::ConnectionReset
        ));
        assert_eq!(budget.usage().bytes, 4_102);

        assert!(matches!(
            read_contract_json::<()>(
                AlwaysInterrupted,
                &mut AssetLoadBudget::default(),
                TEST_LIMITS,
            ),
            Err(BudgetedJsonError::Io(error))
                if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    struct OneByteReader<'a> {
        remaining: &'a [u8],
    }

    impl<'a> OneByteReader<'a> {
        const fn new(remaining: &'a [u8]) -> Self {
            Self { remaining }
        }
    }

    impl Read for OneByteReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.remaining.is_empty() || output.is_empty() {
                return Ok(0);
            }
            output[0] = self.remaining[0];
            self.remaining = &self.remaining[1..];
            Ok(1)
        }
    }

    struct InterruptOnceReader<'a> {
        inner: OneByteReader<'a>,
        interrupted: bool,
    }

    impl<'a> InterruptOnceReader<'a> {
        const fn new(remaining: &'a [u8]) -> Self {
            Self {
                inner: OneByteReader::new(remaining),
                interrupted: false,
            }
        }
    }

    impl Read for InterruptOnceReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.inner.read(output)
        }
    }

    struct FailAfterReader<'a> {
        first: Option<&'a [u8]>,
    }

    impl<'a> FailAfterReader<'a> {
        const fn new(first: &'a [u8]) -> Self {
            Self { first: Some(first) }
        }
    }

    impl Read for FailAfterReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let Some(first) = self.first.take() else {
                return Err(io::Error::from(io::ErrorKind::ConnectionReset));
            };
            output[..first.len()].copy_from_slice(first);
            Ok(first.len())
        }
    }

    struct AlwaysInterrupted;

    impl Read for AlwaysInterrupted {
        fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::Interrupted))
        }
    }
}
