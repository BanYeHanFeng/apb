//! Incremental (rsync-style) transfer helpers.
//!
//! `push` and `pull` never send a file that the other side already has: the
//! receiving side hashes its local copy and, when the content differs, sends a
//! list of block signatures back.  The sending side then slides a rolling
//! checksum over its own file, reuses every block the receiver already holds
//! (`Copy`) and only emits the bytes that really changed (`Data`).
//!
//! A signature is 20 bytes: a 4-byte rolling checksum plus the first 16 bytes
//! of the block's SHA-256.  Block sizes grow with the file so that the
//! signature list stays a small fraction of the file (well under 0.25%).

use crate::util::set_mode;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Weak checksum (4 bytes) + truncated strong hash (16 bytes).
pub const SIG_LEN: usize = 20;
pub const STRONG_LEN: usize = 16;

/// Target payload size for a `Data` or `FileSigs` frame.
pub const CHUNK: usize = 16 * 1024;

/// Signatures per `FileSigs` frame (a frame carries whole signatures only).
pub const SIGS_PER_FRAME: usize = CHUNK / SIG_LEN;
pub const SIG_BATCH: usize = SIGS_PER_FRAME * SIG_LEN;

/// How much of the source is buffered while scanning for matching blocks.
const READ_CHUNK: usize = 1 << 20;

/// Files smaller than this are always sent whole: the signature list and the
/// copy instructions would cost about as much as the file itself.
const DELTA_MIN_SIZE: u64 = 4096;

/// Pick the block size for a base file.  Larger files get larger blocks so the
/// signature list stays small: 8 KiB up to 8 MiB, then 32/128/512 KiB.
pub fn choose_block_size(base_size: u64) -> u32 {
    const KIB: u64 = 1024;
    let bs = if base_size > 512 * 1024 * KIB {
        512 * KIB
    } else if base_size > 64 * 1024 * KIB {
        128 * KIB
    } else if base_size > 8 * 1024 * KIB {
        32 * KIB
    } else {
        8 * KIB
    };
    // Always leave at least one full block so the base can be reused at all.
    bs.min(base_size.max(1)).max(1) as u32
}

/// Is a delta worth its round trip?  Sending the signatures is only cheaper
/// than sending the file when the file is not tiny and the signature list is a
/// small fraction of what a full transfer would cost.
pub fn worth_delta(base_size: u64, source_size: u64, block_count: u64) -> bool {
    if base_size < DELTA_MIN_SIZE || source_size < DELTA_MIN_SIZE || block_count == 0 {
        return false;
    }
    let sig_bytes = block_count * SIG_LEN as u64;
    sig_bytes * 8 <= source_size && sig_bytes * 8 <= base_size
}

/// The rsync rolling checksum: `a` is the byte sum, `b` the position weighted
/// sum, both modulo 2^16.
#[derive(Clone, Copy)]
pub struct Rolling {
    a: u32,
    b: u32,
    n: u32,
}

impl Rolling {
    pub fn new(data: &[u8]) -> Self {
        let n = data.len() as u32;
        let mut a = 0u32;
        let mut b = 0u32;
        for (i, &d) in data.iter().enumerate() {
            a = a.wrapping_add(d as u32);
            b = b.wrapping_add((data.len() - i) as u32 * d as u32);
        }
        Self {
            a: a & 0xffff,
            b: b & 0xffff,
            n,
        }
    }

    /// Slide the window by one byte: `out` leaves, `inb` enters.
    pub fn roll(&mut self, out: u8, inb: u8) {
        self.a = (self.a.wrapping_sub(out as u32).wrapping_add(inb as u32)) & 0xffff;
        self.b = (self
            .b
            .wrapping_sub(self.n.wrapping_mul(out as u32))
            .wrapping_add(self.a))
            & 0xffff;
    }

    pub fn digest(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

pub fn weak_checksum(data: &[u8]) -> u32 {
    Rolling::new(data).digest()
}

/// Block signatures of one base file, in block order.
pub struct SigSet {
    pub block_size: u32,
    pub block_count: u64,
    weak: Vec<u32>,
    strong: Vec<u8>,
    /// SHA-256 of the whole base file; a match with the source means the file
    /// does not have to be transferred at all.
    pub file_sha: [u8; 32],
    pub size: u64,
}

impl SigSet {
    /// Read `path` once, hashing it as a whole and per block.
    pub fn scan(path: &Path, block_size: u32) -> io::Result<SigSet> {
        let mut f = File::open(path)?;
        let size = f.metadata()?.len();
        let bs = block_size.max(1) as usize;
        let mut buf = vec![0u8; bs];
        let mut hasher = Sha256::new();
        let mut weak = Vec::new();
        let mut strong = Vec::new();
        loop {
            let n = read_full(&mut f, &mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            if n == bs {
                weak.push(weak_checksum(&buf[..n]));
                let h = Sha256::digest(&buf[..n]);
                strong.extend_from_slice(&h[..STRONG_LEN]);
            }
            if n < bs {
                break;
            }
        }
        let mut file_sha = [0u8; 32];
        file_sha.copy_from_slice(&hasher.finalize());
        Ok(SigSet {
            block_size,
            block_count: weak.len() as u64,
            weak,
            strong,
            file_sha,
            size,
        })
    }

    /// Serialize as `(u32 big-endian weak, 16-byte strong)` pairs.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.weak.len() * SIG_LEN);
        for (i, w) in self.weak.iter().enumerate() {
            out.extend_from_slice(&w.to_be_bytes());
            let s = &self.strong[i * STRONG_LEN..(i + 1) * STRONG_LEN];
            out.extend_from_slice(s);
        }
        out
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Sender-side view of the receiver's block signatures.
pub struct SigIndex {
    pub block_size: u32,
    pub block_count: u64,
    strong: Vec<u8>,
    by_weak: HashMap<u32, Vec<u32>>,
}

impl SigIndex {
    /// `blob` is the concatenation of every `FileSigs` frame of one file.
    pub fn new(block_size: u32, block_count: u64, blob: &[u8]) -> Result<SigIndex, String> {
        if block_size == 0 {
            return Err("bad block size 0".into());
        }
        let want = usize::try_from(block_count)
            .ok()
            .and_then(|n| n.checked_mul(SIG_LEN))
            .ok_or_else(|| format!("impossible block count {block_count}"))?;
        if blob.len() != want {
            return Err(format!(
                "signature list is {} bytes, expected {}",
                blob.len(),
                want
            ));
        }
        let mut by_weak: HashMap<u32, Vec<u32>> = HashMap::with_capacity(block_count as usize);
        let mut strong = Vec::with_capacity(block_count as usize * STRONG_LEN);
        for i in 0..block_count as usize {
            let at = i * SIG_LEN;
            let mut w = [0u8; 4];
            w.copy_from_slice(&blob[at..at + 4]);
            by_weak
                .entry(u32::from_be_bytes(w))
                .or_default()
                .push(i as u32);
            strong.extend_from_slice(&blob[at + 4..at + SIG_LEN]);
        }
        Ok(SigIndex {
            block_size,
            block_count,
            strong,
            by_weak,
        })
    }

    fn strong_of(&self, block: usize) -> &[u8] {
        &self.strong[block * STRONG_LEN..(block + 1) * STRONG_LEN]
    }
}

/// One instruction for the receiver, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Reuse `len` bytes of the base file that start at `start`.
    Copy { start: u64, len: u64 },
    /// Literal bytes that follow the previous instruction.
    Data(Vec<u8>),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeltaStats {
    /// Literal bytes that had to be sent.
    pub literal: u64,
    /// Bytes reused from the receiver's copy.
    pub copied: u64,
}

/// Sliding-window match of `src` against the receiver's blocks.  Literal bytes
/// are batched into `CHUNK`-sized `Op::Data` values and consecutive matched
/// blocks are merged into one `Op::Copy`, so a file that is mostly unchanged
/// costs a single copy instruction.
pub fn produce_delta<R: Read>(
    src: &mut R,
    idx: &SigIndex,
    emit: &mut impl FnMut(Op) -> io::Result<()>,
) -> io::Result<DeltaStats> {
    let bs = idx.block_size as usize;
    let bs64 = idx.block_size as u64;
    let cap = READ_CHUNK + bs;
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    let mut pos = 0usize;
    let mut lit = 0usize;
    let mut eof = false;
    let mut roll: Option<Rolling> = None;
    let mut pending: Vec<u8> = Vec::with_capacity(CHUNK);
    let mut copy: Option<(u64, u64)> = None;
    let mut stats = DeltaStats::default();

    // Emit the pending copy.  Literal bytes that come *before* it in stream
    // order are buffered in `pending` and must be flushed first.
    fn emit_copy(
        copy: &mut Option<(u64, u64)>,
        pending: &mut Vec<u8>,
        emit: &mut impl FnMut(Op) -> io::Result<()>,
        stats: &mut DeltaStats,
    ) -> io::Result<()> {
        let Some((start, len)) = copy.take() else {
            return Ok(());
        };
        if !pending.is_empty() {
            let out = std::mem::replace(pending, Vec::with_capacity(CHUNK));
            emit(Op::Data(out))?;
        }
        stats.copied += len;
        emit(Op::Copy { start, len })
    }

    // Hand `bytes` to the batcher, emitting full `Data` frames as they fill up.
    fn append(
        mut bytes: &[u8],
        pending: &mut Vec<u8>,
        emit: &mut impl FnMut(Op) -> io::Result<()>,
        stats: &mut DeltaStats,
    ) -> io::Result<()> {
        stats.literal += bytes.len() as u64;
        while !bytes.is_empty() {
            let take = (CHUNK - pending.len()).min(bytes.len());
            pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if pending.len() == CHUNK {
                let out = std::mem::replace(pending, Vec::with_capacity(CHUNK));
                emit(Op::Data(out))?;
            }
        }
        Ok(())
    }

    // Close the copy and turn everything currently literal into `Data` frames.
    fn flush_literal(
        copy: &mut Option<(u64, u64)>,
        bytes: &[u8],
        pending: &mut Vec<u8>,
        emit: &mut impl FnMut(Op) -> io::Result<()>,
        stats: &mut DeltaStats,
    ) -> io::Result<()> {
        emit_copy(copy, pending, emit, stats)?;
        append(bytes, pending, emit, stats)
    }

    loop {
        if pos + bs > filled {
            if eof {
                break;
            }
            // Drop everything already known to be literal, then fetch more.
            if lit < pos {
                flush_literal(&mut copy, &buf[lit..pos], &mut pending, emit, &mut stats)?;
            }
            if pos > 0 {
                buf.copy_within(pos..filled, 0);
                filled -= pos;
                pos = 0;
            }
            lit = 0;
            roll = None;
            let n = src.read(&mut buf[filled..])?;
            if n == 0 {
                eof = true;
            }
            filled += n;
            continue;
        }

        let mut matched: Option<u32> = None;
        {
            let window = &buf[pos..pos + bs];
            let r = match roll {
                Some(r) => r,
                None => {
                    let r = Rolling::new(window);
                    roll = Some(r);
                    r
                }
            };
            if let Some(cands) = idx.by_weak.get(&r.digest()) {
                let strong = Sha256::digest(window);
                let strong = &strong[..STRONG_LEN];
                // Try to continue the open copy run first: with repetitive
                // content every block shares one weak checksum, and the first
                // candidate would otherwise split the file into one Copy frame
                // per block.
                if let Some((start, len)) = copy {
                    let want = (start + len) / bs64;
                    if want < idx.block_count
                        && cands.contains(&(want as u32))
                        && idx.strong_of(want as usize) == strong
                    {
                        matched = Some(want as u32);
                    }
                }
                if matched.is_none() {
                    for &i in cands {
                        if idx.strong_of(i as usize) == strong {
                            matched = Some(i);
                            break;
                        }
                    }
                }
            }
        }

        match matched {
            Some(i) => {
                let start = i as u64 * bs64;
                if lit < pos {
                    // Literal gap: the previous copy and these bytes go first.
                    flush_literal(&mut copy, &buf[lit..pos], &mut pending, emit, &mut stats)?;
                    copy = Some((start, bs64));
                } else {
                    match copy {
                        Some((s, l)) if s + l == start => copy = Some((s, l + bs64)),
                        _ => {
                            emit_copy(&mut copy, &mut pending, emit, &mut stats)?;
                            copy = Some((start, bs64));
                        }
                    }
                }
                pos += bs;
                lit = pos;
                roll = None;
            }
            None => {
                if pos + 1 + bs <= filled {
                    if let Some(mut r) = roll {
                        r.roll(buf[pos], buf[pos + bs]);
                        roll = Some(r);
                    }
                    pos += 1;
                } else if eof {
                    break;
                } else {
                    if lit < pos {
                        flush_literal(&mut copy, &buf[lit..pos], &mut pending, emit, &mut stats)?;
                    }
                    if pos > 0 {
                        buf.copy_within(pos..filled, 0);
                        filled -= pos;
                        pos = 0;
                    }
                    lit = 0;
                    roll = None;
                    let n = src.read(&mut buf[filled..])?;
                    if n == 0 {
                        eof = true;
                    }
                    filled += n;
                }
            }
        }
    }

    if lit < filled {
        flush_literal(&mut copy, &buf[lit..filled], &mut pending, emit, &mut stats)?;
    }
    // A copy that is still open comes after every buffered literal.
    emit_copy(&mut copy, &mut pending, emit, &mut stats)?;
    if !pending.is_empty() {
        emit(Op::Data(pending))?;
    }
    Ok(stats)
}

/// Receiver-side writer: builds the new file in a temporary file next to the
/// destination, reading reused ranges out of the previous copy.
pub struct FileWriter {
    tmp: PathBuf,
    dest: PathBuf,
    file: File,
    hasher: Sha256,
    received: u64,
    expected: u64,
    base: Option<File>,
    copybuf: Vec<u8>,
}

impl FileWriter {
    /// `base` is the receiver's existing copy (if any) used for `Copy` ops.
    pub fn create(
        dest: &Path,
        base: Option<&Path>,
        tmp: PathBuf,
        expected: u64,
    ) -> io::Result<FileWriter> {
        let _ = fs::remove_file(&tmp);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let base = base.and_then(|p| File::open(p).ok());
        Ok(FileWriter {
            tmp,
            dest: dest.to_path_buf(),
            file,
            hasher: Sha256::new(),
            received: 0,
            expected,
            base,
            copybuf: vec![0u8; 64 * 1024],
        })
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    fn overrun(&self) -> io::Result<()> {
        if self.received > self.expected {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "received more bytes than expected ({}) for {}",
                    self.expected,
                    self.dest.display()
                ),
            ))
        } else {
            Ok(())
        }
    }

    pub fn write_literal(&mut self, data: &[u8]) -> io::Result<()> {
        self.received += data.len() as u64;
        self.overrun()?;
        self.hasher.update(data);
        self.file.write_all(data)
    }

    /// Reuse `len` bytes of the base file starting at `start`.
    pub fn copy_from_base(&mut self, start: u64, len: u64) -> io::Result<()> {
        // Destructure so the base file and the output file are borrowed
        // independently of each other.
        let FileWriter {
            base,
            copybuf,
            hasher,
            file,
            received,
            expected,
            dest,
            ..
        } = self;
        let base = base.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "copy instruction without a base file for {}",
                    dest.display()
                ),
            )
        })?;
        base.seek(SeekFrom::Start(start))?;
        let mut left = len;
        while left > 0 {
            let want = left.min(copybuf.len() as u64) as usize;
            base.read_exact(&mut copybuf[..want])?;
            *received += want as u64;
            if *received > *expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "received more bytes than expected ({}) for {}",
                        expected,
                        dest.display()
                    ),
                ));
            }
            hasher.update(&copybuf[..want]);
            file.write_all(&copybuf[..want])?;
            left -= want as u64;
        }
        Ok(())
    }

    /// Verify size and content hash, then atomically publish the file.
    pub fn finish(self, expected_sha: &[u8; 32], mode: u32) -> Result<u64, String> {
        if self.received != self.expected {
            let got = self.received;
            let want = self.expected;
            self.abort();
            return Err(format!(
                "size mismatch for {}: expected {}, got {}",
                self.dest.display(),
                want,
                got
            ));
        }
        if *expected_sha != [0u8; 32] {
            let got = self.hasher.clone().finalize();
            if got[..] != expected_sha[..] {
                self.abort();
                return Err(format!("sha256 mismatch for {}", self.dest.display()));
            }
        }
        if let Err(e) = self.file.sync_all() {
            let msg = format!("sync {}: {e}", self.tmp.display());
            self.abort();
            return Err(msg);
        }
        // Permissions belong on the file *before* it becomes visible, and
        // before `self.file` is moved out below.
        if let Err(e) = set_mode(&self.tmp, mode) {
            let msg = format!("chmod {}: {e}", self.tmp.display());
            self.abort();
            return Err(msg);
        }
        drop(self.file);
        if let Err(e) = fs::rename(&self.tmp, &self.dest) {
            let msg = format!("rename to {}: {e}", self.dest.display());
            let _ = fs::remove_file(&self.tmp);
            return Err(msg);
        }
        Ok(self.received)
    }

    /// Remove the temporary file.  Taking `&self` keeps it usable from
    /// `finish`, where `self` is already owned.
    pub fn abort(&self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        while out.len() < len {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Rebuild `delta` on top of `base` the way the receiver does.
    fn apply(base: &[u8], ops: &[Op]) -> Vec<u8> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                Op::Copy { start, len } => {
                    let s = *start as usize;
                    out.extend_from_slice(&base[s..s + *len as usize]);
                }
                Op::Data(d) => out.extend_from_slice(d),
            }
        }
        out
    }

    fn delta(base: &[u8], source: &[u8]) -> (Vec<Op>, DeltaStats) {
        let bs = choose_block_size(base.len() as u64);
        let base_file = write_temp(base);
        let set = SigSet::scan(&base_file, bs).unwrap();
        let _ = fs::remove_file(&base_file);
        let blob = set.encode();
        let idx = SigIndex::new(set.block_size, set.block_count, &blob).unwrap();
        let mut ops = Vec::new();
        let stats = produce_delta(&mut Cursor::new(source.to_vec()), &idx, &mut |op| {
            ops.push(op);
            Ok(())
        })
        .unwrap();
        (ops, stats)
    }

    fn sha256(data: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(data));
        out
    }

    /// Unique temp file per call: the test harness runs tests in parallel.
    fn write_temp(data: &[u8]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "apb-delta-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            data.len()
        ));
        fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn rolling_matches_recomputation() {
        let data = pattern(4096, 7);
        let mut r = Rolling::new(&data[0..64]);
        for i in 0..data.len() - 64 {
            assert_eq!(r.digest(), weak_checksum(&data[i..i + 64]), "at {i}");
            r.roll(data[i], data[i + 64]);
        }
    }

    #[test]
    fn identical_file_is_fully_reused() {
        let base = pattern(300_000, 1);
        let bs = choose_block_size(base.len() as u64) as u64;
        let (ops, stats) = delta(&base, &base);
        assert_eq!(apply(&base, &ops), base);
        // Only the block-aligned prefix is indexed, so at most one block of the
        // tail can be literal.
        assert!(stats.literal <= bs, "literal {}", stats.literal);
        assert_eq!(stats.copied + stats.literal, base.len() as u64);
    }

    #[test]
    fn consecutive_blocks_merge_into_one_copy() {
        let base = pattern(400_000, 21);
        let (ops, stats) = delta(&base, &base);
        assert_eq!(apply(&base, &ops), base);
        let copies = ops.iter().filter(|o| matches!(o, Op::Copy { .. })).count();
        assert_eq!(copies, 1, "expected a single merged copy, got {ops:?}");
        assert!(stats.copied > 380_000, "copied {}", stats.copied);
    }

    #[test]
    fn small_edit_only_sends_changed_block() {
        let base = pattern(300_000, 2);
        let bs = choose_block_size(base.len() as u64) as u64;
        let mut source = base.clone();
        source[150_000] ^= 0xff;
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        assert!(stats.literal <= 2 * bs, "literal {}", stats.literal);
        assert!(stats.copied > 250_000, "copied {}", stats.copied);
    }

    #[test]
    fn insertion_shifts_blocks() {
        let base = pattern(400_000, 3);
        let bs = choose_block_size(base.len() as u64) as u64;
        let mut source = base[..100_000].to_vec();
        source.extend_from_slice(b"inserted-bytes");
        source.extend_from_slice(&base[100_000..]);
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        // A 15-byte insertion: everything before it still matches, everything
        // after is re-found once the sliding window re-aligns (at most ~2 blocks
        // of literal data around the shift).
        assert!(
            stats.literal <= 4 * bs,
            "literal {} for a 15-byte insertion",
            stats.literal
        );
    }

    #[test]
    fn append_only_sends_the_tail() {
        let base = pattern(200_000, 4);
        let bs = choose_block_size(base.len() as u64) as u64;
        let mut source = base.clone();
        source.extend_from_slice(&pattern(700, 5));
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        assert!(stats.literal <= bs + 700, "literal {}", stats.literal);
    }

    #[test]
    fn deletion_is_handled() {
        let base = pattern(250_000, 6);
        let bs = choose_block_size(base.len() as u64) as u64;
        let mut source = base[..50_000].to_vec();
        source.extend_from_slice(&base[120_000..]);
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        assert!(stats.literal <= 4 * bs, "literal {}", stats.literal);
    }

    #[test]
    fn repetitive_content_still_merges_into_few_copies() {
        // Every block of an all-zero file shares one weak checksum; the matcher
        // must still extend the open run instead of switching to block 0.
        let base = vec![0u8; 1 << 20];
        let mut source = base.clone();
        source[500_000] ^= 0xff;
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        let copies = ops.iter().filter(|o| matches!(o, Op::Copy { .. })).count();
        assert!(copies <= 4, "expected a few merged copies, got {copies}");
        assert!(stats.literal < 16 * 1024, "literal {}", stats.literal);
    }

    #[test]
    fn unrelated_content_is_sent_as_literal() {
        let base = pattern(200_000, 7);
        let source = pattern(200_000, 8);
        let (ops, stats) = delta(&base, &source);
        assert_eq!(apply(&base, &ops), source);
        assert!(
            stats.literal as usize >= source.len(),
            "unexpected reuse of unrelated data"
        );
    }

    #[test]
    fn empty_and_tiny_files_round_trip() {
        let base: Vec<u8> = Vec::new();
        let (ops, stats) = delta(&base, b"hello");
        assert_eq!(apply(&base, &ops), b"hello");
        assert_eq!(stats.copied, 0);

        let (ops, _) = delta(b"abc", b"abc");
        assert_eq!(apply(b"abc", &ops), b"abc");
    }

    #[test]
    fn writer_rebuilds_and_verifies() {
        let dir = std::env::temp_dir().join(format!("apb-delta-writer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base");
        let dest = dir.join("dest");
        let base = pattern(120_000, 9);
        fs::write(&base_path, &base).unwrap();
        let mut source = base.clone();
        source[60_000] ^= 0x55;

        let (ops, _) = delta(&base, &source);
        let mut w = FileWriter::create(
            &dest,
            Some(&base_path),
            dir.join(".dest.tmp"),
            source.len() as u64,
        )
        .unwrap();
        for op in &ops {
            match op {
                Op::Copy { start, len } => w.copy_from_base(*start, *len).unwrap(),
                Op::Data(d) => w.write_literal(d).unwrap(),
            }
        }
        w.finish(&sha256(&source), 0o644).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), source);

        // A wrong hash must be rejected and must not publish the file.
        let dest2 = dir.join("dest2");
        let mut w = FileWriter::create(&dest2, None, dir.join(".dest2.tmp"), 3).unwrap();
        w.write_literal(b"abc").unwrap();
        assert!(w.finish(&[9u8; 32], 0o644).is_err());
        assert!(!dest2.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_size_and_worth_delta() {
        assert_eq!(choose_block_size(1024), 1024);
        assert_eq!(choose_block_size(100 * 1024), 8 * 1024);
        assert_eq!(choose_block_size(100 * 1024 * 1024), 128 * 1024);
        assert!(worth_delta(1 << 20, 1 << 20, 128));
        // A tiny source cannot pay for the signature list of a huge base.
        assert!(!worth_delta(1 << 30, 1024, 1 << 30 >> 19));
        assert!(!worth_delta(10, 1024, 0));
    }
}
