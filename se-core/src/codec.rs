//! Posting-list compression: variable-byte (varint) codes, delta encoding,
//! and skip pointers for fast list intersection.

/// Encode a u32 as variable-byte: 7 payload bits per byte, high bit = stop.
pub fn encode_varint(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            b |= 0x80;
            out.push(b);
            break;
        }
        out.push(b);
    }
}

/// Decode one varint at `pos` (advanced past the value). Returns None on
/// truncated input.
pub fn decode_varint(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut shift = 0u32;
    let mut val = 0u32;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        val |= ((b & 0x7F) as u32) << shift;
        shift += 7;
        if b & 0x80 != 0 {
            return Some(val);
        }
        if shift > 35 {
            return None;
        }
    }
}

/// Delta-encode a sorted doc-id list into bytes (gaps + varint).
pub fn encode_doc_deltas(sorted_ids: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0u32;
    for &id in sorted_ids {
        encode_varint(id - prev, &mut out);
        prev = id;
    }
    out
}

pub fn decode_doc_deltas(buf: &[u8]) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut prev = 0u32;
    while pos < buf.len() {
        let gap = decode_varint(buf, &mut pos)?;
        prev += gap;
        out.push(prev);
    }
    Some(out)
}

/// One posting inside a list.
#[derive(Debug, Clone, PartialEq)]
pub struct Posting {
    pub doc: u32,
    pub freq: u32,
    pub positions: Vec<u32>,
}

/// Encode a full posting list: per posting `gap(varint) freq(varint)
/// npos(varint) posgap*(varint)`. Position gaps are deltas from previous
/// position with base 0 handled as gap = pos+1 to keep values > 0... in
/// fact positions may start at 0, so we store pos-prev+1 minus 1 scheme:
/// gap0 = pos0+1, gapN = posN-posN-1+1, decode subtracts 1.
pub fn encode_postings(postings: &[Posting]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev_doc = 0u32;
    for p in postings {
        encode_varint(p.doc - prev_doc, &mut out);
        prev_doc = p.doc;
        encode_varint(p.freq, &mut out);
        encode_varint(p.positions.len() as u32, &mut out);
        let mut prev_pos = 0u32;
        for (i, &pos) in p.positions.iter().enumerate() {
            let gap = if i == 0 { pos + 1 } else { pos - prev_pos + 1 };
            encode_varint(gap, &mut out);
            prev_pos = pos;
        }
    }
    out
}

pub fn decode_postings(buf: &[u8]) -> Option<Vec<Posting>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut prev_doc = 0u32;
    while pos < buf.len() {
        let gap = decode_varint(buf, &mut pos)?;
        prev_doc += gap;
        let freq = decode_varint(buf, &mut pos)?;
        let npos = decode_varint(buf, &mut pos)? as usize;
        let mut positions = Vec::with_capacity(npos);
        let mut prev_pos = 0u32;
        for i in 0..npos {
            let g = decode_varint(buf, &mut pos)?;
            let p = if i == 0 { g - 1 } else { prev_pos + g - 1 };
            positions.push(p);
            prev_pos = p;
        }
        out.push(Posting {
            doc: prev_doc,
            freq,
            positions,
        });
    }
    Some(out)
}

/// Skip entry: posting index -> (byte offset in encoded list, doc id).
#[derive(Debug, Clone, PartialEq)]
pub struct SkipEntry {
    pub index: usize,
    pub byte_offset: usize,
    pub doc: u32,
}

/// Build skip entries every `every` postings, recording byte offsets by
/// re-encoding incrementally.
pub fn build_skips(postings: &[Posting], encoded: &[u8], every: usize) -> Vec<SkipEntry> {
    let _ = encoded;
    // Recompute offsets by walking the encoding.
    let mut skips = Vec::new();
    let mut pos = 0usize;
    let mut prev_doc = 0u32;
    for (i, p) in postings.iter().enumerate() {
        if i.is_multiple_of(every) {
            skips.push(SkipEntry {
                index: i,
                byte_offset: pos,
                doc: p.doc,
            });
        }
        // advance pos past this posting: re-encode single posting and skip
        let mut tmp = Vec::new();
        encode_varint(p.doc - prev_doc, &mut tmp);
        encode_varint(p.freq, &mut tmp);
        encode_varint(p.positions.len() as u32, &mut tmp);
        let mut prev_pos = 0u32;
        for (j, &pp) in p.positions.iter().enumerate() {
            let gap = if j == 0 { pp + 1 } else { pp - prev_pos + 1 };
            encode_varint(gap, &mut tmp);
            prev_pos = pp;
        }
        prev_doc = p.doc;
        pos += tmp.len();
    }
    skips
}

/// Build skip entries by walking already-encoded bytes: decodes only varint
/// boundaries (no postings materialized, no re-encoding). Used by
/// rebuild_aux so flush/merge never pay for a second encode pass.
pub fn build_skips_from_encoded(encoded: &[u8], every: usize) -> Vec<SkipEntry> {
    let mut skips = Vec::new();
    let mut pos = 0usize;
    let mut doc = 0u32;
    let mut i = 0usize;
    while pos < encoded.len() {
        let entry_off = pos;
        let gap = match decode_varint(encoded, &mut pos) {
            Some(g) => g,
            None => break,
        };
        doc += gap;
        if i.is_multiple_of(every) {
            skips.push(SkipEntry {
                index: i,
                byte_offset: entry_off,
                doc,
            });
        }
        i += 1;
        // skip freq + npos, then npos position gaps (decode-and-discard)
        let (_freq, npos) = match (
            decode_varint(encoded, &mut pos),
            decode_varint(encoded, &mut pos),
        ) {
            (Some(f), Some(n)) => (f, n as usize),
            _ => break,
        };
        for _ in 0..npos {
            if decode_varint(encoded, &mut pos).is_none() {
                break;
            }
        }
    }
    skips
}

/// Intersect two sorted doc-id lists using skip-assisted gallop on the
/// longer list. Returns sorted intersection.
pub fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::new();
    for &d in small {
        if big.binary_search(&d).is_ok() {
            out.push(d);
        }
    }
    out
}

/// Skip-to on a decoded id list using skip entries: returns the first index
/// with doc >= target.
pub fn skip_to(ids: &[u32], skips: &[SkipEntry], target: u32, hint: usize) -> usize {
    let mut i = hint;
    for s in skips.iter().skip_while(|s| s.index <= hint) {
        if s.doc < target {
            i = s.index;
        } else {
            break;
        }
    }
    while i < ids.len() && ids[i] < target {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0, 1, 127, 128, 255, 300, 16384, u32::MAX] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf);
            let mut p = 0;
            assert_eq!(decode_varint(&buf, &mut p), Some(v));
            assert_eq!(p, buf.len());
        }
    }

    #[test]
    fn varint_known_bytes() {
        // 300 = 0b10_0101100 -> bytes [0x2C, 0x82]
        let mut buf = Vec::new();
        encode_varint(300, &mut buf);
        assert_eq!(buf, vec![0x2C, 0x82]);
    }

    #[test]
    fn delta_roundtrip() {
        let ids = vec![1, 2, 3, 100, 1000, 1_000_000];
        let enc = encode_doc_deltas(&ids);
        assert_eq!(decode_doc_deltas(&enc).unwrap(), ids);
        // gaps compress: 6 ids in far fewer than 24 bytes
        assert!(enc.len() < 24);
    }

    #[test]
    fn postings_roundtrip() {
        let ps = vec![
            Posting {
                doc: 1,
                freq: 2,
                positions: vec![0, 5],
            },
            Posting {
                doc: 7,
                freq: 1,
                positions: vec![3],
            },
            Posting {
                doc: 100,
                freq: 3,
                positions: vec![0, 1, 9],
            },
        ];
        let enc = encode_postings(&ps);
        assert_eq!(decode_postings(&enc).unwrap(), ps);
    }

    #[test]
    fn skips_cover_list() {
        let ps: Vec<Posting> = (0..20)
            .map(|i| Posting {
                doc: i,
                freq: 1,
                positions: vec![0],
            })
            .collect();
        let enc = encode_postings(&ps);
        let skips = build_skips(&ps, &enc, 8);
        assert_eq!(skips.len(), 3);
        assert_eq!(
            skips[0],
            SkipEntry {
                index: 0,
                byte_offset: 0,
                doc: 0
            }
        );
        // every skip entry must point at the right posting: doc ids decoded
        // from the full list match the recorded absolute doc
        let ids: Vec<u32> = ps.iter().map(|p| p.doc).collect();
        for s in &skips {
            assert_eq!(ids[s.index], s.doc);
        }
        // decoding from a skip offset yields the suffix list whose first gap
        // is relative to the previous doc (delta coding), so the suffix head
        // plus the preceding doc reconstructs the absolute id
        let suffix = decode_postings(&enc[skips[1].byte_offset..]).unwrap();
        assert_eq!(suffix[0].doc, skips[1].doc - ids[skips[1].index - 1]);
        let ids: Vec<u32> = ps.iter().map(|p| p.doc).collect();
        assert_eq!(skip_to(&ids, &skips, 9, 0), 9);
    }

    #[test]
    fn skips_from_encoded_match() {
        // skip tables walked from bytes must equal the re-encoded ones
        let mut rng_like = 1u32;
        for len in [0, 1, 7, 8, 9, 20, 100] {
            let mut ps = Vec::new();
            for _ in 0..len {
                rng_like = rng_like.wrapping_mul(1664525).wrapping_add(1013904223);
                let doc = rng_like % 1000;
                ps.push(Posting {
                    doc,
                    freq: 1 + rng_like % 5,
                    positions: vec![0, 1 + rng_like % 50],
                });
            }
            ps.sort_by_key(|p| p.doc);
            ps.dedup_by_key(|p| p.doc);
            let enc = encode_postings(&ps);
            assert_eq!(
                build_skips_from_encoded(&enc, 8),
                build_skips(&ps, &enc, 8),
                "len={len}"
            );
        }
    }

    #[test]
    fn intersect_basic() {
        assert_eq!(intersect(&[1, 2, 3], &[2, 3, 4]), vec![2, 3]);
        assert!(intersect(&[1], &[2]).is_empty());
    }
}
