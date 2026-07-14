//! ISO disc-image probing: locate the main feature and probe its audio.
//!
//! Blu-ray (BDMV): walk the UDF filesystem to `BDMV/PLAYLIST`, rank the
//! playlists by deduped duration, pick the winner's largest clip and probe
//! that `BDMV/STREAM/*.m2ts` as a transport stream. DVD-Video: walk the
//! ISO9660 bridge to `VIDEO_TS`, pick the title set with the most VOB bytes and
//! probe its first `VTS_tt_1.VOB` as a program stream. Everything runs against
//! the whole-image byte slice (the file's mmap), reusing the TS and PS backends
//! on the located clip's byte range.
//!
//! The UDF and MPLS readers are ported from hdrprobe; the ISO9660 reader and
//! the DVD selection are audioprobe's own.

mod iso9660;
mod mpls;
mod udf;

use std::collections::HashMap;
use std::io::Cursor;

use crate::report::Report;

use super::{mpegps, mpegts, Options};

/// Playlists are KiB-scale; cap the read so a corrupt file entry can't gather
/// megabytes per playlist.
const MPLS_READ_CAP: usize = 4 << 20;
/// Two playlists within this many seconds are a duration tie, broken by bytes.
const DURATION_TIE_SECS: f64 = 1.0;

/// Whether an image slice is any ISO we can probe (UDF or ISO9660).
pub fn looks_like_iso(data: &[u8]) -> bool {
    udf::is_udf_iso(data) || iso9660::looks_like_iso9660(data)
}

pub fn probe(data: &[u8], opts: &Options) -> Result<Report, String> {
    // Blu-ray first: a UDF volume with a BDMV directory. DVDs are UDF too, but
    // without BDMV, so a missing-BDMV UDF volume falls through to the DVD path.
    if udf::is_udf_iso(data) {
        if let Ok(vol) = udf::UdfVolume::open(data) {
            let has_bdmv = vol.root().and_then(|r| vol.read_dir(&r)).is_ok_and(|es| {
                es.iter().any(|e| e.is_dir && e.name.eq_ignore_ascii_case("BDMV"))
            });
            if has_bdmv {
                return probe_bluray(data, &vol, opts);
            }
        }
    }
    if iso9660::looks_like_iso9660(data) {
        return probe_dvd(data, opts);
    }
    Err("ISO image is neither a Blu-ray (BDMV) nor a DVD-Video (VIDEO_TS) volume".into())
}

// --- Blu-ray (BDMV over UDF) -------------------------------------------------

fn probe_bluray(data: &[u8], vol: &udf::UdfVolume, opts: &Options) -> Result<Report, String> {
    let find = |list: &[udf::Entry], name: &str| -> Option<udf::Entry> {
        list.iter().find(|e| e.name.eq_ignore_ascii_case(name)).cloned()
    };
    let root = vol.root()?;
    let entries = vol.read_dir(&root)?;
    let has_aacs = find(&entries, "AACS").is_some();
    let bdmv = find(&entries, "BDMV").filter(|e| e.is_dir).ok_or("no BDMV directory")?;
    let bdmv_entries = vol.read_dir(&bdmv)?;
    let playlist_dir =
        find(&bdmv_entries, "PLAYLIST").filter(|e| e.is_dir).ok_or("no BDMV/PLAYLIST directory")?;
    let stream_dir =
        find(&bdmv_entries, "STREAM").filter(|e| e.is_dir).ok_or("no BDMV/STREAM directory")?;

    // Clip id ("00055") -> (entry, size) from BDMV/STREAM.
    let mut clips: HashMap<String, (udf::Entry, u64)> = HashMap::new();
    for e in vol.read_dir(&stream_dir)? {
        if e.is_dir {
            continue;
        }
        let Some(id) = clip_id_of(&e.name) else { continue };
        let Ok(size) = vol.info_len(&e) else { continue };
        clips.insert(id, (e, size));
    }
    let clip_sizes: HashMap<String, u64> =
        clips.iter().map(|(id, (_, size))| (id.clone(), *size)).collect();

    // Parse every playlist (tiny files; malformed ones are skipped).
    let mut mpls_entries: Vec<udf::Entry> = vol
        .read_dir(&playlist_dir)?
        .into_iter()
        .filter(|e| !e.is_dir && has_ext(&e.name, ".mpls"))
        .collect();
    mpls_entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut cands = Vec::new();
    for e in &mpls_entries {
        if let Ok(bytes) = vol.read_small(e, MPLS_READ_CAP) {
            if let Ok(playlist) = mpls::parse(&bytes) {
                if !playlist.items.is_empty() {
                    cands.push(Candidate { name: e.name.clone(), playlist });
                }
            }
        }
    }

    let sel = select_main(&cands, &clip_sizes)?;
    let (clip_entry, _) = &clips[&sel.clip_id];
    let extents = vol.extents(clip_entry)?;
    let (clip_start, clip_len) = coalesce(&extents)
        .ok_or("main-feature m2ts is fragmented inside the ISO; not supported")?;
    let end = clip_start.saturating_add(clip_len);
    if end > data.len() as u64 {
        return Err("main-feature m2ts extends past the end of the image (truncated ISO?)".into());
    }

    let sub = &data[clip_start as usize..end as usize];
    let mut report = mpegts::probe(Cursor::new(sub), opts.scan_limit).map_err(|e| {
        if has_aacs {
            format!("clip {} is not readable ({e}); AACS-encrypted? probe a decrypted backup", clip_entry.name)
        } else {
            format!("main-feature clip {}: {e}", clip_entry.name)
        }
    })?;
    report.container = format!("Blu-ray ISO (BDMV, {} via {})", clip_entry.name, sel.name);
    Ok(report)
}

struct Candidate {
    name: String,
    playlist: mpls::Playlist,
}

struct Selection {
    name: String,
    clip_id: String,
}

/// The main-title heuristic: longest deduped duration wins, ties (within
/// `DURATION_TIE_SECS`) broken by total referenced clip bytes, then by the
/// lowest playlist name. Playlists referencing clips absent from STREAM are
/// dropped (robustness and decoy filtering), and identical PlayItem sequences
/// collapse to the lowest-numbered playlist. The probe clip is the winner's
/// largest referenced clip.
fn select_main(cands: &[Candidate], clip_sizes: &HashMap<String, u64>) -> Result<Selection, String> {
    let mut order: Vec<&Candidate> = cands.iter().collect();
    order.sort_by(|a, b| a.name.cmp(&b.name));
    let mut kept: Vec<&Candidate> = Vec::new();
    for c in order {
        if c.playlist.items.iter().any(|i| !clip_sizes.contains_key(&i.clip_id)) {
            continue;
        }
        if kept.iter().any(|k| k.playlist.items == c.playlist.items) {
            continue;
        }
        kept.push(c);
    }

    let score = |c: &Candidate| -> (f64, u64) {
        let bytes = c.playlist.distinct_clips().iter().map(|id| clip_sizes[*id]).sum();
        (c.playlist.duration_secs_deduped(), bytes)
    };
    let mut best: Option<(&Candidate, f64, u64)> = None;
    for c in kept {
        let (d, b) = score(c);
        let wins = match best {
            None => true,
            Some((_, bd, bb)) => {
                if (d - bd).abs() > DURATION_TIE_SECS {
                    d > bd
                } else {
                    b > bb // equal falls through: kept is name-sorted, first wins
                }
            }
        };
        if wins {
            best = Some((c, d, b));
        }
    }
    let (best, _, _) = best.ok_or("no usable playlist in BDMV/PLAYLIST")?;

    let distinct = best.playlist.distinct_clips();
    let mut pick = 0usize;
    for (i, id) in distinct.iter().enumerate() {
        if clip_sizes[*id] > clip_sizes[distinct[pick]] {
            pick = i;
        }
    }
    Ok(Selection { name: best.name.clone(), clip_id: distinct[pick].to_string() })
}

/// One contiguous `(start, len)` from file-ordered extents, or `None` when a
/// gap remains. UDF caps a single extent near 1 GiB, so a feature-length clip
/// is many exactly-adjacent extents; a genuinely scattered file stays `None`.
fn coalesce(extents: &[(u64, u64)]) -> Option<(u64, u64)> {
    let (&(start, first_len), rest) = extents.split_first()?;
    let mut end = start.checked_add(first_len)?;
    for &(off, len) in rest {
        if off != end {
            return None;
        }
        end = end.checked_add(len)?;
    }
    (end > start).then_some((start, end - start))
}

/// `"00055.m2ts"` -> `"00055"`; `None` for non-m2ts names.
fn clip_id_of(name: &str) -> Option<String> {
    has_ext(name, ".m2ts").then(|| name[..name.len() - 5].to_ascii_uppercase())
}

fn has_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

// --- DVD-Video (VIDEO_TS over ISO9660) ---------------------------------------

fn probe_dvd(data: &[u8], opts: &Options) -> Result<Report, String> {
    let iso = iso9660::Iso9660::open(data)?;
    let root = iso.root();
    let entries = iso.read_dir(&root)?;
    let video_ts = entries
        .iter()
        .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
        .ok_or("no VIDEO_TS directory (not a DVD-Video ISO)")?;
    let files = iso.read_dir(video_ts)?;

    let (vob, title) = select_dvd_vob(&files).ok_or("no VOB files in VIDEO_TS")?;
    let end = vob.offset.saturating_add(vob.size);
    if end > data.len() as u64 {
        return Err("main-feature VOB extends past the end of the image (truncated ISO?)".into());
    }
    let sub = &data[vob.offset as usize..end as usize];
    let mut report = mpegps::probe(Cursor::new(sub), opts.scan_limit)
        .map_err(|e| format!("main-feature VOB {}: {e}", vob.name))?;
    report.container = format!("DVD-Video ISO ({title}, {})", vob.name);
    Ok(report)
}

/// The DVD main title set: the `VTS_tt` group with the most VOB bytes across its
/// parts 1..9 (menu part 0 and `VIDEO_TS.VOB` excluded), returning its first
/// part `VTS_tt_1.VOB`. Falls back to the single largest VOB if no title set has
/// a part 1.
fn select_dvd_vob(files: &[iso9660::DirRec]) -> Option<(iso9660::DirRec, String)> {
    let mut totals: HashMap<String, u64> = HashMap::new();
    let mut part1: HashMap<String, iso9660::DirRec> = HashMap::new();
    for f in files.iter().filter(|f| !f.is_dir) {
        if let Some((tt, p)) = parse_vts(&f.name) {
            if (1..=9).contains(&p) {
                *totals.entry(tt.clone()).or_default() += f.size;
                if p == 1 {
                    part1.insert(tt, f.clone());
                }
            }
        }
    }
    // Best title set: most bytes, ties broken by the lowest set number.
    let best_tt = totals
        .iter()
        .filter(|(tt, _)| part1.contains_key(*tt))
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(tt, _)| tt.clone());
    if let Some(tt) = best_tt {
        let vob = part1.get(&tt)?.clone();
        return Some((vob, format!("VTS_{tt}")));
    }
    // Fallback: the largest VOB present at all.
    let vob = files
        .iter()
        .filter(|f| !f.is_dir && has_ext(&f.name, ".vob"))
        .max_by_key(|f| f.size)?
        .clone();
    Some((vob, "title".into()))
}

/// `"VTS_01_1.VOB"` -> `("01", 1)`; `None` for menu (`VIDEO_TS.VOB`) or non-VTS.
fn parse_vts(name: &str) -> Option<(String, u32)> {
    let up = name.to_ascii_uppercase();
    let stem = up.strip_suffix(".VOB")?;
    let mut it = stem.split('_');
    if it.next()? != "VTS" {
        return None;
    }
    let tt = it.next()?.to_string();
    let p: u32 = it.next()?.parse().ok()?;
    Some((tt, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, segs: &[(&str, u32, u32)]) -> Candidate {
        Candidate { name: name.into(), playlist: mpls::parse(&mpls::tests::build_mpls(segs)).unwrap() }
    }

    #[test]
    fn select_main_picks_longest_then_largest_clip() {
        let sizes: HashMap<String, u64> =
            [("00001".into(), 900u64), ("00002".into(), 100), ("00003".into(), 50)]
                .into_iter()
                .collect();
        let cands = vec![
            cand("00000.mpls", &[("00003", 0, 45_000)]),           // 1s extras
            cand("00800.mpls", &[("00001", 0, 45_000 * 100), ("00002", 0, 45_000 * 100)]), // long feature
        ];
        let sel = select_main(&cands, &sizes).unwrap();
        assert_eq!(sel.name, "00800.mpls");
        // Winner's largest referenced clip is 00001 (900 bytes).
        assert_eq!(sel.clip_id, "00001");
    }

    #[test]
    fn select_main_drops_playlists_with_missing_clips() {
        let sizes: HashMap<String, u64> = [("00001".into(), 100u64)].into_iter().collect();
        let cands = vec![cand("00000.mpls", &[("09999", 0, 45_000 * 999)])];
        assert!(select_main(&cands, &sizes).is_err());
    }

    #[test]
    fn coalesce_joins_adjacent_and_rejects_gaps() {
        assert_eq!(coalesce(&[(100, 50), (150, 50)]), Some((100, 100)));
        assert_eq!(coalesce(&[(100, 50), (200, 50)]), None);
        assert_eq!(coalesce(&[]), None);
    }

    #[test]
    fn select_dvd_vob_picks_biggest_title_set() {
        let rec = |name: &str, size: u64| iso9660::DirRec {
            name: name.into(),
            is_dir: false,
            offset: 0,
            size,
        };
        let files = vec![
            rec("VIDEO_TS.VOB", 1000), // menu, excluded
            rec("VTS_01_1.VOB", 100),
            rec("VTS_02_1.VOB", 500),
            rec("VTS_02_2.VOB", 500),
        ];
        let (vob, title) = select_dvd_vob(&files).unwrap();
        assert_eq!(title, "VTS_02"); // 1000 bytes beats VTS_01's 100
        assert_eq!(vob.name, "VTS_02_1.VOB");
    }

    // ---- End-to-end DVD: synthetic ISO9660 with a real MPEG-PS VOB ----

    fn ps_with_mp2() -> Vec<u8> {
        let pack = [0u8, 0, 1, 0xBA, 0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0xF8];
        let mut mp2 = vec![0xFFu8, 0xFD, 0x94, 0x00];
        mp2.resize(4096, 0);
        let mut body = vec![0x80u8, 0x00, 0x00];
        body.extend_from_slice(&mp2);
        let mut pes = vec![0u8, 0, 1, 0xC0];
        pes.extend_from_slice(&(body.len() as u16).to_be_bytes());
        pes.extend_from_slice(&body);
        [&pack[..], &pes[..]].concat()
    }

    #[test]
    fn probes_dvd_iso_end_to_end() {
        let img = iso9660::testimg::build(&[
            iso9660::testimg::FileSpec { name: "VIDEO_TS.VOB".into(), data: vec![0u8; 100] },
            iso9660::testimg::FileSpec { name: "VTS_01_1.VOB".into(), data: ps_with_mp2() },
        ]);
        let opts = Options { scan_limit: 1 << 20, deep: true };
        let report = probe(&img, &opts).expect("probe ok");
        assert!(report.container.starts_with("DVD-Video ISO"), "{}", report.container);
        assert_eq!(report.tracks.len(), 1);
        assert_eq!(report.tracks[0].codec, "MP2");
        assert_eq!(report.tracks[0].sample_rate, Some(48000));
    }

    // ---- End-to-end Blu-ray: synthetic UDF image with a real M2TS clip ----

    /// A 188-byte TS packet.
    fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0xFFu8; 188];
        p[0] = 0x47;
        p[1] = (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F);
        p[2] = (pid & 0xFF) as u8;
        p[3] = 0x10 | (cc & 0xF);
        let n = payload.len().min(184);
        p[4..4 + n].copy_from_slice(&payload[..n]);
        p
    }
    /// Wrap a 188-byte packet into a 192-byte M2TS packet (4-byte TP_extra_header).
    fn m2ts(pkt: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 4];
        v.extend_from_slice(pkt);
        v
    }

    fn build_m2ts_clip() -> Vec<u8> {
        let psi = |t: &[u8]| {
            let mut v = vec![0u8];
            v.extend_from_slice(t);
            v
        };
        let pat = |pmt_pid: u16| {
            let mut t = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01];
            t.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
            t.push((pmt_pid & 0xFF) as u8);
            t.extend_from_slice(&[0, 0, 0, 0]);
            psi(&t)
        };
        let pmt = || {
            let mut body =
                vec![0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00];
            // stream_type 0x04 (MPEG audio), PID 0x111, no descriptors.
            body.push(0x04);
            body.push(0xE1);
            body.push(0x11);
            body.push(0xF0);
            body.push(0x00);
            let section_len = body.len() + 4;
            let mut t = vec![
                0x02,
                0xB0 | ((section_len >> 8) as u8 & 0x0F),
                (section_len & 0xFF) as u8,
            ];
            t.extend_from_slice(&body);
            t.extend_from_slice(&[0, 0, 0, 0]);
            psi(&t)
        };
        let pes = |sid: u8, es: &[u8]| {
            let mut v = vec![0, 0, 1, sid, 0, 0, 0x80, 0x00, 0x00];
            v.extend_from_slice(es);
            v
        };
        let mut mp2 = vec![0xFFu8, 0xFD, 0x94, 0x00];
        mp2.resize(4096, 0);

        let mut clip = Vec::new();
        clip.extend_from_slice(&m2ts(&ts_packet(0, true, 0, &pat(0x100))));
        clip.extend_from_slice(&m2ts(&ts_packet(0x100, true, 0, &pmt())));
        clip.extend_from_slice(&m2ts(&ts_packet(0x111, true, 0, &pes(0xC0, &mp2))));
        for i in 0..8 {
            clip.extend_from_slice(&m2ts(&ts_packet(0x1FFF, false, i, &[])));
        }
        clip
    }

    #[test]
    fn probes_bluray_iso_end_to_end() {
        use udf::testimg::{DirSpec, Opts};
        let clip = build_m2ts_clip();
        let playlist = mpls::tests::build_mpls(&[("00001", 0, 45_000 * 120)]);
        let tree = DirSpec::named("").dir(
            DirSpec::named("BDMV")
                .dir(DirSpec::named("PLAYLIST").file("00000.mpls", playlist))
                .dir(DirSpec::named("STREAM").file("00001.m2ts", clip)),
        );
        let img = udf::testimg::build(&tree, &Opts { metadata_partition: false });

        let opts = Options { scan_limit: 1 << 20, deep: true };
        let report = probe(&img, &opts).expect("probe ok");
        assert!(report.container.starts_with("Blu-ray ISO (BDMV"), "{}", report.container);
        assert_eq!(report.tracks.len(), 1);
        assert_eq!(report.tracks[0].codec, "MP2");
        assert_eq!(report.tracks[0].sample_rate, Some(48000));
    }
}
