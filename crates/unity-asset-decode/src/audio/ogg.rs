//! Strict allocation-free Ogg container validation shared by audio paths.

pub(crate) fn final_ogg_granule(bytes: &[u8]) -> Option<u64> {
    let mut cursor = 0_usize;
    let mut saw_page = false;
    let mut expected_sequence = 0_u32;
    let mut saw_eos = false;
    let mut final_granule = None;
    let mut serial = None;
    let mut expects_continuation = false;

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
        if (flags & 0x01 != 0) != expects_continuation {
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
        let page = bytes.get(cursor..page_end)?;
        let declared_checksum =
            u32::from_le_bytes(header[22..26].try_into().expect("Ogg checksum"));
        if ogg_page_checksum(page) != declared_checksum {
            return None;
        }

        if !lacing.is_empty() {
            expects_continuation = lacing.last() == Some(&u8::MAX);
        }
        saw_eos = flags & 0x04 != 0;
        if saw_eos {
            if expects_continuation {
                return None;
            }
            final_granule = Some(u64::from_le_bytes(
                header[6..14].try_into().expect("Ogg granule position"),
            ));
        }
        cursor = page_end;
        saw_page = true;
        expected_sequence = expected_sequence.wrapping_add(1);
    }

    (saw_page && saw_eos).then_some(final_granule).flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    const SERIAL: u32 = 0x1234_5678;

    #[test]
    fn complete_stream_validates_through_eos() {
        let stream = valid_stream();
        assert_eq!(final_ogg_granule(&stream), Some(42));
    }

    #[test]
    fn crc_correct_stream_rejects_missing_continuation() {
        let mut stream = page(0x02, 0, 0, &[255], &[0_u8; 255]);
        stream.extend(page(0x04, 1, 42, &[1], &[0]));
        assert_eq!(final_ogg_granule(&stream), None);
    }

    #[test]
    fn crc_correct_stream_rejects_unexpected_continuation() {
        let mut stream = page(0x02, 0, 0, &[1], &[0]);
        stream.extend(page(0x05, 1, 42, &[1], &[0]));
        assert_eq!(final_ogg_granule(&stream), None);
    }

    #[test]
    fn eos_cannot_leave_a_packet_unfinished() {
        let mut stream = page(0x02, 0, 0, &[255], &[0_u8; 255]);
        stream.extend(page(0x05, 1, 42, &[255], &[0_u8; 255]));
        assert_eq!(final_ogg_granule(&stream), None);
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let mut stream = valid_stream();
        stream[22] ^= 0xff;
        assert_eq!(final_ogg_granule(&stream), None);
    }

    fn valid_stream() -> Vec<u8> {
        let mut stream = page(0x02, 0, 0, &[1], &[0]);
        stream.extend(page(0x04, 1, 42, &[1], &[0]));
        stream
    }

    fn page(flags: u8, sequence: u32, granule: u64, lacing: &[u8], payload: &[u8]) -> Vec<u8> {
        assert_eq!(
            lacing
                .iter()
                .map(|value| usize::from(*value))
                .sum::<usize>(),
            payload.len()
        );
        let mut page = vec![0_u8; 27 + lacing.len()];
        page[..4].copy_from_slice(b"OggS");
        page[5] = flags;
        page[6..14].copy_from_slice(&granule.to_le_bytes());
        page[14..18].copy_from_slice(&SERIAL.to_le_bytes());
        page[18..22].copy_from_slice(&sequence.to_le_bytes());
        page[26] = u8::try_from(lacing.len()).unwrap();
        page[27..].copy_from_slice(lacing);
        page.extend_from_slice(payload);
        let checksum = ogg_page_checksum(&page);
        page[22..26].copy_from_slice(&checksum.to_le_bytes());
        page
    }
}
