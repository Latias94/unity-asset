//! FMOD FSB5 parsing and Vorbis-to-Ogg reconstruction.
//!
//! Unity frequently stores streamed `AudioClip` payloads in an FMOD sound bank
//! instead of directly embedding a standard Ogg stream. The FSB5 Vorbis codec
//! strips the Ogg layer, so consumers must restore packet framing and the
//! matching Vorbis setup header before exposing a playable file.
//!
//! The setup-header database was imported from Fmod5Sharp under the MIT
//! license. See `assets/FMOD5SHARP-NOTICE` and `assets/FMOD5SHARP-LICENSE`.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem::size_of;
use std::ops::Range;
use std::sync::OnceLock;

use base64::Engine as _;
use ogg::{PacketWriteEndInfo, PacketWriter};
use serde::Deserialize;
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};

const FSB5_MAGIC: &[u8; 4] = b"FSB5";
const FSB5_HEADER_LEN: usize = 0x3C;
const FSB5_VERSION_ZERO_HEADER_LEN: usize = 0x40;
const FSB5_VORBIS_CODEC: u32 = 15;
const FSB5_VORBIS_DATA_CHUNK: u32 = 11;
const FSB5_FREQUENCY_CHUNK: u32 = 2;
const FSB5_CHANNELS_CHUNK: u32 = 1;
const OGG_SERIAL: u32 = 1;
const MAX_VORBIS_PACKETS: usize = 1_000_000;
// `ogg` retains at most one 255-segment page of borrowed packet descriptors,
// lacing values, and a fixed page header. This ceiling includes allocator and
// map overhead with substantial headroom.
const OGG_WRITER_SCRATCH_BYTES: u64 = 32 * 1024;
const VORBIS_SETUP_HEADERS: &str = include_str!("../../assets/fmod5sharp-vorbis-headers.json");

/// Largest setup packet accepted from the bundled FSB5 Vorbis header database.
///
/// This is also the fixed component of extraction's conservative Ogg output
/// bound. Keeping the cap here prevents a future database update from silently
/// invalidating that bound.
pub const MAX_VORBIS_SETUP_PACKET_BYTES: u64 = 1024 * 1024;

static VORBIS_HEADERS: OnceLock<Result<BTreeMap<u32, VorbisSetup>, String>> = OnceLock::new();

pub(crate) fn is_fsb5(bytes: &[u8]) -> bool {
    bytes.starts_with(FSB5_MAGIC)
}

/// Validated, budgeted instructions for rebuilding one FSB5 Vorbis subsound.
pub(crate) struct PreparedVorbisOgg {
    sample_range: Range<usize>,
    channels: u8,
    frequency: u32,
    sample_frames: u64,
    setup_crc: u32,
    block_flags: Vec<bool>,
    packet_ranges: Vec<Range<usize>>,
}

impl PreparedVorbisOgg {
    pub(crate) fn prepare(
        bytes: &[u8],
        subsound_index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, Fsb5Error> {
        let bank = Fsb5Bank::parse(bytes, budget)?;
        if bank.codec != FSB5_VORBIS_CODEC {
            return Err(Fsb5Error::UnsupportedCodec { mode: bank.codec });
        }
        let sample = bank.sample(subsound_index)?;
        let sample_range = sample.range.clone();
        let channels = sample.channels;
        let frequency = sample.frequency;
        let sample_frames = sample.sample_frames;
        let setup_crc = sample
            .vorbis_setup_crc
            .ok_or(Fsb5Error::MissingVorbisData)?;

        let setup = vorbis_setup(setup_crc)?;
        let block_flags = setup.block_flags(budget)?;
        let sample_bytes = bytes
            .get(sample_range.clone())
            .ok_or(Fsb5Error::Truncated("FSB5 selected sample"))?;
        let packet_ranges = parse_packet_ranges(sample_bytes, budget)?;
        budget.consume_bytes(OGG_WRITER_SCRATCH_BYTES)?;

        Ok(Self {
            sample_range,
            channels,
            frequency,
            sample_frames,
            setup_crc,
            block_flags,
            packet_ranges,
        })
    }

    pub(crate) fn write_to<W: Write + ?Sized>(
        &self,
        bytes: &[u8],
        writer: &mut W,
    ) -> Result<(), Fsb5Error> {
        let sample = bytes
            .get(self.sample_range.clone())
            .ok_or(Fsb5Error::Truncated("FSB5 selected sample"))?;
        let setup = vorbis_setup(self.setup_crc)?;
        let info = build_info_packet(self.channels, self.frequency)?;
        let comment = build_comment_packet();
        let mut ogg = PacketWriter::new(writer);
        ogg.write_packet(&info[..], OGG_SERIAL, PacketWriteEndInfo::EndPage, 0)
            .map_err(Fsb5Error::Output)?;
        ogg.write_packet(
            &comment[..],
            OGG_SERIAL,
            PacketWriteEndInfo::NormalPacket,
            0,
        )
        .map_err(Fsb5Error::Output)?;
        ogg.write_packet(
            setup.header_bytes.as_slice(),
            OGG_SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(Fsb5Error::Output)?;

        let mut granule_position = 0_u64;
        let mut previous_block_size = 0_u64;
        for (index, range) in self.packet_ranges.iter().enumerate() {
            let packet = sample
                .get(range.clone())
                .ok_or(Fsb5Error::Truncated("FSB5 prepared Vorbis packet"))?;
            let block_size = packet_block_size(packet, &self.block_flags)?;
            let previous_granule = granule_position;
            if previous_block_size != 0 {
                granule_position = granule_position
                    .checked_add((block_size + previous_block_size) / 4)
                    .ok_or(Fsb5Error::Invalid("Vorbis granule position overflow"))?;
            }
            previous_block_size = block_size;
            let is_last = index + 1 == self.packet_ranges.len();
            let output_granule = if is_last {
                exact_end_granule(self.sample_frames, previous_granule, granule_position)?
            } else {
                granule_position
            };
            let end = if is_last {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            ogg.write_packet(packet, OGG_SERIAL, end, output_granule)
                .map_err(Fsb5Error::Output)?;
        }
        Ok(())
    }
}

fn exact_end_granule(declared: u64, previous: u64, theoretical: u64) -> Result<u64, Fsb5Error> {
    if declared < previous || declared > theoretical {
        return Err(Fsb5Error::Invalid(
            "FSB5 declared sample count is outside the final Vorbis block",
        ));
    }
    Ok(declared)
}

fn build_info_packet(channels: u8, frequency: u32) -> Result<[u8; 30], Fsb5Error> {
    if channels == 0 {
        return Err(Fsb5Error::Invalid("FSB5 Vorbis sample has no channels"));
    }
    if frequency == 0 {
        return Err(Fsb5Error::Invalid("FSB5 Vorbis sample has no sample rate"));
    }

    let mut packet = [0_u8; 30];
    packet[0] = 1;
    packet[1..7].copy_from_slice(b"vorbis");
    packet[11] = channels;
    packet[12..16].copy_from_slice(&frequency.to_le_bytes());
    packet[28] = 0b1011_1000;
    packet[29] = 1;
    Ok(packet)
}

fn build_comment_packet() -> [u8; 27] {
    const VENDOR: &[u8; 11] = b"unity-asset";

    let mut packet = [0_u8; 27];
    packet[0] = 3;
    packet[1..7].copy_from_slice(b"vorbis");
    packet[7..11].copy_from_slice(&11_u32.to_le_bytes());
    packet[11..22].copy_from_slice(VENDOR);
    packet[26] = 1;
    packet
}

fn charge_vec<T>(budget: &mut AssetLoadBudget, capacity: usize) -> Result<(), Fsb5Error> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

/*
 * The setup database below is trusted package data. Untrusted bank metadata,
 * packet framing, and all per-operation allocations are validated above under
 * the caller's budget before concurrent output begins.
 */

fn vorbis_setup(crc: u32) -> Result<&'static VorbisSetup, Fsb5Error> {
    let headers = VORBIS_HEADERS.get_or_init(load_vorbis_headers);
    let headers = headers
        .as_ref()
        .map_err(|error| Fsb5Error::HeaderDatabase(error.clone()))?;
    headers
        .get(&crc)
        .ok_or(Fsb5Error::UnknownVorbisSetup { crc })
}

fn load_vorbis_headers() -> Result<BTreeMap<u32, VorbisSetup>, String> {
    let raw: BTreeMap<String, VorbisSetupWire> = serde_json::from_str(VORBIS_SETUP_HEADERS)
        .map_err(|error| format!("failed to parse bundled FSB5 Vorbis setup headers: {error}"))?;
    let mut headers = BTreeMap::new();
    for (crc, entry) in raw {
        let crc = crc
            .parse::<u32>()
            .map_err(|error| format!("invalid bundled FSB5 Vorbis setup CRC {crc:?}: {error}"))?;
        let header_bytes = base64::engine::general_purpose::STANDARD
            .decode(entry.header_bytes)
            .map_err(|error| format!("invalid bundled FSB5 Vorbis setup {crc:#010X}: {error}"))?;
        if u64::try_from(header_bytes.len())
            .map_or(true, |length| length > MAX_VORBIS_SETUP_PACKET_BYTES)
        {
            return Err(format!(
                "bundled FSB5 Vorbis setup {crc:#010X} exceeds the {MAX_VORBIS_SETUP_PACKET_BYTES}-byte limit"
            ));
        }
        if !header_bytes.starts_with(&[5, b'v', b'o', b'r', b'b', b'i', b's']) {
            return Err(format!(
                "bundled FSB5 Vorbis setup {crc:#010X} is not a Vorbis setup packet"
            ));
        }
        if headers
            .insert(
                crc,
                VorbisSetup {
                    header_bytes,
                    seek_bit: entry.seek_bit,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate bundled FSB5 Vorbis setup {crc:#010X}"));
        }
    }
    Ok(headers)
}

#[derive(Deserialize)]
struct VorbisSetupWire {
    #[serde(rename = "headerBytes")]
    header_bytes: String,
    #[serde(rename = "seekBit")]
    seek_bit: usize,
}

struct VorbisSetup {
    header_bytes: Vec<u8>,
    seek_bit: usize,
}

impl VorbisSetup {
    fn block_flags(&self, budget: &mut AssetLoadBudget) -> Result<Vec<bool>, Fsb5Error> {
        let mut reader = LsbBitReader::new(&self.header_bytes);
        if reader.read_bits(8)? != 5 || !reader.read_bytes(6)?.eq(b"vorbis") {
            return Err(Fsb5Error::Invalid("invalid FSB5 Vorbis setup packet"));
        }
        reader.seek(self.seek_bit)?;
        let mode_count = usize::try_from(reader.read_bits(6)? + 1)
            .map_err(|_| Fsb5Error::Invalid("Vorbis mode count does not fit usize"))?;
        budget.consume_entries(u64::try_from(mode_count).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "entries",
            }
        })?)?;
        charge_vec::<bool>(budget, mode_count)?;
        let mut flags = Vec::new();
        flags
            .try_reserve_exact(mode_count)
            .map_err(|_| Fsb5Error::Allocation("Vorbis mode flags"))?;
        for _ in 0..mode_count {
            flags.push(reader.read_bits(1)? != 0);
            reader.skip(16 + 16 + 8)?;
        }
        Ok(flags)
    }
}

fn packet_block_size(packet: &[u8], block_flags: &[bool]) -> Result<u64, Fsb5Error> {
    let mut reader = LsbBitReader::new(packet);
    if reader.read_bits(1)? != 0 {
        return Ok(0);
    }
    let mode_bits = bit_width(block_flags.len().saturating_sub(1));
    let mode = usize::try_from(reader.read_bits(mode_bits)?)
        .map_err(|_| Fsb5Error::Invalid("Vorbis mode index does not fit usize"))?;
    let long_block = *block_flags.get(mode).ok_or(Fsb5Error::Invalid(
        "Vorbis packet references an unknown mode",
    ))?;
    Ok(if long_block { 2_048 } else { 256 })
}

fn bit_width(value: usize) -> u8 {
    if value == 0 {
        0
    } else {
        (usize::BITS - value.leading_zeros()) as u8
    }
}

fn parse_packet_ranges(
    bytes: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<Range<usize>>, Fsb5Error> {
    let mut cursor = PacketCursor::new(bytes);
    let mut packet_count = 0_usize;
    while cursor.next_range()?.is_some() {
        if packet_count == MAX_VORBIS_PACKETS {
            return Err(Fsb5Error::Invalid(
                "FSB5 Vorbis packet count exceeds the codec limit",
            ));
        }
        budget.consume_entries(1)?;
        packet_count = packet_count
            .checked_add(1)
            .ok_or(Fsb5Error::Invalid("FSB5 Vorbis packet count overflow"))?;
    }
    if packet_count == 0 {
        return Err(Fsb5Error::Invalid("FSB5 Vorbis sample has no packets"));
    }

    charge_vec::<Range<usize>>(budget, packet_count)?;
    let mut packets = Vec::new();
    packets
        .try_reserve_exact(packet_count)
        .map_err(|_| Fsb5Error::Allocation("FSB5 Vorbis packet ranges"))?;
    let mut cursor = PacketCursor::new(bytes);
    while let Some(range) = cursor.next_range()? {
        packets.push(range);
    }
    Ok(packets)
}

struct PacketCursor<'bytes> {
    bytes: &'bytes [u8],
    cursor: usize,
}

impl<'bytes> PacketCursor<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn next_range(&mut self) -> Result<Option<Range<usize>>, Fsb5Error> {
        if self.cursor == self.bytes.len() {
            return Ok(None);
        }
        let remaining = self.bytes.len() - self.cursor;
        if remaining == 1 {
            let padding = self.bytes[self.cursor];
            self.cursor = self.bytes.len();
            return if padding == 0 {
                Ok(None)
            } else {
                Err(Fsb5Error::Invalid(
                    "FSB5 Vorbis sample has non-zero trailing data",
                ))
            };
        }

        let packet_size = u16::from_le_bytes(
            self.bytes[self.cursor..self.cursor + 2]
                .try_into()
                .expect("two-byte packet size"),
        );
        self.cursor += 2;
        if packet_size == 0 || packet_size == u16::MAX {
            if self.bytes[self.cursor..].iter().any(|byte| *byte != 0) {
                return Err(Fsb5Error::Invalid(
                    "FSB5 Vorbis sample has data after its terminator",
                ));
            }
            self.cursor = self.bytes.len();
            return Ok(None);
        }

        let end = self
            .cursor
            .checked_add(usize::from(packet_size))
            .ok_or(Fsb5Error::Invalid("FSB5 Vorbis packet range overflow"))?;
        if end > self.bytes.len() {
            return Err(Fsb5Error::Truncated("FSB5 Vorbis packet body"));
        }
        let range = self.cursor..end;
        self.cursor = end;
        Ok(Some(range))
    }
}

struct Fsb5Bank {
    codec: u32,
    samples: Vec<Fsb5Sample>,
}

impl Fsb5Bank {
    fn parse(bytes: &[u8], budget: &mut AssetLoadBudget) -> Result<Self, Fsb5Error> {
        if !is_fsb5(bytes) {
            return Err(Fsb5Error::NotFsb5);
        }
        let version = read_u32_at(bytes, 4, "FSB5 version")?;
        let sample_count = read_u32_at(bytes, 8, "FSB5 sample count")?;
        let sample_headers_len = usize_from_u32(
            read_u32_at(bytes, 12, "FSB5 sample header size")?,
            "FSB5 sample header size",
        )?;
        let names_len = usize_from_u32(
            read_u32_at(bytes, 16, "FSB5 name table size")?,
            "FSB5 name table size",
        )?;
        let data_len = usize_from_u32(read_u32_at(bytes, 20, "FSB5 data size")?, "FSB5 data size")?;
        let codec = read_u32_at(bytes, 24, "FSB5 codec")?;
        let header_len = if version == 0 {
            FSB5_VERSION_ZERO_HEADER_LEN
        } else {
            FSB5_HEADER_LEN
        };
        if bytes.len() < header_len {
            return Err(Fsb5Error::Truncated("FSB5 header"));
        }
        let sample_headers_end = header_len
            .checked_add(sample_headers_len)
            .ok_or(Fsb5Error::Invalid("FSB5 sample header range overflow"))?;
        let names_end = sample_headers_end
            .checked_add(names_len)
            .ok_or(Fsb5Error::Invalid("FSB5 name table range overflow"))?;
        let data_end = names_end
            .checked_add(data_len)
            .ok_or(Fsb5Error::Invalid("FSB5 data range overflow"))?;
        if bytes.len() < data_end {
            return Err(Fsb5Error::Truncated("FSB5 declared data"));
        }
        let sample_count = usize_from_u32(sample_count, "FSB5 sample count")?;
        let minimum_headers_len = sample_count
            .checked_mul(8)
            .ok_or(Fsb5Error::Invalid("FSB5 sample header count overflow"))?;
        if minimum_headers_len > sample_headers_len {
            return Err(Fsb5Error::Invalid(
                "FSB5 sample header table is shorter than its sample count",
            ));
        }

        budget.consume_entries(u64::try_from(sample_count).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "entries",
            }
        })?)?;
        charge_vec::<SampleMetadata>(budget, sample_count)?;
        charge_vec::<Fsb5Sample>(budget, sample_count)?;

        let mut cursor = header_len;
        let mut metadata_entries = Vec::new();
        metadata_entries
            .try_reserve_exact(sample_count)
            .map_err(|_| Fsb5Error::Allocation("FSB5 sample metadata"))?;
        for _ in 0..sample_count {
            metadata_entries.push(parse_sample_metadata(
                bytes,
                &mut cursor,
                sample_headers_end,
                budget,
            )?);
        }

        let mut samples = Vec::new();
        samples
            .try_reserve_exact(metadata_entries.len())
            .map_err(|_| Fsb5Error::Allocation("FSB5 samples"))?;
        for (index, metadata) in metadata_entries.iter().enumerate() {
            let end_offset = metadata
                .next_offset(&metadata_entries, index, data_len)
                .ok_or(Fsb5Error::Invalid("FSB5 sample offsets are not ordered"))?;
            let start = names_end
                .checked_add(metadata.data_offset)
                .ok_or(Fsb5Error::Invalid("FSB5 sample start overflow"))?;
            let end = names_end
                .checked_add(end_offset)
                .ok_or(Fsb5Error::Invalid("FSB5 sample end overflow"))?;
            bytes
                .get(start..end)
                .ok_or(Fsb5Error::Truncated("FSB5 sample data"))?;
            samples.push(Fsb5Sample {
                range: start..end,
                channels: metadata.channels,
                frequency: metadata.frequency,
                sample_frames: metadata.sample_frames,
                vorbis_setup_crc: metadata.vorbis_setup_crc,
            });
        }
        Ok(Self { codec, samples })
    }

    fn sample(&self, subsound_index: usize) -> Result<&Fsb5Sample, Fsb5Error> {
        self.samples
            .get(subsound_index)
            .ok_or(Fsb5Error::SubsoundOutOfRange {
                requested: subsound_index,
                available: self.samples.len(),
            })
    }
}

struct Fsb5Sample {
    range: Range<usize>,
    channels: u8,
    frequency: u32,
    sample_frames: u64,
    vorbis_setup_crc: Option<u32>,
}

struct SampleMetadata {
    data_offset: usize,
    channels: u8,
    frequency: u32,
    sample_frames: u64,
    vorbis_setup_crc: Option<u32>,
}

impl SampleMetadata {
    fn next_offset(&self, metadata: &[Self], index: usize, data_len: usize) -> Option<usize> {
        let next = metadata
            .get(index + 1)
            .map_or(data_len, |next| next.data_offset);
        (self.data_offset <= next && next <= data_len).then_some(next)
    }
}

fn parse_sample_metadata(
    bytes: &[u8],
    cursor: &mut usize,
    headers_end: usize,
    budget: &mut AssetLoadBudget,
) -> Result<SampleMetadata, Fsb5Error> {
    let encoded = read_u64(bytes, cursor, headers_end, "FSB5 sample metadata")?;
    let has_chunks = encoded & 1 != 0;
    let frequency_id = u32::try_from((encoded >> 1) & 0xF)
        .map_err(|_| Fsb5Error::Invalid("FSB5 frequency identifier does not fit u32"))?;
    let channel_bits = u8::try_from((encoded >> 5) & 0x3)
        .map_err(|_| Fsb5Error::Invalid("FSB5 channel identifier does not fit u8"))?;
    let channels = match channel_bits {
        0 => 1,
        1 => 2,
        2 => 6,
        3 => 8,
        _ => unreachable!("a two-bit channel identifier has four values"),
    };
    let data_offset = usize::try_from(((encoded >> 7) & 0x07FF_FFFF) * 32)
        .map_err(|_| Fsb5Error::Invalid("FSB5 data offset does not fit usize"))?;
    let sample_frames = encoded >> 34;
    if sample_frames == 0 {
        return Err(Fsb5Error::Invalid("FSB5 sample frame count is zero"));
    }
    let mut metadata = SampleMetadata {
        data_offset,
        channels,
        frequency: frequency_for_id(frequency_id)?,
        sample_frames,
        vorbis_setup_crc: None,
    };
    if !has_chunks {
        return Ok(metadata);
    }

    loop {
        let info = read_u32(bytes, cursor, headers_end, "FSB5 sample chunk header")?;
        budget.consume_members(1)?;
        let more = info & 1 != 0;
        let chunk_len = usize::try_from((info >> 1) & 0x00FF_FFFF)
            .map_err(|_| Fsb5Error::Invalid("FSB5 chunk length does not fit usize"))?;
        let chunk_type = info >> 25;
        let chunk_start = *cursor;
        let chunk_end = chunk_start
            .checked_add(chunk_len)
            .ok_or(Fsb5Error::Invalid("FSB5 sample chunk range overflow"))?;
        let chunk = bytes
            .get(chunk_start..chunk_end)
            .filter(|_| chunk_end <= headers_end)
            .ok_or(Fsb5Error::Truncated("FSB5 sample chunk data"))?;
        match chunk_type {
            FSB5_VORBIS_DATA_CHUNK => {
                let crc = chunk
                    .get(..4)
                    .and_then(|crc| crc.try_into().ok())
                    .map(u32::from_le_bytes)
                    .ok_or(Fsb5Error::Truncated("FSB5 Vorbis data chunk"))?;
                metadata.vorbis_setup_crc = Some(crc);
            }
            FSB5_FREQUENCY_CHUNK => {
                let frequency = chunk
                    .get(..4)
                    .and_then(|frequency| frequency.try_into().ok())
                    .map(u32::from_le_bytes)
                    .ok_or(Fsb5Error::Truncated("FSB5 frequency chunk"))?;
                if frequency == 0 {
                    return Err(Fsb5Error::Invalid("FSB5 frequency chunk is zero"));
                }
                metadata.frequency = frequency;
            }
            FSB5_CHANNELS_CHUNK => {
                let channels = *chunk
                    .first()
                    .ok_or(Fsb5Error::Truncated("FSB5 channels chunk"))?;
                if channels == 0 {
                    return Err(Fsb5Error::Invalid("FSB5 channels chunk is zero"));
                }
                metadata.channels = channels;
            }
            _ => {}
        }
        *cursor = chunk_end;
        if !more {
            return Ok(metadata);
        }
    }
}

fn frequency_for_id(id: u32) -> Result<u32, Fsb5Error> {
    match id {
        0 => Ok(4_000),
        1 => Ok(8_000),
        2 => Ok(11_000),
        3 => Ok(12_000),
        4 => Ok(16_000),
        5 => Ok(22_050),
        6 => Ok(24_000),
        7 => Ok(32_000),
        8 => Ok(44_100),
        9 => Ok(48_000),
        10 => Ok(96_000),
        _ => Err(Fsb5Error::Invalid(
            "FSB5 frequency identifier is unsupported",
        )),
    }
}

fn read_u32(
    bytes: &[u8],
    cursor: &mut usize,
    end: usize,
    context: &'static str,
) -> Result<u32, Fsb5Error> {
    let slice = take(bytes, cursor, end, 4, context)?;
    Ok(u32::from_le_bytes(
        slice.try_into().expect("four-byte slice"),
    ))
}

fn read_u64(
    bytes: &[u8],
    cursor: &mut usize,
    end: usize,
    context: &'static str,
) -> Result<u64, Fsb5Error> {
    let slice = take(bytes, cursor, end, 8, context)?;
    Ok(u64::from_le_bytes(
        slice.try_into().expect("eight-byte slice"),
    ))
}

fn read_u32_at(bytes: &[u8], offset: usize, context: &'static str) -> Result<u32, Fsb5Error> {
    let end = offset
        .checked_add(4)
        .ok_or(Fsb5Error::Invalid("FSB5 fixed header range overflow"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or(Fsb5Error::Truncated(context))?;
    Ok(u32::from_le_bytes(
        slice.try_into().expect("four-byte slice"),
    ))
}

fn take<'bytes>(
    bytes: &'bytes [u8],
    cursor: &mut usize,
    end: usize,
    width: usize,
    context: &'static str,
) -> Result<&'bytes [u8], Fsb5Error> {
    let next = cursor
        .checked_add(width)
        .ok_or(Fsb5Error::Invalid("FSB5 cursor range overflow"))?;
    if next > end {
        return Err(Fsb5Error::Truncated(context));
    }
    let slice = bytes
        .get(*cursor..next)
        .ok_or(Fsb5Error::Truncated(context))?;
    *cursor = next;
    Ok(slice)
}

fn usize_from_u32(value: u32, context: &'static str) -> Result<usize, Fsb5Error> {
    usize::try_from(value).map_err(|_| Fsb5Error::Invalid(context))
}

struct LsbBitReader<'bytes> {
    bytes: &'bytes [u8],
    bit_position: usize,
}

impl<'bytes> LsbBitReader<'bytes> {
    fn new(bytes: &'bytes [u8]) -> Self {
        Self {
            bytes,
            bit_position: 0,
        }
    }

    fn seek(&mut self, bit_position: usize) -> Result<(), Fsb5Error> {
        if bit_position > self.bytes.len().saturating_mul(8) {
            return Err(Fsb5Error::Truncated("Vorbis setup seek position"));
        }
        self.bit_position = bit_position;
        Ok(())
    }

    fn skip(&mut self, bits: usize) -> Result<(), Fsb5Error> {
        let next = self
            .bit_position
            .checked_add(bits)
            .ok_or(Fsb5Error::Invalid("Vorbis bit position overflow"))?;
        self.seek(next)
    }

    fn read_bytes(&mut self, count: usize) -> Result<&'bytes [u8], Fsb5Error> {
        if !self.bit_position.is_multiple_of(8) {
            return Err(Fsb5Error::Invalid("Vorbis byte read is not byte-aligned"));
        }
        let start = self.bit_position / 8;
        let end = start
            .checked_add(count)
            .ok_or(Fsb5Error::Invalid("Vorbis byte range overflow"))?;
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or(Fsb5Error::Truncated("Vorbis setup packet"))?;
        self.bit_position = end * 8;
        Ok(bytes)
    }

    fn read_bits(&mut self, count: u8) -> Result<u32, Fsb5Error> {
        if count > 32 {
            return Err(Fsb5Error::Invalid("Vorbis bit read exceeds u32"));
        }
        let count = usize::from(count);
        let end = self
            .bit_position
            .checked_add(count)
            .ok_or(Fsb5Error::Invalid("Vorbis bit range overflow"))?;
        if end > self.bytes.len().saturating_mul(8) {
            return Err(Fsb5Error::Truncated("Vorbis packet bits"));
        }
        let mut value = 0_u32;
        for index in 0..count {
            let position = self.bit_position + index;
            let bit = (self.bytes[position / 8] >> (position % 8)) & 1;
            value |= u32::from(bit) << index;
        }
        self.bit_position = end;
        Ok(value)
    }
}

/// Internal failures while validating or rebuilding an FSB5 Vorbis stream.
#[derive(Debug, Error)]
pub(crate) enum Fsb5Error {
    #[error("payload does not begin with FSB5")]
    NotFsb5,
    #[error("truncated {0}")]
    Truncated(&'static str),
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("failed to allocate {0}")]
    Allocation(&'static str),
    #[error("unsupported FSB5 codec mode {mode}")]
    UnsupportedCodec { mode: u32 },
    #[error("FSB5 subsound index {requested} is outside the {available} available samples")]
    SubsoundOutOfRange { requested: usize, available: usize },
    #[error("FSB5 Vorbis sample has no VORBISDATA chunk")]
    MissingVorbisData,
    #[error("unknown FSB5 Vorbis setup CRC {crc:#010X}")]
    UnknownVorbisSetup { crc: u32 },
    #[error("invalid FSB5 header database: {0}")]
    HeaderDatabase(String),
    #[error("FSB5 load budget exceeded: {0}")]
    Budget(#[from] BudgetError),
    #[error("failed to write Ogg stream: {0}")]
    Output(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    const SHORT_VORBIS: &[u8] = include_bytes!("../../tests/fixtures/short_vorbis.fsb");

    #[test]
    fn parses_single_sample_fsb5_metadata() {
        let bytes = fsb5_with_one_vorbis_sample(&[2, 0, 0, 0]);

        let bank = Fsb5Bank::parse(&bytes, &mut AssetLoadBudget::default()).unwrap();
        let sample = bank.sample(0).unwrap();

        assert_eq!(bank.codec, FSB5_VORBIS_CODEC);
        assert_eq!(sample.channels, 2);
        assert_eq!(sample.frequency, 48_000);
        assert_eq!(sample.sample_frames, 1_234);
        assert_eq!(sample.vorbis_setup_crc, Some(0xAABB_CCDD));
        assert_eq!(&bytes[sample.range.clone()], [2, 0, 0, 0]);
    }

    #[test]
    fn rejects_a_short_fsb5_header() {
        assert!(matches!(
            Fsb5Bank::parse(b"FSB5", &mut AssetLoadBudget::default()),
            Err(Fsb5Error::Truncated("FSB5 version"))
        ));
    }

    #[test]
    fn packet_parser_rejects_a_truncated_packet() {
        assert!(matches!(
            parse_packet_ranges(&[4, 0, 1, 2], &mut AssetLoadBudget::default()),
            Err(Fsb5Error::Truncated("FSB5 Vorbis packet body"))
        ));
    }

    #[test]
    fn packet_parser_preserves_little_endian_packet_boundaries() {
        let bytes = [2, 0, 1, 2, 3, 0, 3, 4, 5, 0, 0];
        let packets = parse_packet_ranges(&bytes, &mut AssetLoadBudget::default()).unwrap();

        assert_eq!(packets, vec![2..4, 6..9]);
    }

    #[test]
    fn packet_parser_accepts_one_fsb_alignment_byte() {
        let packets =
            parse_packet_ranges(&[1, 0, 0x80, 0], &mut AssetLoadBudget::default()).unwrap();

        assert_eq!(packets, vec![2..3]);
    }

    #[test]
    fn packet_parser_rejects_data_after_terminator() {
        assert!(matches!(
            parse_packet_ranges(&[1, 0, 0x80, 0, 0, 0xAA], &mut AssetLoadBudget::default()),
            Err(Fsb5Error::Invalid(
                "FSB5 Vorbis sample has data after its terminator"
            ))
        ));
    }

    #[test]
    fn packet_table_obeys_the_caller_budget() {
        let limits = AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();

        assert!(matches!(
            parse_packet_ranges(&[1, 0, 0x80, 1, 0, 0x80], &mut budget),
            Err(Fsb5Error::Budget(BudgetError::Exceeded {
                resource: "entries",
                ..
            }))
        ));
    }

    #[test]
    fn real_fsb5_rebuild_uses_the_declared_eos_granule() {
        let prepared =
            PreparedVorbisOgg::prepare(SHORT_VORBIS, 0, &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(prepared.sample_frames, 24_806);

        let mut output = Vec::new();
        prepared.write_to(SHORT_VORBIS, &mut output).unwrap();

        assert_eq!(last_ogg_granule(&output), Some(24_806));
    }

    #[test]
    fn exact_end_granule_rejects_an_impossible_declared_count() {
        assert!(exact_end_granule(99, 100, 200).is_err());
        assert!(exact_end_granule(201, 100, 200).is_err());
        assert_eq!(exact_end_granule(150, 100, 200).unwrap(), 150);
    }

    fn fsb5_with_one_vorbis_sample(sample: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; FSB5_HEADER_LEN];
        bytes[..4].copy_from_slice(FSB5_MAGIC);
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&16_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&(sample.len() as u32).to_le_bytes());
        bytes[24..28].copy_from_slice(&FSB5_VORBIS_CODEC.to_le_bytes());

        let encoded = 1_u64 | (9_u64 << 1) | (1_u64 << 5) | (1_234_u64 << 34);
        bytes.extend_from_slice(&encoded.to_le_bytes());
        let chunk_info = (4_u32 << 1) | (FSB5_VORBIS_DATA_CHUNK << 25);
        bytes.extend_from_slice(&chunk_info.to_le_bytes());
        bytes.extend_from_slice(&0xAABB_CCDD_u32.to_le_bytes());
        bytes.extend_from_slice(sample);
        bytes
    }

    fn last_ogg_granule(bytes: &[u8]) -> Option<u64> {
        let mut cursor = 0_usize;
        let mut last = None;
        while cursor < bytes.len() {
            let header = bytes
                .get(cursor..cursor + 27)
                .expect("every Ogg page must contain its fixed header");
            assert_eq!(&header[..4], b"OggS");
            last = Some(u64::from_le_bytes(header[6..14].try_into().unwrap()));
            let segment_count = usize::from(header[26]);
            let lacing = bytes
                .get(cursor + 27..cursor + 27 + segment_count)
                .expect("every Ogg page must contain its segment table");
            let payload = lacing
                .iter()
                .map(|value| usize::from(*value))
                .sum::<usize>();
            cursor = cursor
                .checked_add(27 + segment_count + payload)
                .expect("Ogg page length must not overflow");
            assert!(cursor <= bytes.len(), "Ogg page exceeds the artifact");
        }
        last
    }
}
