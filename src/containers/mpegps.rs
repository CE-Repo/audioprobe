//! Native MPEG program stream parser (`.mpg .mpeg .vob .ps`, and the elementary
//! payload of DVD-Video VOBs inside an ISO).
//!
//! Walks pack / system / PES layers, collects the payload of each audio stream —
//! MPEG audio (stream id `0xC0..=0xDF`) and the DVD `private_stream_1`
//! sub-streams (`0xBD`: AC-3 `0x80..=0x87`, DTS `0x88..=0x8F`, LPCM
//! `0xA0..=0xA7`) — and hands it to the codec header parsers.

use std::collections::BTreeMap;
use std::io::Read;

use crate::codecs::{self, CodecInfo};
use crate::report::{Report, Track};

const PER_STREAM_CAP: usize = 256 * 1024;
const RESOLVE_AT: usize = 4096;
/// First-tier read: DVD/PS audio configuration resolves within the first packs,
/// so probe a modest head before honouring a larger `--limit-mb`.
const FIRST_TIER: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    MpegAudio,
    Ac3,
    Dts,
    Lpcm,
}

struct PsStream {
    kind: Kind,
    /// Display byte: the stream id for MPEG audio, the sub-stream id for private.
    tag: u8,
    buf: Vec<u8>,
    resolved: Option<CodecInfo>,
}

pub fn probe<R: Read>(mut reader: R, scan_limit: u64) -> Result<Report, String> {
    let cap = (scan_limit as usize).max(4096);

    // Read the first tier, parse; only pull more (up to the cap) if no audio
    // stream resolved yet.
    let mut data = read_up_to(&mut reader, FIRST_TIER.min(cap)).map_err(|e| e.to_string())?;
    if !data.starts_with(&[0x00, 0x00, 0x01]) {
        return Err("not an MPEG program stream".into());
    }
    let mut streams = BTreeMap::new();
    parse(&data, &mut streams);
    let resolved_any = streams.values().any(|s| s.resolved.is_some());
    if !resolved_any && data.len() >= FIRST_TIER && cap > data.len() {
        let more = read_up_to(&mut reader, cap - data.len()).map_err(|e| e.to_string())?;
        data.extend_from_slice(&more);
        streams.clear();
        parse(&data, &mut streams);
    }

    let mut report = Report {
        container: "MPEG-PS".into(),
        ..Report::default()
    };
    for s in streams.values_mut() {
        let info = s.resolved.take().or_else(|| parse_codec(s.kind, &s.buf));
        report.tracks.push(build_track(s, info));
    }
    Ok(report)
}

fn read_up_to<R: Read>(r: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; limit];
    let mut n = 0;
    while n < limit {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(n);
    Ok(buf)
}

/// Single forward pass over the buffered program stream.
fn parse(data: &[u8], streams: &mut BTreeMap<u16, PsStream>) {
    let len = data.len();
    let mut i = 0;
    while i + 4 <= len {
        if data[i] != 0 || data[i + 1] != 0 || data[i + 2] != 1 {
            i += 1; // resync to the next start code prefix
            continue;
        }
        let start = i;
        let sid = data[i + 3];
        match sid {
            0xBA => i = skip_pack_header(data, i),
            0xB9 => break, // MPEG_program_end_code
            // Padding, system header, program stream map/directory: length-prefixed.
            0xBB | 0xBC | 0xBE | 0xBF | 0xF0..=0xFF => i = skip_length_prefixed(data, i),
            // Audio / private / video PES packets.
            0xBD | 0xC0..=0xEF => {
                let (payload, next) = pes_payload(data, i);
                if let Some(payload) = payload {
                    dispatch(sid, payload, streams);
                }
                i = next;
            }
            _ => i = skip_length_prefixed(data, i),
        }
        if i <= start {
            i = start + 1; // guarantee forward progress on any malformed packet
        }
    }
}

/// Advance past a pack header (start code `0x000001BA` at `i`).
fn skip_pack_header(data: &[u8], i: usize) -> usize {
    match data.get(i + 4) {
        // MPEG-2 pack: 4-byte start code + 10 bytes, then a 3-bit stuffing count.
        Some(&b) if b & 0xC0 == 0x40 => {
            let stuffing = data.get(i + 13).map_or(0, |&s| (s & 0x07) as usize);
            (i + 14 + stuffing).min(data.len())
        }
        // MPEG-1 pack: 4-byte start code + 8 bytes.
        Some(&b) if b & 0xF0 == 0x20 => (i + 12).min(data.len()),
        _ => next_start_code(data, i + 4),
    }
}

/// Advance past a length-prefixed packet (`0x000001 xx len16 body`).
fn skip_length_prefixed(data: &[u8], i: usize) -> usize {
    match data.get(i + 4..i + 6) {
        Some(l) => (i + 6 + u16::from_be_bytes([l[0], l[1]]) as usize).min(data.len()),
        None => data.len(),
    }
}

/// Extract a PES packet's elementary payload and the offset of the next packet.
fn pes_payload(data: &[u8], i: usize) -> (Option<&[u8]>, usize) {
    let Some(l) = data.get(i + 4..i + 6) else {
        return (None, data.len());
    };
    let packet_len = u16::from_be_bytes([l[0], l[1]]) as usize;
    let pkt_end = if packet_len == 0 {
        next_start_code(data, i + 6)
    } else {
        (i + 6 + packet_len).min(data.len())
    };
    let after_hdr = &data[i + 6..pkt_end];
    let payload_start = pes_header_len(after_hdr);
    let payload = after_hdr.get(payload_start..);
    (payload.filter(|p| !p.is_empty()), pkt_end)
}

/// Length of the PES header within a packet body (bytes before the payload),
/// handling both MPEG-2 and MPEG-1 framing.
fn pes_header_len(body: &[u8]) -> usize {
    if body.len() >= 3 && body[0] & 0xC0 == 0x80 {
        // MPEG-2 PES: '10' marker, flags, then PES_header_data_length.
        return 3 + body[2] as usize;
    }
    // MPEG-1 PES: up to 16 stuffing bytes, optional STD buffer, then PTS/DTS.
    let mut j = 0;
    while j < 16 && body.get(j) == Some(&0xFF) {
        j += 1;
    }
    if body.get(j).is_some_and(|&b| b & 0xC0 == 0x40) {
        j += 2; // STD buffer scale/size
    }
    match body.get(j).map(|&b| b & 0xF0) {
        Some(0x20) => j + 5,  // PTS only
        Some(0x30) => j + 10, // PTS + DTS
        _ if body.get(j) == Some(&0x0F) => j + 1,
        _ => j,
    }
}

fn dispatch(sid: u8, payload: &[u8], streams: &mut BTreeMap<u16, PsStream>) {
    match sid {
        0xC0..=0xDF => append(streams, sid as u16, Kind::MpegAudio, sid, payload),
        0xBD => {
            let Some(&sub) = payload.first() else { return };
            match sub {
                // AC-3 / DTS carry a 4-byte private header (sub id + frame info).
                0x80..=0x87 => append(
                    streams,
                    0x100 | sub as u16,
                    Kind::Ac3,
                    sub,
                    &payload[4.min(payload.len())..],
                ),
                0x88..=0x8F => append(
                    streams,
                    0x100 | sub as u16,
                    Kind::Dts,
                    sub,
                    &payload[4.min(payload.len())..],
                ),
                // LPCM: the parameters are in the 6-byte header after the sub id.
                0xA0..=0xA7 => {
                    let key = 0x100 | sub as u16;
                    let entry = streams.entry(key).or_insert_with(|| PsStream {
                        kind: Kind::Lpcm,
                        tag: sub,
                        buf: Vec::new(),
                        resolved: None,
                    });
                    if entry.resolved.is_none() {
                        entry.resolved =
                            codecs::pcm::parse_dvd_lpcm(&payload[1.min(payload.len())..]);
                    }
                }
                _ => {} // subpicture (0x20..=0x3F) and others: not audio
            }
        }
        _ => {} // video or extended: ignored
    }
}

fn append(streams: &mut BTreeMap<u16, PsStream>, key: u16, kind: Kind, tag: u8, data: &[u8]) {
    let entry = streams.entry(key).or_insert_with(|| PsStream {
        kind,
        tag,
        buf: Vec::new(),
        resolved: None,
    });
    if entry.buf.len() < PER_STREAM_CAP {
        entry.buf.extend_from_slice(data);
    }
    if entry.resolved.is_none() && entry.buf.len() >= RESOLVE_AT {
        // Give DTS a larger window so extension substreams classify reliably.
        let want_more = kind == Kind::Dts && entry.buf.len() < 32 * 1024;
        if !want_more {
            entry.resolved = parse_codec(kind, &entry.buf);
        }
    }
}

/// Scan forward from `from` for the next `0x000001` start-code prefix.
fn next_start_code(data: &[u8], from: usize) -> usize {
    let mut j = from;
    while j + 3 <= data.len() {
        if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
            return j;
        }
        j += 1;
    }
    data.len()
}

fn parse_codec(kind: Kind, buf: &[u8]) -> Option<CodecInfo> {
    if buf.is_empty() {
        return None;
    }
    match kind {
        Kind::MpegAudio => codecs::mpeg_audio::parse(buf),
        Kind::Ac3 => codecs::ac3::parse(buf),
        Kind::Dts => codecs::dts::parse(buf),
        Kind::Lpcm => None, // resolved from the header, never the payload
    }
}

fn fallback_name(kind: Kind) -> &'static str {
    match kind {
        Kind::MpegAudio => "MPEG Audio",
        Kind::Ac3 => "AC-3",
        Kind::Dts => "DTS",
        Kind::Lpcm => "LPCM (DVD)",
    }
}

fn build_track(s: &PsStream, info: Option<CodecInfo>) -> Track {
    let mut track = Track {
        id: format!("0x{:02X}", s.tag),
        codec: fallback_name(s.kind).into(),
        ..Track::default()
    };
    match info {
        Some(i) => {
            if let Some(name) = i.name {
                track.codec = name;
            }
            track.sample_rate = i.sample_rate;
            track.bit_depth = i.bit_depth;
            track.channels = i.channels;
            track.lfe = i.lfe;
            track.bitrate = i.bitrate;
            track.immersive = i.immersive;
            track.note = i.note;
        }
        None => track.note = Some("no payload captured in scan window".into()),
    }
    track
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_header() -> Vec<u8> {
        // MPEG-2 pack header, zero stuffing.
        let mut v = vec![0x00, 0x00, 0x01, 0xBA];
        v.extend_from_slice(&[0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0xF8]);
        v
    }

    fn pes(sid: u8, payload: &[u8]) -> Vec<u8> {
        // MPEG-2 PES: '10' marker, no flags, zero header data length.
        let mut body = vec![0x80, 0x00, 0x00];
        body.extend_from_slice(payload);
        let mut v = vec![0x00, 0x00, 0x01, sid];
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn probes_ps_with_mp2_and_ac3() {
        use crate::bits::tests_support::BitWriter;
        // AC-3 5.1 @ 48 kHz syncframe header.
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
        let mut ac3 = w.finish();
        ac3.resize(4096, 0);
        // AC-3 in private_stream_1 carries a 4-byte header (sub id + frame info).
        let mut priv_ac3 = vec![0x80, 0x01, 0x00, 0x02];
        priv_ac3.extend_from_slice(&ac3);

        // MP2 stereo @ 48 kHz.
        let mut mp2 = vec![0xFF, 0xFD, 0x94, 0x00];
        mp2.resize(4096, 0);

        let mut stream = pack_header();
        stream.extend_from_slice(&pes(0xC0, &mp2));
        stream.extend_from_slice(&pes(0xBD, &priv_ac3));

        let report = probe(&stream[..], 1024 * 1024).expect("probe ok");
        assert_eq!(report.container, "MPEG-PS");
        assert_eq!(report.tracks.len(), 2);

        // BTreeMap orders MPEG audio (key 0xC0) before private (key 0x180).
        let mp2_t = &report.tracks[0];
        assert_eq!(mp2_t.codec, "MP2");
        assert_eq!(mp2_t.sample_rate, Some(48000));
        let ac3_t = &report.tracks[1];
        assert_eq!(ac3_t.codec, "AC-3");
        assert_eq!(ac3_t.sample_rate, Some(48000));
        assert_eq!(ac3_t.channels, Some(6));
    }

    #[test]
    fn probes_ps_with_dvd_lpcm() {
        // LPCM header: quant=2 (24-bit), fs=1 (96 kHz), channels-1=1 -> 2 ch.
        let info_byte = (2 << 6) | (1 << 4) | 1;
        let mut priv_lpcm = vec![0xA0, 0x01, 0x00, 0x04, 0x00, info_byte, 0x80];
        priv_lpcm.extend_from_slice(&[0u8; 512]);

        let mut stream = pack_header();
        stream.extend_from_slice(&pes(0xBD, &priv_lpcm));

        let report = probe(&stream[..], 1024 * 1024).expect("probe ok");
        assert_eq!(report.tracks.len(), 1);
        let t = &report.tracks[0];
        assert_eq!(t.codec, "LPCM (DVD)");
        assert_eq!(t.sample_rate, Some(96000));
        assert_eq!(t.bit_depth, Some(24));
        assert_eq!(t.channels, Some(2));
    }
}
