use std::io::{self, Read, Write};

use flate2::{Compression, GzBuilder};
use thiserror::Error;
use unity_asset_binary::webfile::{WebFile, WebFileCompression};

use crate::artifact::{
    ArtifactBatch, ArtifactBuildError, ArtifactBuildFailurePhase, ArtifactHandle, encode_brotli,
};

const MAX_MEMBER_NAME_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WebFilePackingPolicy {
    #[default]
    Preserve,
    Uncompressed,
    Gzip,
    Brotli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedPacking {
    Uncompressed,
    Gzip,
    Brotli,
}

/// One already-prepared artifact used as a WebFile member.
///
/// The member length is captured from the batch at construction time, so a handle and its wire
/// length cannot drift apart between planning and encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebFileArtifactMember<'name> {
    name: &'name str,
    artifact: ArtifactHandle,
    length: u64,
}

impl<'name> WebFileArtifactMember<'name> {
    pub fn new(
        batch: &ArtifactBatch<'_, '_>,
        name: &'name str,
        artifact: ArtifactHandle,
    ) -> std::result::Result<Self, WebFileWriteError> {
        let length = batch.artifact_len(artifact)?;
        Ok(Self {
            name,
            artifact,
            length,
        })
    }

    #[must_use]
    pub const fn name(self) -> &'name str {
        self.name
    }

    #[must_use]
    pub const fn artifact(self) -> ArtifactHandle {
        self.artifact
    }
}

/// Errors raised while constructing a prepared WebFile artifact.
#[derive(Debug, Error)]
pub enum WebFileWriteError {
    #[error(transparent)]
    Artifact(Box<ArtifactBuildError>),
    #[error("invalid WebFile signature {signature:?}; expected UnityWebData* or TuanjieWebData*")]
    InvalidSignature { signature: String },
    #[error("WebFile signature contains an embedded NUL byte")]
    SignatureContainsNul,
    #[error("WebFile member {ordinal} name contains an embedded NUL byte")]
    MemberNameContainsNul { ordinal: usize },
    #[error("WebFile member {ordinal} name is {length} bytes; the parser limit is {limit}")]
    MemberNameTooLong {
        ordinal: usize,
        length: usize,
        limit: usize,
    },
    #[error("WebFile member {ordinal} length {length} does not fit the i32 wire field")]
    MemberLengthTooLarge { ordinal: usize, length: u64 },
    #[error("WebFile {field} value {value} does not fit the i32 wire field")]
    WireFieldTooLarge { field: &'static str, value: u64 },
    #[error("WebFile arithmetic overflow while computing {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

impl WebFileWriteError {
    /// Reports the artifact-build stage in which this WebFile preparation failed.
    #[must_use]
    pub const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::Artifact(error) => error.failure_phase(),
            _ => ArtifactBuildFailurePhase::Encoding,
        }
    }
}

impl From<ArtifactBuildError> for WebFileWriteError {
    fn from(error: ArtifactBuildError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WebFileLayout {
    head_length: u64,
    total_length: u64,
}

pub struct WebFileWriter;

impl WebFileWriter {
    /// Build a budgeted, independently reparsed WebFile artifact from prepared member roots.
    ///
    /// The member slice is consumed in its given order and duplicate names are retained as
    /// distinct wire occurrences. Compression is performed directly into a generated proof image;
    /// compressed output is never materialized in a second temporary `Vec`.
    pub fn prepare(
        batch: &mut ArtifactBatch<'_, '_>,
        web: &WebFile,
        members: &[WebFileArtifactMember<'_>],
        policy: WebFilePackingPolicy,
    ) -> std::result::Result<ArtifactHandle, WebFileWriteError> {
        let layout = plan_layout(
            web.signature.as_str(),
            members.iter().map(|member| (member.name, member.length)),
        )?;
        let compression = resolve_packing(web.compression, policy);

        match compression {
            ResolvedPacking::Uncompressed => {
                let handle = batch.prepare_web_file(layout.total_length, |encoder| {
                    let mut header = encoder.generated_chunk_writer()?;
                    write_header(
                        &mut header,
                        web.signature.as_str(),
                        layout,
                        members.iter().map(|member| (member.name, member.length)),
                    )?;
                    let header = encoder.finish_generated_chunk(header)?;
                    encoder.push_payload_full(&header)?;

                    for member in members {
                        if member.length == 0 {
                            // The full append records an empty edge and rejects a nonempty child.
                            encoder.append_dependency(member.artifact)?;
                        } else {
                            encoder.append_dependency_range(member.artifact, 0..member.length)?;
                        }
                    }
                    Ok(())
                })?;
                Ok(handle)
            }
            ResolvedPacking::Gzip | ResolvedPacking::Brotli => {
                let derived = batch.derive_generated_chunk(|encoder| {
                    let generated = encoder.generated_chunk_writer()?;
                    match compression {
                        ResolvedPacking::Gzip => {
                            let mut compressor = GzBuilder::new()
                                .mtime(0)
                                .write(generated, Compression::best());
                            write_header(
                                &mut compressor,
                                web.signature.as_str(),
                                layout,
                                members.iter().map(|member| (member.name, member.length)),
                            )?;
                            for member in members {
                                if member.length == 0 {
                                    encoder.record_empty_dependency(member.artifact)?;
                                    continue;
                                }
                                let mut reader = encoder.dependency_reader(member.artifact)?;
                                let mut limited = (&mut reader).take(member.length);
                                let copied = io::copy(&mut limited, &mut compressor)?;
                                if copied != member.length {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        format!(
                                            "WebFile member {:?} has {} bytes, expected {}",
                                            member.name, copied, member.length
                                        ),
                                    )
                                    .into());
                                }
                            }
                            let generated = compressor.finish()?;
                            encoder.finish_generated_chunk(generated)?;
                        }
                        ResolvedPacking::Brotli => {
                            let generated = encode_brotli(
                                encoder.codec_scratch_budget(),
                                generated,
                                layout.total_length,
                                |compressor| {
                                    write_header(
                                        compressor,
                                        web.signature.as_str(),
                                        layout,
                                        members.iter().map(|member| (member.name, member.length)),
                                    )?;
                                    for member in members {
                                        if member.length == 0 {
                                            encoder.record_empty_dependency(member.artifact)?;
                                            continue;
                                        }
                                        let mut reader =
                                            encoder.dependency_reader(member.artifact)?;
                                        let mut limited = (&mut reader).take(member.length);
                                        let copied = io::copy(&mut limited, compressor)?;
                                        if copied != member.length {
                                            return Err(io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                format!(
                                                    "WebFile member {:?} has {} bytes, expected {}",
                                                    member.name, copied, member.length
                                                ),
                                            )
                                            .into());
                                        }
                                    }
                                    Ok::<(), ArtifactBuildError>(())
                                },
                            )?;
                            encoder.finish_generated_chunk(generated)?;
                        }
                        ResolvedPacking::Uncompressed => {
                            return Err(ArtifactBuildError::InternalInvariant {
                                message: "resolved WebFile compression unexpectedly uncompressed",
                            });
                        }
                    }
                    Ok(())
                })?;
                let compressed_length = derived.len();
                let handle = batch.prepare_web_file(compressed_length, |encoder| {
                    encoder.push_derived_generated_chunk(derived)
                })?;
                Ok(handle)
            }
        }
    }
}

fn resolve_packing(original: WebFileCompression, policy: WebFilePackingPolicy) -> ResolvedPacking {
    match policy {
        WebFilePackingPolicy::Preserve => match original {
            WebFileCompression::None => ResolvedPacking::Uncompressed,
            WebFileCompression::Gzip => ResolvedPacking::Gzip,
            WebFileCompression::Brotli => ResolvedPacking::Brotli,
        },
        WebFilePackingPolicy::Uncompressed => ResolvedPacking::Uncompressed,
        WebFilePackingPolicy::Gzip => ResolvedPacking::Gzip,
        WebFilePackingPolicy::Brotli => ResolvedPacking::Brotli,
    }
}

fn plan_layout<'a, I>(
    signature: &str,
    members: I,
) -> std::result::Result<WebFileLayout, WebFileWriteError>
where
    I: IntoIterator<Item = (&'a str, u64)>,
{
    if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
        return Err(WebFileWriteError::InvalidSignature {
            signature: signature.to_owned(),
        });
    }
    if signature.as_bytes().contains(&0) {
        return Err(WebFileWriteError::SignatureContainsNul);
    }

    let signature_bytes =
        signature
            .len()
            .checked_add(1)
            .ok_or(WebFileWriteError::ArithmeticOverflow {
                resource: "signature_length",
            })?;
    let mut head_length = u64::try_from(signature_bytes)
        .map_err(|_| WebFileWriteError::ArithmeticOverflow {
            resource: "signature_length",
        })?
        .checked_add(4)
        .ok_or(WebFileWriteError::ArithmeticOverflow {
            resource: "header_length",
        })?;
    let mut data_length = 0_u64;

    for (ordinal, (name, length)) in members.into_iter().enumerate() {
        if name.as_bytes().contains(&0) {
            return Err(WebFileWriteError::MemberNameContainsNul { ordinal });
        }
        if name.len() > MAX_MEMBER_NAME_BYTES {
            return Err(WebFileWriteError::MemberNameTooLong {
                ordinal,
                length: name.len(),
                limit: MAX_MEMBER_NAME_BYTES,
            });
        }
        if length > i32::MAX as u64 {
            return Err(WebFileWriteError::MemberLengthTooLarge { ordinal, length });
        }

        let name_length =
            u64::try_from(name.len()).map_err(|_| WebFileWriteError::ArithmeticOverflow {
                resource: "member_name_length",
            })?;
        head_length = head_length
            .checked_add(12)
            .and_then(|value| value.checked_add(name_length))
            .ok_or(WebFileWriteError::ArithmeticOverflow {
                resource: "header_length",
            })?;
        data_length =
            data_length
                .checked_add(length)
                .ok_or(WebFileWriteError::ArithmeticOverflow {
                    resource: "member_data_length",
                })?;
    }

    if head_length > i32::MAX as u64 {
        return Err(WebFileWriteError::WireFieldTooLarge {
            field: "head_length",
            value: head_length,
        });
    }
    // `member_offset` is checked while writing each directory record. The final data end may be
    // larger than i32; only encoded offset/length fields are constrained by the wire format.
    let total_length =
        head_length
            .checked_add(data_length)
            .ok_or(WebFileWriteError::ArithmeticOverflow {
                resource: "total_length",
            })?;
    Ok(WebFileLayout {
        head_length,
        total_length,
    })
}

fn write_header<'a, W, I>(
    writer: &mut W,
    signature: &str,
    layout: WebFileLayout,
    members: I,
) -> io::Result<()>
where
    W: Write + ?Sized,
    I: IntoIterator<Item = (&'a str, u64)>,
{
    writer.write_all(signature.as_bytes())?;
    writer.write_all(&[0])?;
    writer.write_all(
        &i32::try_from(layout.head_length)
            .map_err(|_| invalid_wire_value("head_length", layout.head_length))?
            .to_le_bytes(),
    )?;

    let mut cursor = layout.head_length;
    for (name, length) in members {
        let offset =
            i32::try_from(cursor).map_err(|_| invalid_wire_value("member_offset", cursor))?;
        let length =
            i32::try_from(length).map_err(|_| invalid_wire_value("member_length", length))?;
        let name_length = i32::try_from(name.len())
            .map_err(|_| invalid_wire_value("member_name_length", name.len() as u64))?;
        writer.write_all(&offset.to_le_bytes())?;
        writer.write_all(&length.to_le_bytes())?;
        writer.write_all(&name_length.to_le_bytes())?;
        writer.write_all(name.as_bytes())?;
        cursor = cursor
            .checked_add(
                u64::try_from(length).map_err(|_| invalid_wire_value("member_length", u64::MAX))?,
            )
            .ok_or_else(|| invalid_wire_value("member_data_end", u64::MAX))?;
    }
    Ok(())
}

fn invalid_wire_value(field: &'static str, value: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("WebFile {field} value {value} does not fit the wire representation"),
    )
}
