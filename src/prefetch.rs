//! Network-filesystem prefetch warmer.
//!
//! File probes parse through an `mmap`, so every byte a demuxer touches is
//! served by a page fault. Locally that's microseconds; over SMB/NFS each fault
//! region that isn't cached becomes a *synchronous* network round-trip with
//! almost none of the pipelined read-ahead a sequential `read()` would get. A
//! track scan that touches a few scattered regions then costs many RTTs — the
//! "same file is 20 ms local / 700 ms on the NAS" gap.
//!
//! This module warms the byte ranges the parser is about to read with pipelined
//! positioned reads before the demux runs (`warm_metadata`): the front metadata
//! window per container, plus an MP4 `moov` box wherever it sits (commonly at
//! the tail, the one region a head window could never cover). The warm only
//! affects *timing* — parsing still runs against the mmap and nothing is copied
//! into the report, so the result is byte-identical with or without it.
//!
//! It is gated to genuinely remote volumes (`is_remote`, decided from the open
//! handle / mount table at no extra network cost) so audioprobe's millisecond
//! local path is never touched: warming an 8 MiB head on every local probe
//! would be a regression for the fast case the tool exists to serve.
//!
//! This is a leaner port of hdrprobe's `prefetch`: audioprobe has no `--full`
//! whole-file walk (so no look-ahead `Frontier`), no Blu-ray ISO branch, and
//! samples codec frames inline within the metadata head walk (so no separate
//! sampled-chunk warm). What remains — remote detection and metadata-range
//! warming — is what applies to a bounded head probe.

use std::fs::File;
use std::path::Path;

/// Generic head window for a remote file whose front working set can't be
/// pinned to an exact extent: Matroska (EBML header + Tracks + the leading
/// clusters the deep sampler reads) and unrecognized formats. TS/M2TS gets a
/// larger window (`TS_HEAD_WARM`) since its audio PES rides deeper into the
/// stream; MP4 and the simple header formats get smaller ones — their real
/// regions are warmed by exact extent (`moov`) or sit inside `SIMPLE_HEAD_WARM`.
const HEAD_WARM: usize = 8 << 20; // 8 MiB

/// Head window for the formats whose parameters live in a small front header:
/// MP4 (`ftyp` and incidental front boxes — the `moov` is warmed by extent),
/// FLAC STREAMINFO, WAV `fmt `, Ogg identification headers, and raw elementary
/// streams (their head reads are 64 KiB–1 MiB).
const SIMPLE_HEAD_WARM: usize = 1 << 20; // 1 MiB

/// Head window for a transport stream, capped so a large `--limit-mb` doesn't
/// pre-stream more of a NAS file than the scan is likely to read (the scan
/// stops as soon as every track resolves, usually well inside this).
const TS_HEAD_WARM: usize = 24 << 20; // 24 MiB

#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetFileInformationByHandleEx(
        h_file: *mut std::ffi::c_void,
        file_information_class: u32,
        file_information: *mut std::ffi::c_void,
        buffer_size: u32,
    ) -> i32;
}
/// `FILE_INFO_BY_HANDLE_CLASS::FileRemoteProtocolInfo`.
#[cfg(windows)]
const FILE_REMOTE_PROTOCOL_INFO_CLASS: u32 = 13;

/// Whether the open file lives on a network filesystem — the gate for every
/// warm. Errs toward `false`: warming pre-streams a head window, so a false
/// positive on a local disk would regress audioprobe's fast path, whereas a
/// false negative just leaves a NAS probe as slow as it is today. Decided from
/// the already-open handle (Windows) or the mount table (Linux) at no extra
/// network round-trip; every other platform declines.
#[cfg(windows)]
pub fn is_remote(file: &File, _path: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    // FILE_REMOTE_PROTOCOL_INFO is 116 bytes, 4-byte aligned; only the call's
    // success matters (it succeeds only for remote files), never the contents.
    let mut info = [0u32; 29];
    // SAFETY: the handle is valid for the lifetime of `file`, and the buffer is
    // a live, writable allocation of the documented size.
    unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_REMOTE_PROTOCOL_INFO_CLASS,
            info.as_mut_ptr().cast(),
            std::mem::size_of_val(&info) as u32,
        ) != 0
    }
}

#[cfg(target_os = "linux")]
pub fn is_remote(_file: &File, path: &Path) -> bool {
    let Ok(canon) = path.canonicalize() else {
        return false;
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    network_fstype(&canon, &mounts)
}

#[cfg(all(not(windows), not(target_os = "linux")))]
pub fn is_remote(_file: &File, _path: &Path) -> bool {
    false
}

/// Filesystem types that mean "bytes cross a network for every read".
#[cfg(any(target_os = "linux", test))]
const NETWORK_FSTYPES: &[&str] =
    &["cifs", "smb3", "nfs", "nfs4", "9p", "fuse.sshfs", "davfs", "afs", "ceph"];

/// Whether the mount holding `path` (longest mount-point prefix wins) is a
/// network filesystem, per a `/proc/self/mounts`-formatted table. Fields are
/// whitespace-separated with spaces octal-escaped (`\040`), so mount points
/// with spaces decode before matching. Pure, for testability; unknown or
/// unparseable input is `false` (the warm just stays off).
#[cfg(any(target_os = "linux", test))]
fn network_fstype(path: &Path, mounts: &str) -> bool {
    let path = path.to_string_lossy();
    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(_dev), Some(mount), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let mount = unescape_mount(mount);
        let is_prefix = path == mount
            || (path.starts_with(&mount)
                && (mount == "/" || path[mount.len()..].starts_with(['/', '\\'])));
        if is_prefix && best.is_none_or(|(len, _)| mount.len() >= len) {
            best = Some((mount.len(), NETWORK_FSTYPES.contains(&fstype)));
        }
    }
    best.is_some_and(|(_, net)| net)
}

/// Decode the octal escapes `/proc/self/mounts` uses for whitespace in mount
/// points (`\040` space, `\011` tab, plus `\012`/`\134`).
#[cfg(any(target_os = "linux", test))]
fn unescape_mount(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            if let Ok(code) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Warm the container metadata regions so a network filesystem streams them in
/// pipelined reads instead of many synchronous page faults. Best-effort and a
/// no-op on local volumes (`remote` is the caller's `is_remote` verdict); never
/// changes what is parsed. `data` is the whole-file mmap.
pub fn warm_metadata(remote: bool, file: &File, path: &Path, data: &[u8]) {
    if !remote {
        return;
    }
    let size = data.len();
    let mut ranges: Vec<(u64, usize)> = Vec::new();

    let is_ts = looks_like_ts(path, data);
    let is_mp4 = !is_ts && looks_like_mp4(path, data);
    let is_mkv = !is_ts && !is_mp4 && looks_like_mkv(path, data);
    let is_simple = !is_ts && !is_mp4 && !is_mkv && looks_like_simple(path, data);

    // Front-loaded metadata. TS reads a large head window to reach the audio
    // PES; MKV walks the EBML header + Tracks + leading clusters; a confirmed
    // MP4 or a simple-header format needs only a small front window (MP4's
    // `moov` is warmed by exact extent below).
    let head = if is_ts {
        TS_HEAD_WARM
    } else if is_mkv {
        HEAD_WARM
    } else if is_mp4 || is_simple {
        SIMPLE_HEAD_WARM
    } else {
        HEAD_WARM
    }
    .min(size);
    ranges.push((0, head));

    // The `moov` is warmed by its exact extent wherever it sits: a front-placed
    // one merges into the head range, a tail-placed one (the common QuickTime
    // faststart-less layout) is the region a head window could never cover.
    if is_mp4 {
        if let Some((start, end)) = moov_extent(data) {
            ranges.push((start, (end - start) as usize));
        }
    }

    warm_ranges(file, ranges);
}

/// Warm the given `(offset, len)` ranges: coalesce overlaps, then stream each
/// merged extent with positioned reads. Positioned reads on a shared `&File`
/// are safe — each carries its own offset and nothing relies on the file
/// cursor. Sequential (audioprobe warms at most a head plus one `moov` extent,
/// so the concurrency hdrprobe needs for scattered sample chunks doesn't pay).
fn warm_ranges(file: &File, ranges: Vec<(u64, usize)>) {
    for (start, end) in merge_ranges(ranges) {
        warm(file, start, (end - start) as usize);
    }
}

/// Sort `(offset, len)` ranges and coalesce overlapping/adjacent ones into
/// disjoint `(start, end)` extents, dropping empties.
fn merge_ranges(mut ranges: Vec<(u64, usize)>) -> Vec<(u64, u64)> {
    ranges.retain(|r| r.1 > 0);
    ranges.sort_unstable_by_key(|r| r.0);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (off, len) in ranges {
        let end = off.saturating_add(len as u64);
        match merged.last_mut() {
            Some(last) if off <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((off, end)),
        }
    }
    merged
}

/// Sequentially read `len` bytes from `offset` into a scratch buffer and discard
/// them, pulling the range into the OS/SMB cache. Positioned reads leave the
/// file cursor and the mmap untouched; errors are ignored (parsing still works,
/// just without the warm cache).
fn warm(file: &File, offset: u64, len: usize) {
    if len == 0 {
        return;
    }
    let mut buf = vec![0u8; len.min(1 << 20)]; // scratch, 1 MiB cap
    let mut pos = offset;
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        match read_at(file, &mut buf[..want], pos) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pos += n as u64;
                remaining -= n;
            }
        }
    }
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    file.seek_read(buf, off)
}
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    file.read_at(buf, off)
}

/// Find the ISOBMFF `moov` box by walking the top-level box list over the mmap.
/// Reads only box headers (8 or 16 bytes each), following each declared size,
/// so it faults a handful of pages — the same "a few cold round-trips to locate
/// the extent" trade hdrprobe makes. Returns the box's byte range `[start, end)`
/// including its header. `None` if not found or the boxes don't parse.
fn moov_extent(data: &[u8]) -> Option<(u64, u64)> {
    let len = data.len() as u64;
    let mut pos = 0u64;
    while pos + 8 <= len {
        let p = pos as usize;
        let size32 = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
        let kind = &data[p + 4..p + 8];
        let (box_size, header) = match size32 {
            1 => {
                // 64-bit size in the 8 bytes after the type.
                if pos + 16 > len {
                    return None;
                }
                let q = p + 8;
                let s = u64::from_be_bytes([
                    data[q], data[q + 1], data[q + 2], data[q + 3], data[q + 4], data[q + 5],
                    data[q + 6], data[q + 7],
                ]);
                (s, 16u64)
            }
            0 => (len - pos, 8u64), // extends to EOF
            n => (n as u64, 8u64),
        };
        if box_size < header {
            return None;
        }
        if kind == b"moov" {
            return Some((pos, pos.saturating_add(box_size).min(len)));
        }
        pos = pos.checked_add(box_size)?;
    }
    None
}

fn looks_like_mp4(path: &Path, data: &[u8]) -> bool {
    let ext = ext_of(path);
    matches!(ext.as_str(), "mp4" | "m4v" | "m4a" | "mov")
        || (data.len() >= 8
            && matches!(&data[4..8], b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"))
}

fn looks_like_ts(path: &Path, data: &[u8]) -> bool {
    let ext = ext_of(path);
    matches!(ext.as_str(), "ts" | "tsv" | "m2ts" | "mts")
        || crate::containers::sniffs_as_ts(&data[..data.len().min(64 * 1024)])
}

fn looks_like_mkv(path: &Path, data: &[u8]) -> bool {
    let ext = ext_of(path);
    matches!(ext.as_str(), "mkv" | "mka" | "mk3d" | "webm")
        || data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

fn looks_like_simple(path: &Path, data: &[u8]) -> bool {
    let ext = ext_of(path);
    matches!(
        ext.as_str(),
        "flac" | "wav" | "ogg" | "oga" | "opus" | "ac3" | "eac3" | "ec3" | "dts" | "dtshd"
            | "dtsma" | "thd" | "truehd" | "mlp" | "aac" | "adts" | "latm" | "loas" | "mp3"
            | "mp2" | "mp1" | "mpa"
    ) || data.starts_with(b"fLaC")
        || data.starts_with(b"OggS")
        || data.starts_with(b"ID3")
        || (data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WAVE")
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_fstype_matches_longest_mount_prefix() {
        let mounts = "\
/dev/sda1 / ext4 rw 0 0
//nas/media /mnt/nas cifs rw 0 0
/dev/sdb1 /mnt/nas/local ext4 rw 0 0
server:/export /mnt/n\\040f\\040s nfs4 rw 0 0
";
        let net = |p: &str| network_fstype(Path::new(p), mounts);
        assert!(net("/mnt/nas/movie.mkv"), "cifs mount");
        assert!(!net("/mnt/nas/local/movie.mkv"), "deeper local mount wins");
        assert!(!net("/home/user/movie.mkv"), "root ext4");
        assert!(net("/mnt/n f s/movie.mkv"), "octal-escaped mount point decodes");
        assert!(!net("/mnt/nascar/movie.mkv"), "prefix must end at a separator");
    }

    #[test]
    fn unescape_mount_decodes_octal_whitespace() {
        assert_eq!(unescape_mount("/mnt/n\\040f\\040s"), "/mnt/n f s");
        assert_eq!(unescape_mount("/plain"), "/plain");
        assert_eq!(unescape_mount("trailing\\04"), "trailing\\04");
    }

    #[test]
    fn merge_ranges_coalesces_overlaps_and_drops_empties() {
        let merged = merge_ranges(vec![
            (100, 50),  // 100..150
            (0, 0),     // empty
            (900, 10),  // 900..910
            (120, 80),  // overlaps -> ..200
            (200, 25),  // adjacent -> ..225
        ]);
        assert_eq!(merged, vec![(100, 225), (900, 910)]);
    }

    #[test]
    fn moov_extent_finds_a_tail_moov() {
        // ftyp (16) + mdat (24) + moov (32): the walk must follow the sizes and
        // land on the moov's exact byte range, header included.
        let mut f = Vec::new();
        f.extend_from_slice(&16u32.to_be_bytes());
        f.extend_from_slice(b"ftyp");
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&24u32.to_be_bytes());
        f.extend_from_slice(b"mdat");
        f.extend_from_slice(&[0u8; 16]);
        let moov_at = f.len() as u64;
        f.extend_from_slice(&32u32.to_be_bytes());
        f.extend_from_slice(b"moov");
        f.extend_from_slice(&[0u8; 24]);
        assert_eq!(moov_extent(&f), Some((moov_at, moov_at + 32)));
    }

    #[test]
    fn moov_extent_handles_64bit_size_and_absence() {
        // A 64-bit-sized moov (size32 == 1) at the front.
        let mut f = Vec::new();
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(b"moov");
        f.extend_from_slice(&40u64.to_be_bytes());
        f.extend_from_slice(&[0u8; 24]);
        assert_eq!(moov_extent(&f), Some((0, 40)));

        // No moov: a lone mdat.
        let mut g = Vec::new();
        g.extend_from_slice(&16u32.to_be_bytes());
        g.extend_from_slice(b"mdat");
        g.extend_from_slice(&[0u8; 8]);
        assert_eq!(moov_extent(&g), None);
    }
}
