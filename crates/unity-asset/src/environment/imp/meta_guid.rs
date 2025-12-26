use super::*;

pub(crate) fn parse_guid_32_hex(raw: &str) -> Option<[u8; 16]> {
    let s = raw.trim();
    if s.len() != 32 {
        return None;
    }

    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = s.as_bytes().get(i * 2).copied()?;
        let lo = s.as_bytes().get(i * 2 + 1).copied()?;
        out[i] = (hex_nibble(hi)? << 4) | hex_nibble(lo)?;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn read_guid_from_meta_text(text: &str) -> Option<[u8; 16]> {
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("guid:") {
            let guid = rest.trim();
            if let Some(bytes) = parse_guid_32_hex(guid) {
                return Some(bytes);
            }
        }
    }
    None
}

impl Environment {
    pub(crate) fn index_meta_guid_path(&self, meta_path: &Path) {
        let Ok(text) = std::fs::read_to_string(meta_path) else {
            return;
        };
        let Some(guid) = read_guid_from_meta_text(&text) else {
            return;
        };

        // `foo.ext.meta` -> `foo.ext`
        let mut asset_path = meta_path.to_path_buf();
        if asset_path.extension().and_then(|e| e.to_str()) == Some("meta") {
            asset_path.set_extension("");
        }

        match self.meta_guid_cache.write() {
            Ok(mut cache) => {
                cache.entry(guid).or_insert(asset_path);
            }
            Err(e) => {
                e.into_inner().entry(guid).or_insert(asset_path);
            }
        }
    }

    pub(crate) fn asset_path_for_guid(&self, guid: [u8; 16]) -> Option<PathBuf> {
        match self.meta_guid_cache.read() {
            Ok(cache) => cache.get(&guid).cloned(),
            Err(e) => e.into_inner().get(&guid).cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_guid_32_hex_accepts_lowercase_hex() {
        let guid = "0123456789abcdef0123456789abcdef";
        let out = parse_guid_32_hex(guid).expect("parse");
        assert_eq!(
            out,
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
    }

    #[test]
    fn read_guid_from_meta_text_finds_guid_line() {
        let text = "fileFormatVersion: 2\n guid: 0123456789abcdef0123456789abcdef\n";
        let out = read_guid_from_meta_text(text).expect("guid");
        assert_eq!(out[0], 0x01);
    }
}
