//! Native Matroska / WebM (EBML) parser.
//!
//! Reads the Tracks element for audio track metadata, then — for codecs whose
//! container metadata is incomplete (bit depth for DTS, exact rates for
//! AC-3/TrueHD, …) — samples the first frames from the Clusters and hands
//! them to the codec header parsers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use crate::codecs::{self, CodecInfo};
use crate::report::{Report, Track};

// Element IDs (with marker bits, as stored)
const ID_EBML: u64 = 0x1A45DFA3;
const ID_DOCTYPE: u64 = 0x4282;
const ID_SEGMENT: u64 = 0x18538067;
const ID_TRACKS: u64 = 0x1654AE6B;
const ID_TRACK_ENTRY: u64 = 0xAE;
const ID_TRACK_NUMBER: u64 = 0xD7;
const ID_TRACK_TYPE: u64 = 0x83;
const ID_FLAG_DEFAULT: u64 = 0x88;
const ID_CODEC_ID: u64 = 0x86;
const ID_CODEC_PRIVATE: u64 = 0x63A2;
const ID_NAME: u64 = 0x536E;
const ID_LANGUAGE: u64 = 0x22B59C;
const ID_LANGUAGE_IETF: u64 = 0x22B59D;
const ID_AUDIO: u64 = 0xE1;
const ID_SAMPLING_FREQUENCY: u64 = 0xB5;
const ID_OUTPUT_SAMPLING_FREQUENCY: u64 = 0x78B5;
const ID_CHANNELS: u64 = 0x9F;
const ID_BIT_DEPTH: u64 = 0x6264;
const ID_CLUSTER: u64 = 0x1F43B675;
const ID_TIMESTAMP: u64 = 0xE7;
const ID_SIMPLE_BLOCK: u64 = 0xA3;
const ID_BLOCK_GROUP: u64 = 0xA0;
const ID_BLOCK: u64 = 0xA1;
const ID_VOID: u64 = 0xEC;
const ID_CRC32: u64 = 0xBF;
const ID_CLUSTER_POSITION: u64 = 0xA7;
const ID_CLUSTER_PREV_SIZE: u64 = 0xAB;
const ID_SILENT_TRACKS: u64 = 0x5854;

const UNKNOWN_SIZE: u64 = u64::MAX;
const MAX_SAMPLE_BYTES: usize = 256 * 1024; // per-track frame sample cap
const MAX_CLUSTERS: usize = 64;

struct Ebml<R> {
    r: R,
    pos: u64,
}

impl<R: Read + Seek> Ebml<R> {
    fn read_byte(&mut self) -> std::io::Result<u8> {
        let mut b = [0u8; 1];
        self.r.read_exact(&mut b)?;
        self.pos += 1;
        Ok(b[0])
    }

    fn read_bytes(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.r.read_exact(&mut buf)?;
        self.pos += n as u64;
        Ok(buf)
    }

    fn seek_to(&mut self, pos: u64) -> std::io::Result<()> {
        self.r.seek(SeekFrom::Start(pos))?;
        self.pos = pos;
        Ok(())
    }

    /// Read an element ID (marker bits kept, as conventionally written).
    fn read_id(&mut self) -> std::io::Result<u64> {
        let first = self.read_byte()?;
        let len = vint_length(first).ok_or_else(invalid)?;
        let mut v = first as u64;
        for _ in 1..len {
            v = (v << 8) | self.read_byte()? as u64;
        }
        Ok(v)
    }

    /// Read an element size (marker stripped; UNKNOWN_SIZE for "unknown").
    fn read_size(&mut self) -> std::io::Result<u64> {
        let first = self.read_byte()?;
        let len = vint_length(first).ok_or_else(invalid)?;
        let mask = (0xFFu32 >> len) as u8; // len can be 8 -> mask 0
        let mut v = (first & mask) as u64;
        let mut all_ones = v == mask as u64;
        for _ in 1..len {
            let b = self.read_byte()?;
            all_ones = all_ones && b == 0xFF;
            v = (v << 8) | b as u64;
        }
        if all_ones {
            return Ok(UNKNOWN_SIZE);
        }
        Ok(v)
    }

    fn read_uint(&mut self, size: u64) -> std::io::Result<u64> {
        if size > 8 {
            return Err(invalid());
        }
        let bytes = self.read_bytes(size as usize)?;
        Ok(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64))
    }

    fn read_float(&mut self, size: u64) -> std::io::Result<f64> {
        match size {
            0 => Ok(0.0),
            4 => {
                let b = self.read_bytes(4)?;
                Ok(f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64)
            }
            8 => {
                let b = self.read_bytes(8)?;
                Ok(f64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            _ => Err(invalid()),
        }
    }

    fn read_string(&mut self, size: u64) -> std::io::Result<String> {
        let bytes = self.read_bytes(size.min(1024) as usize)?;
        if size > 1024 {
            self.skip(size - 1024)?;
        }
        Ok(String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_string())
    }

    fn skip(&mut self, n: u64) -> std::io::Result<()> {
        self.seek_to(self.pos + n)
    }
}

fn vint_length(first: u8) -> Option<u32> {
    if first == 0 {
        return None;
    }
    Some(first.leading_zeros() + 1)
}

fn invalid() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid EBML data")
}

#[derive(Default)]
struct MkvTrack {
    number: u64,
    track_type: u64,
    codec_id: String,
    codec_private: Vec<u8>,
    language: Option<String>,
    language_ietf: Option<String>,
    name: Option<String>,
    default: bool,
    sampling_frequency: Option<f64>,
    output_sampling_frequency: Option<f64>,
    channels: Option<u64>,
    bit_depth: Option<u64>,
}

pub fn probe<R: Read + Seek>(reader: R, file_len: u64, deep: bool) -> Result<Report, String> {
    let mut e = Ebml { r: reader, pos: 0 };
    let mut report = Report {
        container: "Matroska".into(),
        ..Report::default()
    };

    // EBML header
    let id = e.read_id().map_err(|err| err.to_string())?;
    if id != ID_EBML {
        return Err("not a Matroska file".into());
    }
    let size = e.read_size().map_err(|err| err.to_string())?;
    if size != UNKNOWN_SIZE {
        let end = e.pos + size;
        while e.pos < end {
            let (cid, csize) = match read_header(&mut e) {
                Ok(v) => v,
                Err(_) => break,
            };
            if csize == UNKNOWN_SIZE {
                break;
            }
            if cid == ID_DOCTYPE {
                let doctype = e.read_string(csize).map_err(|err| err.to_string())?;
                if doctype == "webm" {
                    report.container = "WebM".into();
                }
            } else {
                let _ = e.skip(csize);
            }
        }
        let _ = e.seek_to(end);
    }

    // Segment
    let id = e.read_id().map_err(|err| err.to_string())?;
    if id != ID_SEGMENT {
        return Err("no Segment element found".into());
    }
    let seg_size = e.read_size().map_err(|err| err.to_string())?;
    let seg_end = if seg_size == UNKNOWN_SIZE {
        file_len
    } else {
        (e.pos + seg_size).min(file_len)
    };

    let mut tracks: Vec<MkvTrack> = Vec::new();
    let mut tracks_seen = false;
    // Track number -> sampled frame bytes for deep inspection.
    let mut samples: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut needed: Vec<u64> = Vec::new();
    let mut clusters_scanned = 0usize;

    while e.pos < seg_end {
        let (id, size) = match read_header(&mut e) {
            Ok(v) => v,
            Err(_) => break,
        };
        match id {
            ID_TRACKS => {
                if size == UNKNOWN_SIZE {
                    return Err("Tracks element with unknown size".into());
                }
                let end = e.pos + size;
                while e.pos < end {
                    let (cid, csize) = read_header(&mut e).map_err(|err| err.to_string())?;
                    if cid == ID_TRACK_ENTRY && csize != UNKNOWN_SIZE {
                        let entry_end = e.pos + csize;
                        if let Ok(t) = parse_track_entry(&mut e, entry_end) {
                            tracks.push(t);
                        }
                    } else if csize != UNKNOWN_SIZE {
                        e.skip(csize).map_err(|err| err.to_string())?;
                    } else {
                        break;
                    }
                }
                tracks_seen = true;
                needed = tracks
                    .iter()
                    .filter(|t| t.track_type == 2 && wants_frame_sample(&t.codec_id))
                    .map(|t| t.number)
                    .collect();
                if !deep || needed.is_empty() {
                    break;
                }
            }
            ID_CLUSTER => {
                if !tracks_seen || needed.is_empty() || clusters_scanned >= MAX_CLUSTERS {
                    break;
                }
                clusters_scanned += 1;
                let cluster_end = if size == UNKNOWN_SIZE {
                    seg_end
                } else {
                    e.pos + size
                };
                if scan_cluster(
                    &mut e,
                    cluster_end,
                    size == UNKNOWN_SIZE,
                    &mut samples,
                    &mut needed,
                )
                .is_err()
                {
                    break;
                }
                if needed.is_empty() {
                    break;
                }
            }
            _ => {
                if size == UNKNOWN_SIZE {
                    break;
                }
                if e.skip(size).is_err() {
                    break;
                }
            }
        }
    }

    if !tracks_seen {
        return Err("no Tracks element found".into());
    }

    for t in tracks.iter().filter(|t| t.track_type == 2) {
        report.tracks.push(build_track(t, samples.get(&t.number)));
    }
    Ok(report)
}

fn read_header<R: Read + Seek>(e: &mut Ebml<R>) -> std::io::Result<(u64, u64)> {
    let id = e.read_id()?;
    let size = e.read_size()?;
    Ok((id, size))
}

fn parse_track_entry<R: Read + Seek>(e: &mut Ebml<R>, end: u64) -> std::io::Result<MkvTrack> {
    let mut t = MkvTrack {
        default: true, // FlagDefault defaults to 1
        ..MkvTrack::default()
    };
    while e.pos < end {
        let (id, size) = read_header(e)?;
        if size == UNKNOWN_SIZE {
            return Err(invalid());
        }
        match id {
            ID_TRACK_NUMBER => t.number = e.read_uint(size)?,
            ID_TRACK_TYPE => t.track_type = e.read_uint(size)?,
            ID_FLAG_DEFAULT => t.default = e.read_uint(size)? == 1,
            ID_CODEC_ID => t.codec_id = e.read_string(size)?,
            ID_CODEC_PRIVATE => t.codec_private = e.read_bytes(size.min(65536) as usize)?,
            ID_NAME => t.name = Some(e.read_string(size)?),
            ID_LANGUAGE => t.language = Some(e.read_string(size)?),
            ID_LANGUAGE_IETF => t.language_ietf = Some(e.read_string(size)?),
            ID_AUDIO => {
                let aend = e.pos + size;
                while e.pos < aend {
                    let (aid, asize) = read_header(e)?;
                    if asize == UNKNOWN_SIZE {
                        return Err(invalid());
                    }
                    match aid {
                        ID_SAMPLING_FREQUENCY => t.sampling_frequency = Some(e.read_float(asize)?),
                        ID_OUTPUT_SAMPLING_FREQUENCY => {
                            t.output_sampling_frequency = Some(e.read_float(asize)?)
                        }
                        ID_CHANNELS => t.channels = Some(e.read_uint(asize)?),
                        ID_BIT_DEPTH => t.bit_depth = Some(e.read_uint(asize)?),
                        _ => e.skip(asize)?,
                    }
                }
            }
            _ => e.skip(size)?,
        }
    }
    Ok(t)
}

/// Which codecs benefit from sampling actual frames out of the clusters?
fn wants_frame_sample(codec_id: &str) -> bool {
    matches!(codec_id, "A_AC3" | "A_EAC3" | "A_TRUEHD" | "A_MLP")
        || codec_id.starts_with("A_DTS")
        || codec_id.starts_with("A_MPEG/")
}

fn scan_cluster<R: Read + Seek>(
    e: &mut Ebml<R>,
    cluster_end: u64,
    unknown_size: bool,
    samples: &mut HashMap<u64, Vec<u8>>,
    needed: &mut Vec<u64>,
) -> std::io::Result<()> {
    while e.pos < cluster_end {
        let start = e.pos;
        let (id, size) = match read_header(e) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        match id {
            ID_SIMPLE_BLOCK | ID_BLOCK => {
                if size == UNKNOWN_SIZE {
                    return Err(invalid());
                }
                collect_block(e, size, samples, needed)?;
            }
            ID_BLOCK_GROUP => {
                if size == UNKNOWN_SIZE {
                    return Err(invalid());
                }
                let gend = e.pos + size;
                while e.pos < gend {
                    let (gid, gsize) = read_header(e)?;
                    if gsize == UNKNOWN_SIZE {
                        return Err(invalid());
                    }
                    if gid == ID_BLOCK {
                        collect_block(e, gsize, samples, needed)?;
                    } else {
                        e.skip(gsize)?;
                    }
                }
            }
            ID_TIMESTAMP | ID_VOID | ID_CRC32 | ID_CLUSTER_POSITION | ID_CLUSTER_PREV_SIZE
            | ID_SILENT_TRACKS => {
                if size == UNKNOWN_SIZE {
                    return Err(invalid());
                }
                e.skip(size)?;
            }
            _ => {
                // Unknown-size clusters end at the next element that doesn't
                // belong inside a cluster (e.g. the next Cluster ID).
                if unknown_size {
                    e.seek_to(start)?;
                    return Ok(());
                }
                if size == UNKNOWN_SIZE {
                    return Err(invalid());
                }
                e.skip(size)?;
            }
        }
        if needed.is_empty() {
            // Skip the rest of the cluster; the caller stops afterwards.
            if !unknown_size {
                e.seek_to(cluster_end)?;
            }
            return Ok(());
        }
    }
    Ok(())
}

fn collect_block<R: Read + Seek>(
    e: &mut Ebml<R>,
    size: u64,
    samples: &mut HashMap<u64, Vec<u8>>,
    needed: &mut Vec<u64>,
) -> std::io::Result<()> {
    let take = size.min(128 * 1024) as usize;
    let data = e.read_bytes(take)?;
    if (size as usize) > take {
        e.skip(size - take as u64)?;
    }
    if let Some((track, payload_start)) = parse_block_header(&data) {
        if needed.contains(&track) {
            let buf = samples.entry(track).or_default();
            buf.extend_from_slice(&data[payload_start..]);
            if buf.len() >= 16 * 1024 || buf.len() >= MAX_SAMPLE_BYTES {
                // Enough material collected; try to finalize lazily later,
                // but stop collecting for this track once we hit the cap.
                if buf.len() >= 16 * 1024 {
                    needed.retain(|&n| n != track);
                }
            }
        }
    }
    Ok(())
}

/// Returns (track number, offset of first payload byte after lacing headers).
fn parse_block_header(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }
    let len = vint_length(data[0])? as usize;
    if data.len() < len + 3 {
        return None;
    }
    let mut track = (data[0] & (0xFFu32 >> len) as u8) as u64;
    for &b in &data[1..len] {
        track = (track << 8) | b as u64;
    }
    let flags = data[len + 2];
    let mut p = len + 3;
    let lacing = (flags >> 1) & 3;
    if lacing != 0 {
        let nframes_minus1 = *data.get(p)? as usize;
        p += 1;
        match lacing {
            1 => {
                // Xiph: n-1 sizes, each a run of 0xFF bytes plus terminator
                for _ in 0..nframes_minus1 {
                    while *data.get(p)? == 255 {
                        p += 1;
                    }
                    p += 1;
                }
            }
            3 if nframes_minus1 > 0 => {
                // EBML: first size as VINT, then n-2 signed VINT deltas
                let l = vint_length(*data.get(p)?)? as usize;
                p += l;
                for _ in 0..nframes_minus1.saturating_sub(1) {
                    let l = vint_length(*data.get(p)?)? as usize;
                    p += l;
                }
            }
            _ => {} // fixed-size lacing (or single-frame EBML): nothing to skip
        }
    }
    if p > data.len() {
        return None;
    }
    Some((track, p))
}

fn build_track(t: &MkvTrack, sample: Option<&Vec<u8>>) -> Track {
    let empty: Vec<u8> = Vec::new();
    let sample = sample.unwrap_or(&empty);
    let private = &t.codec_private;

    let parsed: Option<CodecInfo> = match t.codec_id.as_str() {
        "A_AC3" | "A_EAC3" => codecs::ac3::parse(sample),
        "A_TRUEHD" | "A_MLP" => codecs::truehd::parse(sample),
        "A_FLAC" => codecs::flac::parse(private),
        "A_OPUS" => codecs::xiph::parse_opus_head(private),
        "A_VORBIS" => codecs::xiph::parse_vorbis(private),
        "A_MS/ACM" => codecs::pcm::parse_waveformatex(private),
        "A_ALAC" => codecs::pcm::parse_alac_config(private),
        id if id.starts_with("A_DTS") => codecs::dts::parse(sample),
        id if id.starts_with("A_AAC") => {
            if !private.is_empty() {
                codecs::aac::parse_asc(private)
            } else {
                None
            }
        }
        id if id.starts_with("A_MPEG/") => codecs::mpeg_audio::parse(sample),
        _ => None,
    };

    let fallback_name = codec_display_name(&t.codec_id);
    let container_rate = t
        .output_sampling_frequency
        .or(t.sampling_frequency)
        .map(|f| f.round() as u32)
        .filter(|&r| r > 0);

    let mut track = Track {
        id: t.number.to_string(),
        codec: fallback_name,
        sample_rate: container_rate,
        bit_depth: t.bit_depth.map(|d| d as u32),
        channels: t.channels.map(|c| c as u32),
        lfe: None,
        language: t.language_ietf.clone().or_else(|| t.language.clone()),
        title: t.name.clone(),
        default: t.default,
        note: None,
    };

    if let Some(info) = parsed {
        if let Some(name) = info.name {
            track.codec = name;
        }
        if info.sample_rate.is_some() {
            track.sample_rate = info.sample_rate;
        }
        if info.bit_depth.is_some() {
            track.bit_depth = info.bit_depth;
        }
        if info.channels.is_some() {
            track.channels = info.channels;
            track.lfe = info.lfe;
        }
        track.note = info.note;
    } else if wants_frame_sample(&t.codec_id) && sample.is_empty() {
        track.note = Some("container metadata only".into());
    }

    // PCM float is at least 32-bit even if BitDepth is missing.
    if t.codec_id == "A_PCM/FLOAT/IEEE" && track.bit_depth.is_none() {
        track.bit_depth = Some(32);
    }
    // Old-style SBR signalling without CodecPrivate: output rate is doubled.
    if t.codec_id.contains("/SBR") && t.output_sampling_frequency.is_none() {
        if let Some(r) = track.sample_rate {
            if r <= 24000 {
                track.sample_rate = Some(r * 2);
            }
        }
    }
    track
}

fn codec_display_name(codec_id: &str) -> String {
    match codec_id {
        "A_AC3" => "AC-3".into(),
        "A_EAC3" => "E-AC-3".into(),
        "A_TRUEHD" => "TrueHD".into(),
        "A_MLP" => "MLP".into(),
        "A_DTS" => "DTS".into(),
        "A_DTS/LOSSLESS" => "DTS-HD MA".into(),
        "A_DTS/EXPRESS" => "DTS Express".into(),
        "A_FLAC" => "FLAC".into(),
        "A_OPUS" => "Opus".into(),
        "A_VORBIS" => "Vorbis".into(),
        "A_ALAC" => "ALAC".into(),
        "A_WAVPACK4" => "WavPack".into(),
        "A_TTA1" => "TTA".into(),
        "A_MPEG/L3" => "MP3".into(),
        "A_MPEG/L2" => "MP2".into(),
        "A_MPEG/L1" => "MP1".into(),
        "A_PCM/INT/LIT" => "PCM".into(),
        "A_PCM/INT/BIG" => "PCM (BE)".into(),
        "A_PCM/FLOAT/IEEE" => "PCM (float)".into(),
        "A_MS/ACM" => "ACM".into(),
        id if id.starts_with("A_AAC") => {
            let mut name = "AAC".to_string();
            if id.contains("/SBR") {
                name = "HE-AAC".into();
            } else if id.contains("/LC") {
                name = "AAC-LC".into();
            } else if id.contains("/MAIN") {
                name = "AAC Main".into();
            }
            name
        }
        other => other.trim_start_matches("A_").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::tests_support::BitWriter;
    use std::io::Cursor;

    /// EBML element: id bytes + 8-byte size vint + payload.
    fn elem(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        let size = payload.len() as u64;
        v.push(0x01);
        v.extend_from_slice(&size.to_be_bytes()[1..8]);
        v.extend_from_slice(payload);
        v
    }

    fn dts_core_frame() -> Vec<u8> {
        // 48 kHz, amode 9 (5 ch), LFE on, pcmr 5 (24 bit)
        let mut w = BitWriter::new();
        w.put(32, 0x7FFE8001);
        w.put(1, 1); // FTYPE
        w.put(5, 31); // SHORT
        w.put(1, 0); // CPF
        w.put(7, 15); // NBLKS
        w.put(14, 1007); // FSIZE
        w.put(6, 9); // AMODE
        w.put(4, 13); // SFREQ = 48 kHz
        w.put(5, 24); // RATE
        w.put(5, 0); // MIX..HDCD
        w.put(3, 0); // EXT_AUDIO_ID
        w.put(1, 0); // EXT_AUDIO
        w.put(1, 0); // ASPF
        w.put(2, 1); // LFF
        w.put(1, 0); // HFLAG
        w.put(1, 0); // FILTS
        w.put(4, 7); // VERNUM
        w.put(2, 0); // CHIST
        w.put(3, 5); // PCMR = 24 bit
        w.put(32, 0);
        w.finish()
    }

    fn synthetic_mkv() -> Vec<u8> {
        let ebml_header = elem(&[0x1A, 0x45, 0xDF, 0xA3], &elem(&[0x42, 0x82], b"matroska"));
        let audio = {
            let mut a = elem(&[0xB5], &48000.0f32.to_be_bytes()); // SamplingFrequency
            a.extend_from_slice(&elem(&[0x9F], &[6])); // Channels
            a
        };
        let mut track_entry = elem(&[0xD7], &[1]); // TrackNumber
        track_entry.extend_from_slice(&elem(&[0x83], &[2])); // TrackType = audio
        track_entry.extend_from_slice(&elem(&[0x86], b"A_DTS")); // CodecID
        track_entry.extend_from_slice(&elem(&[0x22, 0xB5, 0x9C], b"ger")); // Language
        track_entry.extend_from_slice(&elem(&[0xE1], &audio));
        let tracks = elem(&[0x16, 0x54, 0xAE, 0x6B], &elem(&[0xAE], &track_entry));

        // SimpleBlock: track vint, 16-bit timestamp, flags, frame data
        let mut block = vec![0x81, 0x00, 0x00, 0x00];
        block.extend_from_slice(&dts_core_frame());
        let mut cluster_payload = elem(&[0xE7], &[0]); // Timestamp
        cluster_payload.extend_from_slice(&elem(&[0xA3], &block));
        let cluster = elem(&[0x1F, 0x43, 0xB6, 0x75], &cluster_payload);

        let mut segment_payload = tracks;
        segment_payload.extend_from_slice(&cluster);
        let segment = elem(&[0x18, 0x53, 0x80, 0x67], &segment_payload);

        let mut file = ebml_header;
        file.extend_from_slice(&segment);
        file
    }

    #[test]
    fn probes_mkv_with_dts_track() {
        let mkv = synthetic_mkv();
        let len = mkv.len() as u64;
        let report = probe(Cursor::new(mkv), len, true).expect("probe ok");
        assert_eq!(report.container, "Matroska");
        assert_eq!(report.tracks.len(), 1);
        let t = &report.tracks[0];
        assert_eq!(t.codec, "DTS");
        assert_eq!(t.sample_rate, Some(48000));
        assert_eq!(t.bit_depth, Some(24)); // from the sampled core frame
        assert_eq!(t.channels, Some(6));
        assert_eq!(t.language.as_deref(), Some("ger"));
    }

    #[test]
    fn fast_mode_uses_container_metadata() {
        let mkv = synthetic_mkv();
        let len = mkv.len() as u64;
        let report = probe(Cursor::new(mkv), len, false).expect("probe ok");
        let t = &report.tracks[0];
        assert_eq!(t.codec, "DTS");
        assert_eq!(t.sample_rate, Some(48000)); // container float
        assert_eq!(t.bit_depth, None); // no BitDepth element, no sampling
        assert_eq!(t.note.as_deref(), Some("container metadata only"));
    }
}
