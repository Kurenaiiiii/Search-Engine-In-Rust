//! Index management: in-memory write buffer, write-ahead log, tombstone
//! deletes/updates, tiered merge policy with background merging, and
//! snapshot readers for consistent concurrent search.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

use crate::analysis::Schema;
use crate::codec;
use crate::query;
use crate::search::{self, Hit, SearchOptions, Snapshot};
use crate::segment::{merge_segments, Segment, SegmentWriter};

const FLUSH_THRESHOLD: usize = 1000;
const MERGE_THRESHOLD: usize = 4;

/// Simulated crash points for recovery tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FailAt {
    FlushWrite,
    FlushCommit,
    MergeWrite,
    MergeCommit,
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub live_docs: usize,
    pub segments: usize,
    pub buffered_docs: usize,
    pub postings_bytes: usize,
    pub dict_bytes: usize,
    pub next_id: u32,
}

struct Inner {
    schema: Schema,
    segments: Vec<Arc<Segment>>,
    buffer: Vec<(u32, BTreeMap<String, String>)>,
    live: HashSet<u32>,
    stored: Arc<BTreeMap<u32, BTreeMap<String, String>>>,
    next_id: u32,
    next_seg: u64,
    wal: File,
    wal_applied: u64,
}

pub struct Index {
    dir: PathBuf,
    inner: RwLock<Inner>,
    fail: Mutex<Option<FailAt>>,
    commit: Mutex<()>,
    merges: Mutex<Vec<JoinHandle<()>>>,
}

impl Index {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        fs::create_dir_all(dir.join("segments"))?;
        let (segments, next_id, next_seg, wal_applied) = load_manifest(dir)?;
        let wal_path = dir.join("wal.log");
        let mut wal_len = 0u64;
        if wal_path.exists() {
            wal_len = fs::metadata(&wal_path)?.len();
        }
        let wal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal_path)?;
        let idx = Self {
            dir: dir.to_path_buf(),
            inner: RwLock::new(Inner {
                schema: Schema::default(),
                segments,
                buffer: Vec::new(),
                live: HashSet::new(),
                stored: Arc::new(BTreeMap::new()),
                next_id,
                next_seg,
                wal,
                wal_applied,
            }),
            fail: Mutex::new(None),
            commit: Mutex::new(()),
            merges: Mutex::new(Vec::new()),
        };
        // live/stored for flushed segments unknown from manifest alone:
        // rebuild by replaying WAL from wal_applied, plus scan segment docs.
        {
            let mut inner = idx.inner.write().unwrap();
            // mark all segment docs live first; WAL replay fixes dels
            let mut seg_docs = Vec::new();
            for s in &inner.segments {
                for (i, gid) in s.docs.iter().enumerate() {
                    seg_docs.push((*gid, s.stored[i].clone()));
                }
            }
            for (gid, fields) in seg_docs {
                inner.live.insert(gid);
                Arc::make_mut(&mut inner.stored).insert(gid, fields);
            }
            // next_id must exceed any seg doc
            let max_seg = inner
                .segments
                .iter()
                .flat_map(|s| s.docs.iter())
                .max()
                .copied();
            if let Some(m) = max_seg {
                inner.next_id = inner.next_id.max(m + 1);
            }
        }
        idx.replay_wal(wal_applied, wal_len)?;
        // drop fully-deleted... keep as is; search filters by live set
        Ok(idx)
    }

    fn replay_wal(&self, from: u64, end: u64) -> std::io::Result<()> {
        if end <= from {
            return Ok(());
        }
        let mut buf = Vec::new();
        {
            let mut f = File::open(self.dir.join("wal.log"))?;
            f.read_to_end(&mut buf)?;
        }
        let mut pos = from as usize;
        let mut rebound_buffer: Vec<(u32, BTreeMap<String, String>)> = Vec::new();
        // collect existing buffer? fresh open has empty buffer
        while pos < end as usize && pos < buf.len() {
            let reclen = codec::decode_varint(&buf, &mut pos).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "bad wal record len")
            })? as usize;
            let rec = buf.get(pos..pos + reclen).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "truncated wal record")
            })?;
            pos += reclen;
            let mut inner = self.inner.write().unwrap();
            apply_record(&mut inner, rec, &mut rebound_buffer);
        }
        let mut inner = self.inner.write().unwrap();
        inner.buffer = rebound_buffer;
        inner.wal_applied = end;
        Ok(())
    }

    pub fn set_schema(&self, schema: Schema) {
        self.inner.write().unwrap().schema = schema;
    }

    pub fn set_fail_point(&self, f: Option<FailAt>) {
        *self.fail.lock().unwrap() = f;
    }

    fn fail_here(&self, at: FailAt) -> std::io::Result<()> {
        if self.fail.lock().unwrap().as_ref() == Some(&at) {
            return Err(std::io::Error::other(format!("injected failure at {at:?}")));
        }
        Ok(())
    }

    /// Add a document; returns its global id.
    pub fn add(&self, fields: BTreeMap<String, String>) -> std::io::Result<u32> {
        let mut payload = vec![1u8];
        let id = {
            // id assignment, WAL append and state update under one lock so
            // concurrent writers never reuse an id.
            let mut inner = self.inner.write().unwrap();
            let id = inner.next_id;
            codec::encode_varint(id, &mut payload);
            codec::encode_varint(fields.len() as u32, &mut payload);
            for (k, v) in &fields {
                codec::encode_varint(k.len() as u32, &mut payload);
                payload.extend_from_slice(k.as_bytes());
                codec::encode_varint(v.len() as u32, &mut payload);
                payload.extend_from_slice(v.as_bytes());
            }
            let mut rec = Vec::new();
            codec::encode_varint(payload.len() as u32, &mut rec);
            rec.extend_from_slice(&payload);
            inner.wal.write_all(&rec)?;
            inner.next_id = id + 1;
            inner.live.insert(id);
            Arc::make_mut(&mut inner.stored).insert(id, fields.clone());
            inner.buffer.push((id, fields));
            id
        };
        if self.inner.read().unwrap().buffer.len() >= FLUSH_THRESHOLD {
            let _ = self.flush();
        }
        Ok(id)
    }

    pub fn delete(&self, id: u32) -> std::io::Result<bool> {
        let mut inner = self.inner.write().unwrap();
        if !inner.live.contains(&id) {
            return Ok(false);
        }
        let mut payload = vec![2u8];
        codec::encode_varint(id, &mut payload);
        let mut rec = Vec::new();
        codec::encode_varint(payload.len() as u32, &mut rec);
        rec.extend_from_slice(&payload);
        inner.wal.write_all(&rec)?;
        inner.live.remove(&id);
        Arc::make_mut(&mut inner.stored).remove(&id);
        inner.buffer.retain(|(g, _)| *g != id);
        Ok(true)
    }

    /// Update = tombstone old id, insert new id.
    pub fn update(
        &self,
        id: u32,
        fields: BTreeMap<String, String>,
    ) -> std::io::Result<Option<u32>> {
        if !self.inner.read().unwrap().live.contains(&id) {
            return Ok(None);
        }
        self.delete(id)?;
        Ok(Some(self.add(fields)?))
    }

    /// Flush the write buffer into a new immutable segment.
    pub fn flush(&self) -> std::io::Result<()> {
        let _commit = self.commit.lock().unwrap();
        let (docs, schema) = {
            let inner = self.inner.read().unwrap();
            if inner.buffer.is_empty() {
                return Ok(());
            }
            (inner.buffer.clone(), inner.schema.clone())
        };
        self.fail_here(FailAt::FlushWrite)?;
        let seg_id = self.inner.read().unwrap().next_seg;
        let mut w = SegmentWriter::default();
        for (gid, fields) in &docs {
            w.add_doc(*gid, fields, &schema);
        }
        let seg = w.finish(seg_id);
        let bytes = seg.encode();
        let path = self.dir.join("segments").join(format!("seg_{seg_id}.dat"));
        {
            let mut f = File::create(&path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        self.fail_here(FailAt::FlushCommit)?;
        {
            let mut inner = self.inner.write().unwrap();
            inner.segments.push(Arc::new(seg));
            let flushed: HashSet<u32> = docs.iter().map(|(g, _)| *g).collect();
            inner.buffer.retain(|(g, _)| !flushed.contains(g));
            inner.next_seg += 1;
            inner.wal.flush()?;
            inner.wal.sync_all()?;
            let wal_len = fs::metadata(self.dir.join("wal.log"))?.len();
            inner.wal_applied = wal_len;
        }
        self.write_manifest()?;
        Ok(())
    }

    fn write_manifest(&self) -> std::io::Result<()> {
        let inner = self.inner.read().unwrap();
        let tmp = self.dir.join("MANIFEST.tmp");
        let mut s = format!(
            "{}\n{}\n{}\n",
            inner.next_id, inner.next_seg, inner.wal_applied
        );
        for seg in &inner.segments {
            s.push_str(&format!("{}\n", seg.id));
        }
        fs::write(&tmp, s)?;
        fs::rename(&tmp, self.dir.join("MANIFEST"))?;
        Ok(())
    }

    /// Tiered merge: merge the 2 smallest segments into one. Bounded memory
    /// (only two segments are copied), unlike [`Index::merge_all`].
    /// Returns true when a merge happened.
    pub fn merge_tier(&self) -> std::io::Result<bool> {
        let _commit = self.commit.lock().unwrap();
        self.fail_here(FailAt::MergeWrite)?;
        let mut ids: Vec<(usize, u64)> = {
            let inner = self.inner.read().unwrap();
            if inner.segments.len() < 2 {
                return Ok(false);
            }
            let mut v: Vec<(usize, u64)> = inner
                .segments
                .iter()
                .map(|s| s.doc_count() as u64)
                .enumerate()
                .collect();
            v.sort_by_key(|&(_, n)| n);
            v.truncate(2.min(v.len()));
            v
        };
        ids.sort_by_key(|&(i, _)| i);
        let picked: Vec<usize> = ids.into_iter().map(|(i, _)| i).collect();
        let (snapshot_segs, live) = {
            let inner = self.inner.read().unwrap();
            let segs: Vec<Segment> = picked
                .iter()
                .map(|&i| (*inner.segments[i]).clone())
                .collect();
            (segs, inner.live.clone())
        };
        let filtered: Vec<Segment> = snapshot_segs
            .iter()
            .map(|s| retain_live(s, &live))
            .collect();
        let seg_id = self.inner.read().unwrap().next_seg;
        let merged = merge_segments(seg_id, filtered);
        let bytes = merged.encode();
        let path = self.dir.join("segments").join(format!("seg_{seg_id}.dat"));
        {
            let mut f = File::create(&path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        self.fail_here(FailAt::MergeCommit)?;
        {
            let mut inner = self.inner.write().unwrap();
            let old_ids: HashSet<u64> = snapshot_segs.iter().map(|s| s.id).collect();
            inner.segments.retain(|s| !old_ids.contains(&s.id));
            inner.segments.push(Arc::new(merged));
            inner.next_seg += 1;
        }
        self.write_manifest()?;
        for s in &snapshot_segs {
            let _ = fs::remove_file(self.dir.join("segments").join(format!("seg_{}.dat", s.id)));
        }
        Ok(true)
    }

    /// Merge every segment into one (used by tests and small indexes; for
    /// large indexes prefer [`Index::merge_tier`], which bounds memory).
    pub fn merge_all(&self) -> std::io::Result<bool> {
        let _commit = self.commit.lock().unwrap();
        self.fail_here(FailAt::MergeWrite)?;
        let snapshot_segs: Vec<Arc<Segment>> = self.inner.read().unwrap().segments.clone();
        if snapshot_segs.len() < 2 {
            return Ok(false);
        }
        // drop dead docs before merging
        let live = self.inner.read().unwrap().live.clone();
        let filtered: Vec<Segment> = snapshot_segs
            .iter()
            .map(|s| retain_live(s, &live))
            .collect();
        let seg_id = self.inner.read().unwrap().next_seg;
        let merged = merge_segments(seg_id, filtered);
        let bytes = merged.encode();
        let path = self.dir.join("segments").join(format!("seg_{seg_id}.dat"));
        {
            let mut f = File::create(&path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        self.fail_here(FailAt::MergeCommit)?;
        {
            let mut inner = self.inner.write().unwrap();
            let old_ids: HashSet<u64> = snapshot_segs.iter().map(|s| s.id).collect();
            inner.segments.retain(|s| !old_ids.contains(&s.id));
            inner.segments.push(Arc::new(merged));
            inner.next_seg += 1;
        }
        self.write_manifest()?;
        // remove old files only after manifest commit
        for s in &snapshot_segs {
            let _ = fs::remove_file(self.dir.join("segments").join(format!("seg_{}.dat", s.id)));
        }
        Ok(true)
    }

    /// Tiered policy: merge the smallest tier when >= MERGE_THRESHOLD
    /// segments exist. Runs in a background thread.
    pub fn spawn_merge(self: &std::sync::Arc<Self>) -> bool {
        let n = self.inner.read().unwrap().segments.len();
        if n < MERGE_THRESHOLD {
            return false;
        }
        let me = std::sync::Arc::clone(self);
        let h = std::thread::spawn(move || {
            let _ = me.merge_tier();
        });
        self.merges.lock().unwrap().push(h);
        true
    }

    /// Flush + opportunistically kick off a background merge.
    pub fn flush_and_maybe_merge(self: &std::sync::Arc<Self>) -> std::io::Result<()> {
        self.flush()?;
        self.spawn_merge();
        Ok(())
    }

    pub fn wait_merges(&self) {
        let mut handles = self.merges.lock().unwrap();
        while let Some(h) = handles.pop() {
            let _ = h.join();
        }
    }

    /// Consistent snapshot for search while writes continue. The unflushed
    /// write buffer is frozen into an ephemeral segment so fresh writes are
    /// searchable immediately.
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.read().unwrap();
        let mut segments = inner.segments.clone();
        if !inner.buffer.is_empty() {
            let mut w = SegmentWriter::default();
            for (gid, fields) in &inner.buffer {
                w.add_doc(*gid, fields, &inner.schema);
            }
            segments.push(Arc::new(w.finish(u64::MAX)));
        }
        Snapshot {
            segments,
            live: inner.live.clone(),
            stored: Arc::clone(&inner.stored),
            schema: inner.schema.clone(),
        }
    }

    pub fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<Hit>, String> {
        let q = query::parse(query)?;
        Ok(search::search(&self.snapshot(), &q, opts))
    }

    pub fn stored(&self, id: u32) -> Option<BTreeMap<String, String>> {
        self.inner.read().unwrap().stored.get(&id).cloned()
    }

    pub fn stats(&self) -> Stats {
        let inner = self.inner.read().unwrap();
        Stats {
            live_docs: inner.live.len(),
            segments: inner.segments.len(),
            buffered_docs: inner.buffer.len(),
            postings_bytes: inner.segments.iter().map(|s| s.encoded_size()).sum(),
            dict_bytes: inner.segments.iter().map(|s| s.dict.bytes.len()).sum(),
            next_id: inner.next_id,
        }
    }

    pub fn schema(&self) -> Schema {
        self.inner.read().unwrap().schema.clone()
    }
}

fn apply_record(inner: &mut Inner, rec: &[u8], rebound: &mut Vec<(u32, BTreeMap<String, String>)>) {
    if rec.is_empty() {
        return;
    }
    let mut pos = 1;
    match rec[0] {
        1 => {
            let id = codec::decode_varint(rec, &mut pos).unwrap_or(0);
            let nf = codec::decode_varint(rec, &mut pos).unwrap_or(0) as usize;
            let mut m = BTreeMap::new();
            for _ in 0..nf {
                let kl = codec::decode_varint(rec, &mut pos).unwrap_or(0) as usize;
                let kb = rec.get(pos..pos + kl).unwrap_or(&[]).to_vec();
                pos += kl;
                let vl = codec::decode_varint(rec, &mut pos).unwrap_or(0) as usize;
                let vb = rec.get(pos..pos + vl).unwrap_or(&[]).to_vec();
                pos += vl;
                m.insert(
                    String::from_utf8(kb).unwrap_or_default(),
                    String::from_utf8(vb).unwrap_or_default(),
                );
            }
            inner.next_id = inner.next_id.max(id + 1);
            inner.live.insert(id);
            Arc::make_mut(&mut inner.stored).insert(id, m.clone());
            // if doc already in a segment (replay after partial flush), keep
            // it in buffer too — harmless duplicate filtered by live set...
            // avoid double-index: only buffer if not in any segment
            let in_seg = inner.segments.iter().any(|s| s.docs.contains(&id));
            if !in_seg {
                rebound.push((id, m));
            }
        }
        2 => {
            let id = codec::decode_varint(rec, &mut pos).unwrap_or(0);
            inner.live.remove(&id);
            Arc::make_mut(&mut inner.stored).remove(&id);
            rebound.retain(|(g, _)| *g != id);
        }
        _ => {}
    }
}

fn load_manifest(dir: &Path) -> std::io::Result<(Vec<Arc<Segment>>, u32, u64, u64)> {
    let path = dir.join("MANIFEST");
    if !path.exists() {
        return Ok((Vec::new(), 0, 0, 0));
    }
    let text = fs::read_to_string(&path)?;
    let mut lines = text.lines();
    let next_id: u32 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let next_seg: u64 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let wal_applied: u64 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let mut segs: Vec<Arc<Segment>> = Vec::new();
    for l in lines {
        let id: u64 = match l.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let p = dir.join("segments").join(format!("seg_{id}.dat"));
        if let Ok(bytes) = fs::read(&p) {
            if let Some(seg) = Segment::decode(&bytes) {
                segs.push(Arc::new(seg));
            }
        }
    }
    Ok((segs, next_id, next_seg, wal_applied))
}

/// Strip dead (tombstoned) docs out of a segment copy.
fn retain_live(seg: &Segment, live: &HashSet<u32>) -> Segment {
    let keep: Vec<usize> = seg
        .docs
        .iter()
        .enumerate()
        .filter(|(_, g)| live.contains(g))
        .map(|(i, _)| i)
        .collect();
    let remap: HashMap<u32, u32> = keep
        .iter()
        .enumerate()
        .map(|(new, &old)| (old as u32, new as u32))
        .collect();
    let mut out = Segment {
        id: seg.id,
        docs: keep.iter().map(|&i| seg.docs[i]).collect(),
        postings: BTreeMap::new(),
        compressed: BTreeMap::new(),
        skips: BTreeMap::new(),
        field_lens: keep.iter().map(|&i| seg.field_lens[i].clone()).collect(),
        stored: keep.iter().map(|&i| seg.stored[i].clone()).collect(),
        dict: Default::default(),
    };
    for (k, plist) in &seg.postings {
        let mut np = Vec::new();
        for p in plist {
            if let Some(&nl) = remap.get(&p.doc) {
                let mut q = p.clone();
                q.doc = nl;
                np.push(q);
            }
        }
        if !np.is_empty() {
            out.compressed
                .insert(k.clone(), codec::encode_postings(&np));
            out.postings.insert(k.clone(), np);
        }
    }
    out.rebuild_aux();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchOptions;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("se_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn sample() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("title".to_string(), "Rust search".to_string()),
            ("body".to_string(), "a fast search engine".to_string()),
        ])
    }

    #[test]
    fn wal_recovery_no_flush() {
        let dir = tmpdir("wal");
        {
            let idx = Index::open(&dir).unwrap();
            idx.add(sample()).unwrap();
            idx.add(sample()).unwrap();
            // no flush: docs live only in WAL + memory
        }
        let idx = Index::open(&dir).unwrap();
        assert_eq!(idx.stats().live_docs, 2);
        // buffer is part of the snapshot: searchable even before flush
        let hits = idx.search("search", &SearchOptions::top(10)).unwrap();
        assert_eq!(hits.len(), 2);
        idx.flush().unwrap();
        let hits = idx.search("search", &SearchOptions::top(10)).unwrap();
        assert_eq!(hits.len(), 2);
        let _ = hits;
    }

    #[test]
    fn delete_update_tombstones() {
        let dir = tmpdir("del");
        let idx = Index::open(&dir).unwrap();
        let id = idx.add(sample()).unwrap();
        idx.flush().unwrap();
        assert!(idx.delete(id).unwrap());
        idx.flush().unwrap();
        assert_eq!(
            idx.search("search", &SearchOptions::top(10)).unwrap().len(),
            0
        );
        let id2 = idx.add(sample()).unwrap();
        idx.update(id2, BTreeMap::from([("title".into(), "zzz".into())]))
            .unwrap();
        idx.flush().unwrap();
        assert_eq!(idx.search("zzz", &SearchOptions::top(10)).unwrap().len(), 1);
        assert_eq!(
            idx.search("search", &SearchOptions::top(10)).unwrap().len(),
            0
        );
    }

    #[test]
    fn merge_equivalence() {
        let dir = tmpdir("merge");
        let idx = Index::open(&dir).unwrap();
        for i in 0..8 {
            let mut d = sample();
            d.insert("body".into(), format!("doc number {i} search engine test"));
            idx.add(d).unwrap();
            idx.flush().unwrap();
        }
        let before = idx.search("search", &SearchOptions::top(10)).unwrap();
        assert!(idx.merge_all().unwrap());
        let after = idx.search("search", &SearchOptions::top(10)).unwrap();
        assert_eq!(before, after);
        assert_eq!(idx.stats().segments, 1);
    }

    #[test]
    fn crash_at_flush_points_recovers() {
        for point in [FailAt::FlushWrite, FailAt::FlushCommit] {
            let dir = tmpdir(&format!("crash{point:?}"));
            {
                let idx = Index::open(&dir).unwrap();
                idx.add(sample()).unwrap();
                idx.set_fail_point(Some(point));
                assert!(idx.flush().is_err());
                idx.set_fail_point(None);
                idx.add(sample()).unwrap();
            }
            let idx = Index::open(&dir).unwrap();
            // both committed docs recovered (WAL replay)
            assert_eq!(idx.stats().live_docs, 2);
            idx.flush().unwrap();
            assert_eq!(
                idx.search("search", &SearchOptions::top(10)).unwrap().len(),
                2
            );
        }
    }

    #[test]
    fn crash_at_merge_points_recovers() {
        for point in [FailAt::MergeWrite, FailAt::MergeCommit] {
            let dir = tmpdir(&format!("mcrash{point:?}"));
            let idx = Index::open(&dir).unwrap();
            for _ in 0..3 {
                idx.add(sample()).unwrap();
                idx.flush().unwrap();
            }
            let before = idx.search("search", &SearchOptions::top(10)).unwrap();
            idx.set_fail_point(Some(point));
            assert!(idx.merge_all().is_err());
            idx.set_fail_point(None);
            drop(idx);
            let idx = Index::open(&dir).unwrap();
            let after = idx.search("search", &SearchOptions::top(10)).unwrap();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn concurrent_readers_writers() {
        use std::sync::Arc;
        let dir = tmpdir("conc");
        let idx = Arc::new(Index::open(&dir).unwrap());
        let mut writers = Vec::new();
        for t in 0..4 {
            let me = Arc::clone(&idx);
            writers.push(std::thread::spawn(move || {
                for i in 0..25 {
                    let mut d = sample();
                    d.insert("body".into(), format!("thread {t} doc {i} search engine"));
                    me.add(d).unwrap();
                }
            }));
        }
        let mut readers = Vec::new();
        for _ in 0..4 {
            let me = Arc::clone(&idx);
            readers.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    let snap = me.snapshot();
                    let _ = search::search(
                        &snap,
                        &query::parse("search").unwrap(),
                        &SearchOptions::top(5),
                    );
                }
            }));
        }
        for h in writers {
            h.join().unwrap();
        }
        for h in readers {
            h.join().unwrap();
        }
        idx.flush().unwrap();
        assert_eq!(idx.stats().live_docs, 100);
    }
}
