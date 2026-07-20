use std::fmt::Write as _;
use std::mem::size_of;
use std::sync::Arc;

use unity_asset_binary::asset::SerializedFile;
use unity_asset_binary::typetree::{TypeTreeParseMode, TypeTreeParseOptions};
use unity_asset_core::{AssetLoadBudget, BudgetError, DiagnosticSeverity, UnityDocument};
use unity_asset_yaml::{
    YamlDocument, YamlReferenceDiagnostic, YamlReferenceField, YamlReferenceScanError,
    YamlReferenceShape, YamlValueKind, scan_reference_class_occurrences,
    scan_reference_occurrences,
};

use super::ReferenceGraphError;
use super::cache::{
    LocalObjectId, LocalReferenceDiagnostic, LocalReferenceOccurrence, SourceReferenceOccurrences,
};
use super::fact::{BinaryExternalReference, RawReferenceTarget, ReferenceGuid};
use super::input::{PreparedReferenceOverlay, ReferenceSource, ReferenceSourceParse};

pub(crate) fn scan_source_occurrences(
    source: &ReferenceSource<'_>,
    typetree: TypeTreeParseOptions,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<SourceReferenceOccurrences>, ReferenceGraphError> {
    let candidate = match source.parse() {
        ReferenceSourceParse::Serialized(file) => scan_binary_source(file, None, typetree, budget)?,
        ReferenceSourceParse::Yaml(document) => scan_yaml_source(document, None, budget)?,
        ReferenceSourceParse::PreparedSerialized {
            source,
            file,
            overlay,
        } => scan_binary_source(file, Some((source, overlay)), typetree, budget)?,
        ReferenceSourceParse::PreparedYaml {
            source,
            document,
            overlay,
        } => scan_yaml_source(document, Some((source, overlay)), budget)?,
    };
    let wrapper_bytes = u64::try_from(size_of::<SourceReferenceOccurrences>()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "source reference occurrence cache",
        }
    })?;
    budget.consume_bytes(wrapper_bytes)?;
    Ok(Arc::new(candidate))
}

fn scan_binary_source(
    file: &SerializedFile,
    overlay: Option<(unity_asset_core::SourceId, &dyn PreparedReferenceOverlay)>,
    typetree: TypeTreeParseOptions,
    budget: &mut AssetLoadBudget,
) -> Result<SourceReferenceOccurrences, ReferenceGraphError> {
    let object_count = usize_to_u64(file.object_count(), "binary reference objects")?;
    let mut occurrences = Vec::new();
    let mut diagnostics = Vec::new();
    let mut complete = true;

    for object in file.object_handles() {
        budget.consume_entries(1)?;
        let owner = LocalObjectId::Binary(object.path_id());
        let replacement = overlay
            .and_then(|(source, overlay)| overlay.binary_replacement(source, object.path_id()));
        let scan_result = match replacement {
            Some(replacement) => object.scan_replacement_reference_occurrences_with_options(
                replacement,
                budget,
                typetree,
            ),
            None => object.scan_reference_occurrences_with_options(budget, typetree),
        };
        let scan = match scan_result {
            Ok(Some(scan)) => scan,
            Ok(None) => {
                complete = false;
                push_local_diagnostic(
                    &mut diagnostics,
                    LocalReferenceDiagnostic {
                        severity: DiagnosticSeverity::Warning,
                        code: "REFERENCE_SCHEMA_UNAVAILABLE",
                        message: clone_string(
                            "object has no TypeTree schema for reference discovery",
                            "reference schema diagnostic",
                            budget,
                        )?,
                        source: Some(owner),
                        field_path: None,
                    },
                    budget,
                )?;
                continue;
            }
            Err(error)
                if error.is_resource_error() || typetree.mode == TypeTreeParseMode::Strict =>
            {
                return Err(error.into());
            }
            Err(error) => {
                complete = false;
                push_local_diagnostic(
                    &mut diagnostics,
                    LocalReferenceDiagnostic {
                        severity: DiagnosticSeverity::Warning,
                        code: "REFERENCE_OBJECT_SCAN_FAILED",
                        message: render_bounded_error(
                            &error,
                            "binary reference scan diagnostic",
                            budget,
                        )?,
                        source: Some(owner),
                        field_path: None,
                    },
                    budget,
                )?;
                continue;
            }
        };

        reserve_additional(
            &mut occurrences,
            scan.occurrences.len(),
            "binary reference occurrence bindings",
            budget,
        )?;
        for occurrence in scan.occurrences {
            let external = if occurrence.file_id > 0 {
                usize::try_from(occurrence.file_id - 1)
                    .ok()
                    .and_then(|index| {
                        let external = match overlay {
                            Some((source, overlay)) => overlay.binary_external(source, file, index),
                            None => file.externals.get(index),
                        }?;
                        Some((index, external))
                    })
                    .map(|(index, external)| {
                        let index =
                            u32::try_from(index).map_err(|_| BudgetError::ArithmeticOverflow {
                                resource: "binary external reference index",
                            })?;
                        Ok::<_, ReferenceGraphError>(BinaryExternalReference::new(
                            index,
                            external.guid,
                            external.type_,
                            clone_string(&external.path, "binary external reference path", budget)?,
                        ))
                    })
                    .transpose()?
            } else {
                None
            };
            occurrences.push(LocalReferenceOccurrence {
                source: owner.clone(),
                field_path: occurrence.field_path,
                raw_target: RawReferenceTarget::Binary {
                    file_id: occurrence.file_id,
                    path_id: occurrence.path_id,
                    external,
                },
                diagnostics: Box::new([]),
                invalid: None,
            });
        }

        if !scan.diagnostics.is_empty() {
            complete = false;
        }
        for diagnostic in scan.diagnostics {
            push_local_diagnostic(
                &mut diagnostics,
                LocalReferenceDiagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: "REFERENCE_FIELD_RECOVERED",
                    message: diagnostic.message,
                    source: Some(owner.clone()),
                    field_path: Some(diagnostic.field_path),
                },
                budget,
            )?;
        }
    }

    Ok(SourceReferenceOccurrences {
        occurrences: occurrences.into_boxed_slice(),
        diagnostics: diagnostics.into_boxed_slice(),
        object_count,
        complete,
    })
}

fn scan_yaml_source(
    document: &YamlDocument,
    overlay: Option<(unity_asset_core::SourceId, &dyn PreparedReferenceOverlay)>,
    budget: &mut AssetLoadBudget,
) -> Result<SourceReferenceOccurrences, ReferenceGraphError> {
    let scan = match overlay {
        Some((source, overlay)) => scan_reference_class_occurrences(
            document.entries().len(),
            |index| {
                document
                    .entries()
                    .get(index)
                    .map(|base| overlay.yaml_class(source, index, base))
            },
            budget,
        ),
        None => scan_reference_occurrences(document, budget),
    }
    .map_err(|error| match error {
        YamlReferenceScanError::Budget(source) => ReferenceGraphError::Budget(source),
        YamlReferenceScanError::AllocationFailed {
            resource,
            requested,
            source,
        } => ReferenceGraphError::Allocation {
            resource,
            requested,
            unit: super::ReferenceAllocationUnit::Bytes,
            source,
        },
        other => ReferenceGraphError::Yaml(other),
    })?;
    let object_count = usize_to_u64(document.entries().len(), "YAML reference objects")?;
    let mut occurrences = Vec::new();
    reserve_additional(
        &mut occurrences,
        scan.occurrences.len(),
        "YAML reference occurrence bindings",
        budget,
    )?;

    for occurrence in scan.occurrences {
        let (file_id, guid, type_id, invalid) = match occurrence.shape {
            YamlReferenceShape::Null(target) | YamlReferenceShape::Valid(target) => (
                Some(target.file_id),
                target.guid.map(reference_guid),
                target.type_id,
                None,
            ),
            YamlReferenceShape::Invalid { raw, diagnostic } => (
                raw.file_id,
                raw.guid.map(reference_guid),
                raw.type_id,
                Some(LocalReferenceDiagnostic {
                    severity: DiagnosticSeverity::Warning,
                    code: "YAML_REFERENCE_INVALID",
                    message: render_yaml_diagnostic(&diagnostic, budget)?,
                    source: None,
                    field_path: None,
                }),
            ),
        };
        let raw_target = RawReferenceTarget::Yaml {
            file_id,
            guid,
            type_id,
        };
        occurrences.push(LocalReferenceOccurrence {
            source: LocalObjectId::Yaml(occurrence.object),
            field_path: occurrence.field_path,
            raw_target,
            diagnostics: Box::new([]),
            invalid,
        });
    }

    Ok(SourceReferenceOccurrences {
        occurrences: occurrences.into_boxed_slice(),
        diagnostics: Box::new([]),
        object_count,
        complete: scan.complete,
    })
}

fn reference_guid(value: String) -> ReferenceGuid {
    let mut bytes = [0_u8; 16];
    if value.len() != 32 {
        return ReferenceGuid::Invalid(value);
    }
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let Some(high) = hex_value(chunk[0]) else {
            return ReferenceGuid::Invalid(value);
        };
        let Some(low) = hex_value(chunk[1]) else {
            return ReferenceGuid::Invalid(value);
        };
        bytes[index] = (high << 4) | low;
    }
    ReferenceGuid::Parsed(bytes)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn render_yaml_diagnostic(
    diagnostic: &YamlReferenceDiagnostic,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
    let dynamic = match diagnostic {
        YamlReferenceDiagnostic::UnexpectedField { field } => field.len(),
        YamlReferenceDiagnostic::ConflictingAliases { .. }
        | YamlReferenceDiagnostic::InvalidValueType { .. }
        | YamlReferenceDiagnostic::InvalidGuidLength { .. }
        | YamlReferenceDiagnostic::InvalidGuidHex
        | YamlReferenceDiagnostic::IncompleteExternalReference { .. } => 0,
    };
    let capacity = 128_usize
        .checked_add(dynamic)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "YAML reference diagnostic",
        })?;
    let bytes = usize_to_u64(capacity, "YAML reference diagnostic")?;
    budget.check_bytes(bytes)?;
    let mut message = String::new();
    message
        .try_reserve_exact(capacity)
        .map_err(|error| ReferenceGraphError::Allocation {
            resource: "YAML reference diagnostic",
            requested: capacity,
            unit: super::ReferenceAllocationUnit::Bytes,
            source: error,
        })?;
    match diagnostic {
        YamlReferenceDiagnostic::ConflictingAliases { field } => {
            write!(message, "conflicting aliases for {}", yaml_field(*field))
        }
        YamlReferenceDiagnostic::InvalidValueType { field, actual } => write!(
            message,
            "{} has invalid {} value",
            yaml_field(*field),
            yaml_value_kind(*actual)
        ),
        YamlReferenceDiagnostic::InvalidGuidLength { actual } => {
            write!(
                message,
                "GUID has {actual} hexadecimal characters instead of 32"
            )
        }
        YamlReferenceDiagnostic::InvalidGuidHex => {
            message.push_str("GUID contains a non-hexadecimal character");
            Ok(())
        }
        YamlReferenceDiagnostic::IncompleteExternalReference { missing } => {
            write!(
                message,
                "external reference is missing {}",
                yaml_field(*missing)
            )
        }
        YamlReferenceDiagnostic::UnexpectedField { field } => {
            write!(
                message,
                "reference mapping contains unexpected field {field}"
            )
        }
    }
    .map_err(|_| ReferenceGraphError::Invariant("failed to format YAML reference diagnostic"))?;
    budget.consume_bytes(bytes)?;
    Ok(message)
}

fn render_bounded_error(
    error: &impl std::fmt::Display,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
    const CAPACITY: usize = 4 * 1024;
    let bytes = usize_to_u64(CAPACITY, resource)?;
    budget.check_bytes(bytes)?;
    let mut message = String::new();
    message
        .try_reserve_exact(CAPACITY)
        .map_err(|source| ReferenceGraphError::Allocation {
            resource,
            requested: CAPACITY,
            unit: super::ReferenceAllocationUnit::Bytes,
            source,
        })?;
    let mut writer = CappedDiagnosticWriter {
        output: &mut message,
        limit: CAPACITY,
        truncated: false,
    };
    write!(writer, "{error}")
        .map_err(|_| ReferenceGraphError::Invariant("failed to format reference diagnostic"))?;
    if writer.truncated {
        writer.finish_truncation();
    }
    budget.consume_bytes(bytes)?;
    Ok(message)
}

struct CappedDiagnosticWriter<'output> {
    output: &'output mut String,
    limit: usize,
    truncated: bool,
}

impl CappedDiagnosticWriter<'_> {
    fn finish_truncation(&mut self) {
        const MARKER: &str = "...";
        while self.output.len() + MARKER.len() > self.limit {
            self.output.pop();
        }
        self.output.push_str(MARKER);
    }
}

impl std::fmt::Write for CappedDiagnosticWriter<'_> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.truncated {
            return Ok(());
        }
        let remaining = self.limit.saturating_sub(self.output.len());
        if value.len() <= remaining {
            self.output.push_str(value);
            return Ok(());
        }
        let mut end = remaining;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&value[..end]);
        self.truncated = true;
        Ok(())
    }
}

const fn yaml_field(field: YamlReferenceField) -> &'static str {
    match field {
        YamlReferenceField::FileId => "file ID",
        YamlReferenceField::Guid => "GUID",
        YamlReferenceField::Type => "type ID",
    }
}

const fn yaml_value_kind(kind: YamlValueKind) -> &'static str {
    match kind {
        YamlValueKind::Null => "null",
        YamlValueKind::Bool => "boolean",
        YamlValueKind::Integer => "integer",
        YamlValueKind::Unsigned => "unsigned integer",
        YamlValueKind::Float => "floating-point",
        YamlValueKind::String => "string",
        YamlValueKind::Array => "array",
        YamlValueKind::Bytes => "byte array",
        YamlValueKind::Object => "mapping",
    }
}

pub(crate) fn account_cached_source(
    cached: &SourceReferenceOccurrences,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    let occurrences = usize_to_u64(cached.occurrences.len(), "cached reference occurrences")?;
    let entries =
        cached
            .object_count
            .checked_add(occurrences)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "cached reference entries",
            })?;
    budget.consume_entries(entries)?;
    budget.consume_members(occurrences)?;
    Ok(())
}

fn push_local_diagnostic(
    diagnostics: &mut Vec<LocalReferenceDiagnostic>,
    diagnostic: LocalReferenceDiagnostic,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    reserve_additional(diagnostics, 1, "reference scan diagnostics", budget)?;
    diagnostics.push(diagnostic);
    Ok(())
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
    let bytes = usize_to_u64(value.len(), resource)?;
    budget.check_bytes(bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: value.len(),
            unit: super::ReferenceAllocationUnit::Bytes,
            source: error,
        })?;
    cloned.push_str(value);
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn reserve_additional<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    if required <= values.capacity() {
        return Ok(());
    }
    let new_slots = required - values.capacity();
    let bytes = new_slots
        .checked_mul(size_of::<T>())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    values
        .try_reserve_exact(required - values.len())
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: additional,
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage};

    use super::*;

    struct LongDiagnostic;

    impl std::fmt::Display for LongDiagnostic {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..400 {
                formatter.write_str("diagnostic payload ")?;
            }
            Ok(())
        }
    }

    #[test]
    fn diagnostic_rendering_is_bounded_and_budgeted_before_allocation() {
        let mut budget = AssetLoadBudget::default();
        let message = render_bounded_error(&LongDiagnostic, "test diagnostic", &mut budget)
            .expect("bounded diagnostic");
        assert!(message.len() <= 4 * 1024);
        assert!(message.ends_with("..."));
        assert_eq!(budget.usage().bytes, 4 * 1024);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 4 * 1024 - 1,
            ..AssetLoadLimits::default()
        })
        .expect("one-short budget");
        assert!(matches!(
            render_bounded_error(&LongDiagnostic, "test diagnostic", &mut one_short),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert_eq!(one_short.usage(), AssetLoadUsage::default());
    }
}
