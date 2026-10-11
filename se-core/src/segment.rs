//! Segments: immutable inverted indexes with a front-coded (prefix
//! compressed) term dictionary, compressed postings with skip pointers,
//! term positions, and a stored-fields document store.

use std::collections::BTreeMap;

use crate::analysis::Schema;
use crate::codec::{self, Posting, SkipEntry};

pub const SKIP_EVERY: usize = 8;
const DICT_BLOCK: usize = 16;

/// Front-coded term dictionary: sorted keys compressed with shared-prefix
/// coding, in blocks with a sparse index for binary search.
#[derive(Debug, Clone, Default)]
pub struct TermDict {
    pub bytes: Vec<u8>,
    /// (first key of block, byte offset of block start)
    pub index: Vec<(String, usize)>,
    pub len: usize,
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

impl TermDict {
    pub fn build(mut keys: Vec<String>) -> Self {
        keys.sort();
        keys.dedup();
        let mut bytes = Vec::new();
        let mut index = Vec::new();
        let mut prev = String::new();
        for (i, k) in keys.iter().enumerate() {
            if i % DICT_BLOCK == 0 {
                index.push((k.clone(), bytes.len()));
                // first key in block stored whole: prefix 0 relative to ""
                let kb = k.as_bytes();
                codec::encode_varint(0, &mut bytes);
                codec::encode_varint(kb.len() as u32, &mut bytes);
                bytes.extend_from_slice(kb);
                prev = k.clone();
            } else {
                let p = common_prefix(&prev, k);
                let suf = &k.as_bytes()[p..];
                codec::encode_varint(p as u32, &mut bytes);
                codec::encode_varint(suf.len() as u32, &mut bytes);
                bytes.extend_from_slice(suf);
                prev = k.clone();
            }
        }
        Self { bytes, index, len: keys.len() }
    }

    fn decode_block(&self, off: usize) -> Vec<(String, usize)> {
        // returns (key, offset_of_entry) pairs for one block
        let mut out = Vec::new();
        let mut pos = off;
        let mut prev = if out.is_empty() { String::new() } else { String::new() };
        // find block base: previous key state resets per block
        prev.clear();
        // first entry of block has prefix relative to ""
        let mut first = true;
        for _ in 0..DICT_BLOCK {
            if pos >= self.bytes.len() {
                break;
            }
            let entry_off = pos;
            let p = codec::decode_varint(&self.bytes, &mut pos).unwrap_or(0) as usize;
            let s = codec::decode_varint(&self.bytes, &mut pos).unwrap_or(0) as usize;
            if pos + s > self.bytes.len() {
                break;
            }
            let suf = &self.bytes[pos..pos + s];
            pos += s;
            let base: Vec<u8> =
                if first { Vec::new() } else { prev.as_bytes()[..p.min(prev.len())].to_vec() };
            let mut key = base;
            key.extend_from_slice(suf);
            let ks = String::from_utf8(key).unwrap_or_default();
            let _ = first;
            first = false;
            prev = ks.clone();
            out.push((ks, entry_off));
            if out.len() == DICT_BLOCK {
                break;
            }
        }
        out
    }

    /// All keys in sorted order (used by tests and prefix scans).
    pub fn all_keys(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.len);
        for (_, off) in &self.index.clone() {
            for (k, _) in self.decode_block(*off) {
                out.push(k);
            }
        }
        out.truncate(self.len);
        out
    }

    pub fn contains(&self, key: &str) -> bool {
        if self.index.is_empty() {
            return false;
        }
        // binary search blocks
        let mut lo = 0usize;
        let mut hi = self.index.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.index[mid].0.as_str() <= key {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        self.decode_block(self.index[lo].1).iter().any(|(k, _)| k == key)
    }

    /// All keys with the given prefix (prefix includes "field\x00" part).
    pub fn scan_prefix(&self, prefix: &str) -> Vec<String> {
        self.all_keys().into_iter().filter(|k| k.starts_with(prefix)).collect()
    }
}

/// Joined dict key for (field, term).
pub fn dict_key(field: &str, term: &str) -> String {
    format!("{field}\0{term}")
}

/// An immutable segment: postings keyed by (field, term) with local doc ids.
#[derive(Debug, Clone, Default)]
pub struct Segment {
    pub id: u64,
    /// global doc ids, position = local id
    pub docs: Vec<u32>,
    pub postings: BTreeMap<(String, String), Vec<Posting>>,
    pub compressed: BTreeMap<(String, String), Vec<u8>>,
    pub skips: BTreeMap<(String, String), Vec<SkipEntry>>,
    pub field_lens: Vec<BTreeMap<String, u32>>,
    pub stored: Vec<BTreeMap<String, String>>,
    pub dict: TermDict,
}

impl Segment {
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }
    pub fn term_count(&self) -> usize {
        self.postings.len()
    }
    pub fn postings_for(&self, field: &str, term: &str) -> Option<&[Posting]> {
        self.postings.get(&(field.to_string(), term.to_string())).map(|v| v.as_slice())
    }
    pub fn skips_for(&self, field: &str, term: &str) -> Option<&[SkipEntry]> {
        self.skips.get(&(field.to_string(), term.to_string())).map(|v| v.as_slice())
    }
    /// Terms of a field starting with `prefix` (uses the front-coded dict).
    pub fn prefix_terms(&self, field: &str, prefix: &str) -> Vec<String> {
        let want = format!("{field}\0{prefix}");
        self.dict
            .scan_prefix(&want)
            .into_iter()
            .map(|k| k[field.len() + 1..].to_string())
            .collect()
    }
    pub fn all_field_terms(&self, field: &str) -> Vec<String> {
        self.prefix_terms(field, "")
    }
    pub fn avg_field_len(&self, field: &str) -> f64 {
        if self.field_lens.is_empty() {
            return 0.0;
        }
        let sum: u64 =
            self.field_lens.iter().map(|m| m.get(field).copied().unwrap_or(0) as u64).sum();
        sum as f64 / self.field_lens.len() as f64
    }
    /// df of a term within this segment.
    pub fn df(&self, field: &str, term: &str) -> usize {
        self.postings_for(field, term).map(|p| p.len()).unwrap_or(0)
    }
    /// Encoded size in bytes (postings + dict + stored approx).
    pub fn encoded_size(&self) -> usize {
        self.compressed.values().map(|v| v.len()).sum::<usize>() + self.dict.bytes.len()
    }

    // ---- disk persistence (hand-rolled, varint framing) ----
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"SEG1");
        codec::encode_varint(self.id as u32, &mut out);
        codec::encode_varint(self.docs.len() as u32, &mut out);
        for d in &self.docs {
            codec::encode_varint(*d, &mut out);
        }
        codec::encode_varint(self.stored.len() as u32, &mut out);
        for doc in &self.stored {
            codec::encode_varint(doc.len() as u32, &mut out);
            for (k, v) in doc {
                codec::encode_varint(k.len() as u32, &mut out);
                out.extend_from_slice(k.as_bytes());
                codec::encode_varint(v.len() as u32, &mut out);
                out.extend_from_slice(v.as_bytes());
            }
        }
        codec::encode_varint(self.field_lens.len() as u32, &mut out);
        for lens in &self.field_lens {
            codec::encode_varint(lens.len() as u32, &mut out);
            for (k, v) in lens {
                codec::encode_varint(k.len() as u32, &mut out);
                out.extend_from_slice(k.as_bytes());
                codec::encode_varint(*v, &mut out);
            }
        }
        codec::encode_varint(self.postings.len() as u32, &mut out);
        let mut keys: Vec<(&(String, String), &Vec<Posting>)> = self.postings.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        for ((f, t), plist) in keys {
            codec::encode_varint(f.len() as u32, &mut out);
            out.extend_from_slice(f.as_bytes());
            codec::encode_varint(t.len() as u32, &mut out);
            out.extend_from_slice(t.as_bytes());
            let enc = codec::encode_postings(plist);
            codec::encode_varint(enc.len() as u32, &mut out);
            out.extend_from_slice(&enc);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Segment> {
        let read = |buf: &[u8], pos: &mut usize, n: usize| -> Option<Vec<u8>> {
            if *pos + n > buf.len() {
                return None;
            }
            let v = buf[*pos..*pos + n].to_vec();
            *pos += n;
            Some(v)
        };
        if buf.len() < 4 || &buf[0..4] != b"SEG1" {
            return None;
        }
        let mut pos = 4;
        let id = codec::decode_varint(buf, &mut pos)? as u64;
        let ndocs = codec::decode_varint(buf, &mut pos)? as usize;
        let mut docs = Vec::with_capacity(ndocs);
        for _ in 0..ndocs {
            docs.push(codec::decode_varint(buf, &mut pos)?);
        }
        let nstored = codec::decode_varint(buf, &mut pos)? as usize;
        let mut stored = Vec::with_capacity(nstored);
        for _ in 0..nstored {
            let nf = codec::decode_varint(buf, &mut pos)? as usize;
            let mut m = BTreeMap::new();
            for _ in 0..nf {
                let kl = codec::decode_varint(buf, &mut pos)? as usize;
                let kb = read(buf, &mut pos, kl)?;
                let vl = codec::decode_varint(buf, &mut pos)? as usize;
                let vb = read(buf, &mut pos, vl)?;
                m.insert(
                    String::from_utf8(kb).ok()?,
                    String::from_utf8(vb).ok()?,
                );
            }
            stored.push(m);
        }
        let nlens = codec::decode_varint(buf, &mut pos)? as usize;
        let mut field_lens = Vec::with_capacity(nlens);
        for _ in 0..nlens {
            let nf = codec::decode_varint(buf, &mut pos)? as usize;
            let mut m = BTreeMap::new();
            for _ in 0..nf {
                let kl = codec::decode_varint(buf, &mut pos)? as usize;
                let kb = read(buf, &mut pos, kl)?;
                let v = codec::decode_varint(buf, &mut pos)?;
                m.insert(String::from_utf8(kb).ok()?, v);
            }
            field_lens.push(m);
        }
        let nterms = codec::decode_varint(buf, &mut pos)? as usize;
        let mut postings: BTreeMap<(String, String), Vec<Posting>> = BTreeMap::new();
        let mut compressed = BTreeMap::new();
        for _ in 0..nterms {
            let fl = codec::decode_varint(buf, &mut pos)? as usize;
            let fb = read(buf, &mut pos, fl)?;
            let tl = codec::decode_varint(buf, &mut pos)? as usize;
            let tb = read(buf, &mut pos, tl)?;
            let el = codec::decode_varint(buf, &mut pos)? as usize;
            let eb = read(buf, &mut pos, el)?;
            let plist = codec::decode_postings(&eb)?;
            let key = (String::from_utf8(fb).ok()?, String::from_utf8(tb).ok()?);
            compressed.insert(key.clone(), eb);
            postings.insert(key, plist);
        }
        let mut seg = Segment {
            id,
            docs,
            postings,
            compressed,
            skips: BTreeMap::new(),
            field_lens,
            stored,
            dict: TermDict::default(),
        };
        seg.rebuild_aux();
        let _ = buf;
        Some(seg)
    }

    pub fn rebuild_aux(&mut self) {
        let keys: Vec<String> =
            self.postings.keys().map(|(f, t)| dict_key(f, t)).collect();
        self.dict = TermDict::build(keys);
        self.skips.clear();
        for (k, plist) in &self.postings {
            let enc = self.compressed.get(k).cloned().unwrap_or_default();
            self.skips.insert(k.clone(), codec::build_skips(plist, &enc, SKIP_EVERY));
        }
    }
}

/// Collects documents in memory, then freezes into an immutable Segment.
#[derive(Debug, Default)]
pub struct SegmentWriter {
    pub docs: Vec<u32>,
    pub stored: Vec<BTreeMap<String, String>>,
    pub field_lens: Vec<BTreeMap<String, u32>>,
    // (field, term) -> local doc idx -> (freq, positions)
    pub inv: BTreeMap<(String, String), BTreeMap<u32, (u32, Vec<u32>)>>,
}

impl SegmentWriter {
    pub fn add_doc(
        &mut self,
        global_id: u32,
        fields: &BTreeMap<String, String>,
        schema: &Schema,
    ) {
        let local = self.docs.len() as u32;
        self.docs.push(global_id);
        self.stored.push(fields.clone());
        let mut lens = BTreeMap::new();
        for (f, text) in fields {
            let terms = schema.field_terms(f, text);
            lens.insert(f.clone(), terms.values().map(|(c, _)| *c).sum::<u32>());
            for (tok, (freq, positions)) in terms {
                self.inv.entry((f.clone(), tok)).or_default().insert(local, (freq, positions));
            }
        }
        self.field_lens.push(lens);
    }

    pub fn finish(&self, id: u64) -> Segment {
        let mut postings = BTreeMap::new();
        let mut compressed = BTreeMap::new();
        for (k, m) in &self.inv {
            let mut plist: Vec<Posting> = m
                .iter()
                .map(|(&doc, (freq, positions))| Posting {
                    doc,
                    freq: *freq,
                    positions: positions.clone(),
                })
                .collect();
            plist.sort_by_key(|p| p.doc);
            compressed.insert(k.clone(), codec::encode_postings(&plist));
            postings.insert(k.clone(), plist);
        }
        let mut seg = Segment {
            id,
            docs: self.docs.clone(),
            postings,
            compressed,
            skips: BTreeMap::new(),
            field_lens: self.field_lens.clone(),
            stored: self.stored.clone(),
            dict: TermDict::default(),
        };
        seg.rebuild_aux();
        seg
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

/// Merge segments into one: union postings, remap local ids, keep stored.
pub fn merge_segments(id: u64, segs: &[Segment]) -> Segment {
    let mut w = SegmentWriter::default();
    // global order kept sorted for delta compression wins
    let mut all: Vec<(u32, BTreeMap<String, String>, BTreeMap<String, u32>)> = Vec::new();
    for s in segs {
        for (i, gid) in s.docs.iter().enumerate() {
            all.push((*gid, s.stored[i].clone(), s.field_lens[i].clone()));
        }
    }
    all.sort_by_key(|(g, _, _)| *g);
    // rebuild inverted index from stored docs is wasteful; remap directly
    let mut inv: BTreeMap<(String, String), BTreeMap<u32, (u32, Vec<u32>)>> = BTreeMap::new();
    for (new_local, (gid, stored, lens)) in all.iter().enumerate() {
        let nl = new_local as u32;
        w.docs.push(*gid);
        w.stored.push(stored.clone());
        w.field_lens.push(lens.clone());
        let _ = nl;
    }
    for s in segs {
        for ((f, t), plist) in &s.postings {
            let e = inv.entry((f.clone(), t.clone())).or_default();
            for p in plist {
                let gid = s.docs[p.doc as usize];
                let nl = w.docs.iter().position(|&g| g == gid).unwrap() as u32;
                e.insert(nl, (p.freq, p.positions.clone()));
            }
        }
    }
    w.inv = inv;
    w.finish(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(title: &str, body: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("title".to_string(), title.to_string()),
            ("body".to_string(), body.to_string()),
        ])
    }

    #[test]
    fn dict_prefix_compression_roundtrip() {
        let keys: Vec<String> = (0..100).map(|i| format!("body\0term{i:03}")).collect();
        let d = TermDict::build(keys.clone());
        assert_eq!(d.all_keys(), keys);
        for k in &keys {
            assert!(d.contains(k), "missing {k}");
        }
        assert!(!d.contains("body\0zzz"));
        assert_eq!(d.scan_prefix("body\0term00").len(), 10);
        // prefix compression saves bytes vs raw
        let raw: usize = keys.iter().map(|k| k.len()).sum();
        assert!(d.bytes.len() < raw, "{} < {raw}", d.bytes.len());
    }

    #[test]
    fn segment_write_read_positions() {
        let schema = Schema::default();
        let mut w = SegmentWriter::default();
        w.add_doc(0, &doc("Rust search", "fast rust search engine"), &schema);
        w.add_doc(1, &doc("Cooking", "rust recipes for dinner"), &schema);
        let seg = w.finish(1);
        // "rust" stemmed to "rust" in body of both docs
        let p = seg.postings_for("body", "rust").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(seg.df("body", "rust"), 2);
        assert_eq!(seg.postings_for("body", "nope"), None);
        // prefix scan
        assert!(seg.prefix_terms("body", "eng").contains(&"engin".to_string()));
    }

    #[test]
    fn segment_disk_roundtrip() {
        let schema = Schema::default();
        let mut w = SegmentWriter::default();
        w.add_doc(5, &doc("Hi", "hello world hello"), &schema);
        let seg = w.finish(7);
        let bytes = seg.encode();
        let back = Segment::decode(&bytes).unwrap();
        assert_eq!(back.docs, seg.docs);
        assert_eq!(back.stored, seg.stored);
        assert_eq!(
            back.postings_for("body", "hello").unwrap(),
            seg.postings_for("body", "hello").unwrap()
        );
    }

    #[test]
    fn merge_union() {
        let schema = Schema::default();
        let mut w1 = SegmentWriter::default();
        w1.add_doc(0, &doc("a", "alpha beta"), &schema);
        let mut w2 = SegmentWriter::default();
        w2.add_doc(1, &doc("b", "beta gamma"), &schema);
        let m = merge_segments(3, &[w1.finish(1), w2.finish(2)]);
        assert_eq!(m.doc_count(), 2);
        assert_eq!(m.df("body", "beta"), 2);
        assert_eq!(m.df("body", "alpha"), 1);
    }
}
