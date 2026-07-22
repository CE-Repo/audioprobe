//! Native MPEG transport stream parser (188-byte TS, 192-byte M2TS/BDAV,
//! 204-byte TS with FEC).
//!
//! Finds the PAT and PMT(s), identifies audio elementary streams, collects
//! their PES payloads and hands them to the codec header parsers.

use std::collections::HashMap;
use std::io::Read;

use crate::codecs::{self, CodecInfo};
use crate::report::{Report, Track};

const PER_PID_CAP: usize = 256 * 1024;
const ENOUGH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Ac3,
    Eac3,
    Dts,
    DtsHdHra,
    DtsHdMa,
    DtsExpress,
    TrueHd,
    Aac,
    AacLatm,
    MpegAudio,
    LpcmHdmv,
    Opus,
}

struct EsStream {
    kind: Kind,
    language: Option<String>,
    buf: Vec<u8>,
    started: bool,
    resolved: Option<CodecInfo>,
}

pub fn probe<R: Read>(mut reader: R, scan_limit: u64) -> Result<Report, String> {
    // Read an initial window to detect packet size and alignment.
    let mut head = vec![0u8; 65536];
    let mut head_len = 0;
    while head_len < head.len() {
        match reader.read(&mut head[head_len..]) {
            Ok(0) => break,
            Ok(n) => head_len += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    head.truncate(head_len);

    let (pkt_size, first) =
        detect_packet_size(&head).ok_or_else(|| "no MPEG-TS sync pattern found".to_string())?;
    let container = match pkt_size {
        192 => "M2TS (BDAV)",
        204 => "MPEG-TS (204)",
        _ => "MPEG-TS",
    };

    let mut report = Report {
        container: container.into(),
        ..Report::default()
    };

    let mut pmt_pids: Vec<u16> = Vec::new();
    let mut pat_seen = false;
    let mut hdmv = false;
    let mut streams: HashMap<u16, EsStream> = HashMap::new();
    let mut pmts_parsed: Vec<u16> = Vec::new();

    let mut scanned: u64 = 0;
    let mut carry: Vec<u8> = head;
    let mut tail_padded = false;

    'outer: loop {
        // Process all complete packets in `carry`.
        let mut off = if scanned == 0 { first } else { 0 };
        while off + pkt_size <= carry.len() {
            // `first` points at the 0x47 sync byte, so every stride starts
            // on a sync byte; for 192-byte BDAV packets the 4-byte
            // TP_extra_header of the *next* packet trails the slice and is
            // ignored because only the first 188 bytes are used.
            let p = &carry[off..off + pkt_size];
            off += pkt_size;
            if p[0] != 0x47 {
                continue;
            }
            handle_packet(
                p,
                &mut pat_seen,
                &mut pmt_pids,
                &mut pmts_parsed,
                &mut hdmv,
                &mut streams,
            );
            // Early exit: PMT(s) parsed and every audio stream resolved.
            if !pmts_parsed.is_empty()
                && pmts_parsed.len() >= pmt_pids.len()
                && !streams.is_empty()
                && streams
                    .values()
                    .all(|s| s.resolved.is_some() || s.buf.len() >= ENOUGH_BYTES)
            {
                break 'outer;
            }
        }
        let rem = carry.len() - off;
        carry.copy_within(off.., 0);
        carry.truncate(rem);
        scanned += off as u64;
        if scanned >= scan_limit {
            break;
        }
        let mut chunk = vec![0u8; 512 * 1024];
        let mut n = 0;
        while n < chunk.len() {
            match reader.read(&mut chunk[n..]) {
                Ok(0) => break,
                Ok(m) => n += m,
                Err(e) => return Err(e.to_string()),
            }
        }
        if n == 0 {
            // EOF: a 192-byte BDAV stream's final packet lacks the trailing
            // TP_extra_header of a successor — pad it so it still gets parsed.
            if !tail_padded && carry.len() >= 188 && carry.len() < pkt_size {
                carry.resize(pkt_size, 0xFF);
                tail_padded = true;
                continue;
            }
            break;
        }
        chunk.truncate(n);
        carry.extend_from_slice(&chunk);
    }

    if !pat_seen {
        return Err("no PAT found in transport stream".into());
    }
    if pmts_parsed.is_empty() {
        return Err("no PMT found in transport stream".into());
    }

    // Finalize: parse whatever was collected.
    let mut pids: Vec<u16> = streams.keys().copied().collect();
    pids.sort_unstable();
    for pid in pids {
        let s = streams.get_mut(&pid).unwrap();
        let info = s.resolved.take().or_else(|| parse_payload(s.kind, &s.buf));
        report.tracks.push(build_track(pid, s, info));
    }
    Ok(report)
}

fn detect_packet_size(buf: &[u8]) -> Option<(usize, usize)> {
    // Returns (packet size, offset of the first sync byte). For 192-byte
    // BDAV packets the returned offset points at the 0x47 sync byte, i.e.
    // 4 bytes into the packet — the 4-byte TP_extra_header of every
    // subsequent packet is then carried at the *end* of the previous slice,
    // which is harmless because we only look at the first 188 bytes.
    for &size in &[188usize, 192, 204] {
        let scan = (size + 8).min(buf.len());
        'start: for start in 0..scan {
            // 5 sync bytes checked; the last sits at start + size * 4
            if buf.len() < start + size * 4 + 1 {
                break;
            }
            for i in 0..5 {
                if buf[start + i * size] != 0x47 {
                    continue 'start;
                }
            }
            return Some((size, start));
        }
    }
    None
}

fn handle_packet(
    p: &[u8],
    pat_seen: &mut bool,
    pmt_pids: &mut Vec<u16>,
    pmts_parsed: &mut Vec<u16>,
    hdmv: &mut bool,
    streams: &mut HashMap<u16, EsStream>,
) {
    if p.len() < 188 {
        return;
    }
    let pusi = p[1] & 0x40 != 0;
    let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
    let afc = (p[3] >> 4) & 3;
    if afc & 1 == 0 {
        return; // no payload
    }
    let mut off = 4;
    if afc & 2 != 0 {
        let af_len = p[4] as usize;
        off += 1 + af_len;
        if off >= 188 {
            return;
        }
    }
    let payload = &p[off..188];

    if pid == 0 {
        if pusi && !*pat_seen {
            if let Some(pids) = parse_pat(payload) {
                *pmt_pids = pids;
                *pat_seen = true;
            }
        }
        return;
    }
    if pmt_pids.contains(&pid) && !pmts_parsed.contains(&pid) {
        if pusi && parse_pmt(payload, hdmv, streams) {
            pmts_parsed.push(pid);
        }
        return;
    }
    if let Some(s) = streams.get_mut(&pid) {
        append_pes(s, payload, pusi);
        if s.resolved.is_none() && s.started && s.buf.len() >= 4096 {
            if let Some(info) = parse_payload(s.kind, &s.buf) {
                // For DTS keep collecting a bit longer so extension
                // substreams (XLL/XBR/LBR) can be classified reliably.
                let want_more = matches!(
                    s.kind,
                    Kind::Dts | Kind::DtsHdHra | Kind::DtsHdMa | Kind::DtsExpress
                ) && s.buf.len() < 32 * 1024;
                if !want_more {
                    s.resolved = Some(info);
                }
            }
        }
    }
}

fn section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = *payload.first()? as usize;
    payload.get(1 + pointer..)
}

fn parse_pat(payload: &[u8]) -> Option<Vec<u16>> {
    let s = section(payload)?;
    if s.first() != Some(&0x00) {
        return None;
    }
    let section_length = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
    let end = (3 + section_length).min(s.len());
    let mut pids = Vec::new();
    let mut i = 8;
    while i + 4 <= end.saturating_sub(4) {
        let program = ((s[i] as u16) << 8) | s[i + 1] as u16;
        let pid = (((s[i + 2] & 0x1F) as u16) << 8) | s[i + 3] as u16;
        if program != 0 {
            pids.push(pid);
        }
        i += 4;
    }
    if pids.is_empty() {
        None
    } else {
        Some(pids)
    }
}

fn parse_pmt(payload: &[u8], hdmv: &mut bool, streams: &mut HashMap<u16, EsStream>) -> bool {
    let s = match section(payload) {
        Some(s) => s,
        None => return false,
    };
    if s.first() != Some(&0x02) {
        return false;
    }
    if s.len() < 12 {
        return false;
    }
    let section_length = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
    let end = (3 + section_length).min(s.len()).saturating_sub(4); // minus CRC
    let program_info_len = (((s[10] & 0x0F) as usize) << 8) | s[11] as usize;
    if let Some(prog_desc) = s.get(12..12 + program_info_len) {
        if registration_ids(prog_desc).iter().any(|r| r == b"HDMV") {
            *hdmv = true;
        }
    }
    let mut i = 12 + program_info_len;
    while i + 5 <= end {
        let stream_type = s[i];
        let pid = (((s[i + 1] & 0x1F) as u16) << 8) | s[i + 2] as u16;
        let es_info_len = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
        let desc = s.get(i + 5..(i + 5 + es_info_len).min(end)).unwrap_or(&[]);
        i += 5 + es_info_len;
        if let Some(kind) = classify(stream_type, desc, *hdmv) {
            streams.entry(pid).or_insert_with(|| EsStream {
                kind,
                language: iso639_language(desc),
                buf: Vec::new(),
                started: false,
                resolved: None,
            });
        }
    }
    true
}

fn registration_ids(descriptors: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= descriptors.len() {
        let tag = descriptors[i];
        let len = descriptors[i + 1] as usize;
        if let Some(body) = descriptors.get(i + 2..i + 2 + len) {
            if tag == 0x05 && body.len() >= 4 {
                out.push([body[0], body[1], body[2], body[3]]);
            }
        }
        i += 2 + len;
    }
    out
}

fn iso639_language(descriptors: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 2 <= descriptors.len() {
        let tag = descriptors[i];
        let len = descriptors[i + 1] as usize;
        if let Some(body) = descriptors.get(i + 2..i + 2 + len) {
            if tag == 0x0A && body.len() >= 3 {
                let lang: String = body[0..3]
                    .iter()
                    .map(|&b| b as char)
                    .filter(|c| c.is_ascii_alphabetic())
                    .collect();
                if lang.len() == 3 {
                    return Some(lang.to_lowercase());
                }
            }
        }
        i += 2 + len;
    }
    None
}

fn classify(stream_type: u8, descriptors: &[u8], hdmv: bool) -> Option<Kind> {
    let regs = registration_ids(descriptors);
    let has_desc = |tag: u8| -> bool {
        let mut i = 0;
        while i + 2 <= descriptors.len() {
            if descriptors[i] == tag {
                return true;
            }
            i += 2 + descriptors[i + 1] as usize;
        }
        false
    };
    Some(match stream_type {
        0x03 | 0x04 => Kind::MpegAudio,
        0x0F => Kind::Aac,
        0x11 => Kind::AacLatm,
        0x06 => {
            // DVB private data: identified by descriptors
            if has_desc(0x6A) || regs.iter().any(|r| r == b"AC-3") {
                Kind::Ac3
            } else if has_desc(0x7A) {
                Kind::Eac3
            } else if has_desc(0x7B)
                || regs
                    .iter()
                    .any(|r| r == b"DTS1" || r == b"DTS2" || r == b"DTS3")
            {
                Kind::Dts
            } else if has_desc(0x7C) {
                Kind::Aac
            } else if regs.iter().any(|r| r == b"Opus") {
                Kind::Opus
            } else {
                return None;
            }
        }
        0x80 => {
            if hdmv {
                Kind::LpcmHdmv
            } else {
                Kind::Ac3 // ATSC
            }
        }
        0x81 => Kind::Ac3,
        0x82 => Kind::Dts,
        0x83 => Kind::TrueHd,
        0x84 | 0x87 | 0xA1 => Kind::Eac3,
        0x85 => Kind::DtsHdHra,
        0x86 => Kind::DtsHdMa,
        0xA2 => Kind::DtsExpress,
        _ => return None,
    })
}

fn append_pes(s: &mut EsStream, payload: &[u8], pusi: bool) {
    if s.buf.len() >= PER_PID_CAP {
        return;
    }
    if pusi {
        // Parse the PES header: 00 00 01 stream_id len(2) flags(2) hdrlen(1)
        if payload.len() < 9 || payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
            return;
        }
        let stream_id = payload[3];
        // audio (0xC0..0xDF), private stream 1 (0xBD), extended (0xFD)
        let audio_like =
            (0xC0..=0xDF).contains(&stream_id) || stream_id == 0xBD || stream_id == 0xFD;
        if !audio_like {
            return;
        }
        if payload[6] & 0xC0 != 0x80 {
            return; // not an MPEG-2 PES header
        }
        let hdr_len = payload[8] as usize;
        let data_start = 9 + hdr_len;
        if let Some(data) = payload.get(data_start..) {
            s.started = true;
            s.buf.extend_from_slice(data);
        }
    } else if s.started {
        s.buf.extend_from_slice(payload);
    }
}

fn parse_payload(kind: Kind, buf: &[u8]) -> Option<CodecInfo> {
    if buf.is_empty() {
        return None;
    }
    match kind {
        Kind::Ac3 | Kind::Eac3 => codecs::ac3::parse(buf),
        Kind::Dts | Kind::DtsHdHra | Kind::DtsHdMa | Kind::DtsExpress => codecs::dts::parse(buf),
        Kind::TrueHd => codecs::truehd::parse(buf).or_else(|| {
            // The Blu-ray TrueHD PID interleaves an AC-3 compatibility
            // stream; fall back to it if no major sync was captured.
            codecs::ac3::parse(buf).map(|mut i| {
                i.name = Some("TrueHD".into());
                i.bit_depth = None;
                // The AC-3 core's constant bit rate is not the TrueHD rate.
                i.bitrate = None;
                i.note = Some("parameters from embedded AC-3 core".into());
                i
            })
        }),
        Kind::Aac => codecs::aac::parse_adts(buf),
        Kind::AacLatm => codecs::aac::parse_loas_latm(buf),
        Kind::MpegAudio => codecs::mpeg_audio::parse(buf),
        Kind::LpcmHdmv => codecs::pcm::parse_hdmv_lpcm(buf),
        Kind::Opus => None, // DVB Opus carries no in-band OpusHead
    }
}

fn fallback_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Ac3 => "AC-3",
        Kind::Eac3 => "E-AC-3",
        Kind::Dts => "DTS",
        Kind::DtsHdHra => "DTS-HD HRA",
        Kind::DtsHdMa => "DTS-HD MA",
        Kind::DtsExpress => "DTS Express",
        Kind::TrueHd => "TrueHD",
        Kind::Aac => "AAC",
        Kind::AacLatm => "AAC (LATM)",
        Kind::MpegAudio => "MPEG Audio",
        Kind::LpcmHdmv => "LPCM (Blu-ray)",
        Kind::Opus => "Opus",
    }
}

fn build_track(pid: u16, s: &EsStream, info: Option<CodecInfo>) -> Track {
    let mut track = Track {
        id: format!("PID {} (0x{:04X})", pid, pid),
        codec: fallback_name(s.kind).into(),
        language: s.language.clone(),
        ..Track::default()
    };
    match info {
        Some(i) => {
            if let Some(name) = i.name {
                // The PMT stream type is authoritative for the DTS flavour;
                // the bitstream scan may miss extension syncs in the window.
                let keep_pmt_name =
                    matches!(s.kind, Kind::DtsHdMa | Kind::DtsHdHra | Kind::DtsExpress)
                        && name.starts_with("DTS")
                        && !name.contains("MA")
                        && !name.contains("HRA");
                if !keep_pmt_name {
                    track.codec = name;
                }
            }
            track.sample_rate = i.sample_rate;
            track.bit_depth = i.bit_depth;
            track.channels = i.channels;
            track.lfe = i.lfe;
            track.bitrate = i.bitrate;
            track.immersive = i.immersive;
            track.note = i.note;
        }
        None => {
            track.note = Some("no payload captured in scan window".into());
        }
    }
    track
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 188-byte TS packet.
    fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0xFFu8; 188];
        p[0] = 0x47;
        p[1] = (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F);
        p[2] = (pid & 0xFF) as u8;
        p[3] = 0x10 | (cc & 0xF); // payload only
        let n = payload.len().min(184);
        p[4..4 + n].copy_from_slice(&payload[..n]);
        p
    }

    fn psi(table: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8]; // pointer_field
        v.extend_from_slice(table);
        v
    }

    fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut t = vec![
            0x00, // table_id
            0xB0, 0x0D, // section_length = 13
            0x00, 0x01, // transport_stream_id
            0xC1, 0x00, 0x00, // version/section numbers
            0x00, 0x01, // program_number 1
        ];
        t.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
        t.push((pmt_pid & 0xFF) as u8);
        t.extend_from_slice(&[0, 0, 0, 0]); // CRC (unchecked)
        psi(&t)
    }

    fn pmt(es: &[(u8, u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![
            0x00, 0x01, // program_number
            0xC1, 0x00, 0x00, // version/section numbers
            0xE1, 0x00, // PCR PID
            0xF0, 0x00, // program_info_length = 0
        ];
        for (st, pid, desc) in es {
            body.push(*st);
            body.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            body.push((pid & 0xFF) as u8);
            body.push(0xF0 | ((desc.len() >> 8) as u8 & 0x0F));
            body.push((desc.len() & 0xFF) as u8);
            body.extend_from_slice(desc);
        }
        let section_len = body.len() + 4; // + CRC
        let mut t = vec![
            0x02,
            0xB0 | ((section_len >> 8) as u8 & 0x0F),
            (section_len & 0xFF) as u8,
        ];
        t.extend_from_slice(&body);
        t.extend_from_slice(&[0, 0, 0, 0]); // CRC
        psi(&t)
    }

    fn pes(stream_id: u8, es_data: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x00, 0x00];
        v.extend_from_slice(es_data);
        v
    }

    #[test]
    fn probes_ts_with_ac3_and_mp2() {
        use crate::bits::tests_support::BitWriter;
        // AC-3 5.1 @ 48 kHz header
        let mut w = BitWriter::new();
        w.put(16, 0x0B77);
        w.put(16, 0);
        w.put(2, 0);
        w.put(6, 20);
        w.put(5, 8);
        w.put(3, 0);
        w.put(3, 7);
        w.put(2, 0);
        w.put(2, 0);
        w.put(1, 1);
        w.put(32, 0);
        let ac3 = w.finish();
        // MP2 stereo @ 48 kHz
        let mp2 = [0xFF, 0xFD, 0x94, 0x00];

        let lang_deu: &[u8] = &[0x0A, 0x04, b'd', b'e', b'u', 0x00];
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, 0, &pat(0x100)));
        stream.extend_from_slice(&ts_packet(
            0x100,
            true,
            0,
            &pmt(&[(0x81, 0x110, lang_deu), (0x03, 0x111, &[])]),
        ));
        let mut ac3_es = ac3.clone();
        ac3_es.resize(4096, 0);
        stream.extend_from_slice(&ts_packet(0x110, true, 0, &pes(0xBD, &ac3_es)));
        let mut mp2_es = mp2.to_vec();
        mp2_es.resize(4096, 0);
        stream.extend_from_slice(&ts_packet(0x111, true, 0, &pes(0xC0, &mp2_es)));
        // pad with null packets so packet-size detection sees enough syncs
        for i in 0..8 {
            stream.extend_from_slice(&ts_packet(0x1FFF, false, i, &[]));
        }

        let report = probe(&stream[..], 1024 * 1024).expect("probe ok");
        assert_eq!(report.container, "MPEG-TS");
        assert_eq!(report.tracks.len(), 2);
        let ac3_track = &report.tracks[0];
        assert_eq!(ac3_track.codec, "AC-3");
        assert_eq!(ac3_track.sample_rate, Some(48000));
        assert_eq!(ac3_track.channels, Some(6));
        assert_eq!(ac3_track.language.as_deref(), Some("deu"));
        let mp2_track = &report.tracks[1];
        assert_eq!(mp2_track.codec, "MP2");
        assert_eq!(mp2_track.sample_rate, Some(48000));
    }

    #[test]
    fn detects_m2ts_packets() {
        let mut stream = Vec::new();
        for i in 0..8 {
            stream.extend_from_slice(&[0, 0, 0, 0]); // TP_extra_header
            stream.extend_from_slice(&ts_packet(0x1FFF, false, i, &[]));
        }
        let (size, first) = detect_packet_size(&stream).expect("detected");
        assert_eq!(size, 192);
        assert_eq!(first, 4);
    }
}
