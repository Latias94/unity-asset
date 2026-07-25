//! Strict Ogg container validation shared by audio decode and export paths.

const MAX_VORBIS_HEADER_PACKET_BYTES: usize = 1024 * 1024;

pub(crate) fn final_ogg_granule(bytes: &[u8]) -> Option<u64> {
    let mut cursor = 0_usize;
    let mut saw_page = false;
    let mut expected_sequence = 0_u32;
    let mut saw_eos = false;
    let mut final_granule = None;
    let mut serial = None;
    while cursor < bytes.len() {
        let header = bytes.get(cursor..cursor.saturating_add(27))?;
        let flags = header[5];
        if !header.starts_with(b"OggS") || header[4] != 0 || flags & !0x07 != 0 || saw_eos {
            return None;
        }
        if !saw_page {
            if flags & 0x03 != 0x02 {
                return None;
            }
        } else if flags & 0x02 != 0 {
            return None;
        }
        let page_serial = u32::from_le_bytes(header[14..18].try_into().expect("Ogg serial"));
        let page_sequence =
            u32::from_le_bytes(header[18..22].try_into().expect("Ogg page sequence"));
        if serial.is_some_and(|expected| expected != page_serial)
            || page_sequence != expected_sequence
        {
            return None;
        }
        serial = Some(page_serial);
        let segment_count = usize::from(header[26]);
        let lacing_end = cursor
            .checked_add(27)
            .and_then(|start| start.checked_add(segment_count))?;
        let lacing = bytes.get(cursor + 27..lacing_end)?;
        let payload_length = lacing.iter().try_fold(0_usize, |total, value| {
            total.checked_add(usize::from(*value))
        })?;
        let page_end = lacing_end.checked_add(payload_length)?;
        if page_end > bytes.len() {
            return None;
        }
        let page = &bytes[cursor..page_end];
        let declared_checksum =
            u32::from_le_bytes(header[22..26].try_into().expect("Ogg checksum"));
        if ogg_page_checksum(page) != declared_checksum {
            return None;
        }
        saw_eos = flags & 0x04 != 0;
        if saw_eos {
            final_granule = Some(u64::from_le_bytes(
                header[6..14].try_into().expect("Ogg granule position"),
            ));
        }
        cursor = page_end;
        saw_page = true;
        expected_sequence = expected_sequence.wrapping_add(1);
    }
    if saw_page && saw_eos {
        final_granule
    } else {
        None
    }
}

/// Verifies the mandatory Vorbis packet headers after strict Ogg validation.
///
/// A valid Ogg container can carry arbitrary codecs. Export only treats it as a
/// direct Vorbis artifact after its three required headers are structurally
/// present, so a CRC-correct non-Vorbis stream cannot receive an `.ogg`
/// artifact by accident.
pub(crate) fn is_ogg_vorbis(bytes: &[u8]) -> bool {
    if final_ogg_granule(bytes).is_none() {
        return false;
    }

    let mut cursor = 0_usize;
    let mut packet = Vec::new();
    let mut packet_index = 0_usize;
    let mut expects_continuation = false;
    while cursor < bytes.len() && packet_index < 3 {
        let header = &bytes[cursor..cursor + 27];
        let continued = header[5] & 0x01 != 0;
        if continued != expects_continuation {
            return false;
        }
        let segment_count = usize::from(header[26]);
        let lacing_end = cursor + 27 + segment_count;
        let lacing = &bytes[cursor + 27..lacing_end];
        let payload_end = lacing
            .iter()
            .fold(lacing_end, |end, length| end + usize::from(*length));
        let mut payload = &bytes[lacing_end..payload_end];
        for length in lacing {
            let length = usize::from(*length);
            let Some(next_len) = packet.len().checked_add(length) else {
                return false;
            };
            if next_len > MAX_VORBIS_HEADER_PACKET_BYTES {
                return false;
            }
            if packet.try_reserve(length).is_err() {
                return false;
            }
            packet.extend_from_slice(&payload[..length]);
            payload = &payload[length..];
            if length < usize::from(u8::MAX) {
                if !is_vorbis_header(packet_index, &packet) {
                    return false;
                }
                packet_index += 1;
                if packet_index == 3 {
                    return true;
                }
                packet.clear();
            }
        }
        expects_continuation = lacing.last() == Some(&u8::MAX);
        cursor = payload_end;
    }
    false
}

fn is_vorbis_header(index: usize, packet: &[u8]) -> bool {
    if packet.get(1..7) != Some(b"vorbis".as_slice()) {
        return false;
    }
    match index {
        0 => is_vorbis_identification_header(packet),
        1 => is_vorbis_comment_header(packet),
        // The setup framing bit is not necessarily bit 0 of the final byte:
        // Vorbis packs the complete setup structure LSB-first before it.
        2 => packet[0] == 5 && packet.len() > 7,
        _ => false,
    }
}

fn is_vorbis_identification_header(packet: &[u8]) -> bool {
    if packet.len() != 30 || packet[0] != 1 || packet.get(7..11) != Some([0; 4].as_slice()) {
        return false;
    }
    let channels = packet[11];
    let sample_rate = u32::from_le_bytes(packet[12..16].try_into().expect("Vorbis sample rate"));
    let block_sizes = packet[28];
    let short_block = block_sizes & 0x0f;
    let long_block = block_sizes >> 4;
    channels != 0
        && sample_rate != 0
        && (6..=13).contains(&short_block)
        && short_block <= long_block
        && long_block <= 13
        && packet[29] & 1 != 0
}

fn is_vorbis_comment_header(packet: &[u8]) -> bool {
    if packet[0] != 3 {
        return false;
    }
    let mut cursor = 7_usize;
    let Some(vendor_length) = read_u32(packet, &mut cursor) else {
        return false;
    };
    let Ok(vendor_length) = usize::try_from(vendor_length) else {
        return false;
    };
    let Some(after_vendor) = cursor.checked_add(vendor_length) else {
        return false;
    };
    if after_vendor > packet.len() {
        return false;
    }
    cursor = after_vendor;
    let Some(comment_count) = read_u32(packet, &mut cursor) else {
        return false;
    };
    let maximum_count = packet.len().saturating_sub(cursor).saturating_sub(1) / 4;
    if usize::try_from(comment_count)
        .ok()
        .is_none_or(|count| count > maximum_count)
    {
        return false;
    }
    for _ in 0..comment_count {
        let Some(comment_length) = read_u32(packet, &mut cursor) else {
            return false;
        };
        let Ok(comment_length) = usize::try_from(comment_length) else {
            return false;
        };
        let Some(after_comment) = cursor.checked_add(comment_length) else {
            return false;
        };
        if after_comment > packet.len() {
            return false;
        }
        cursor = after_comment;
    }
    packet.get(cursor) == Some(&1) && cursor + 1 == packet.len()
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    let value = u32::from_le_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn ogg_page_checksum(page: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0x04C1_1DB7;

    let mut checksum = 0_u32;
    for (index, byte) in page.iter().copied().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        checksum ^= u32::from(byte) << 24;
        for _ in 0..8 {
            checksum = if checksum & 0x8000_0000 != 0 {
                (checksum << 1) ^ POLYNOMIAL
            } else {
                checksum << 1
            };
        }
    }
    checksum
}
