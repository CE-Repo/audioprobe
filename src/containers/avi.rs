//! Native AVI (RIFF) parser.
//!
//! Walks the `hdrl` header list to each stream's `strl`, and for every audio
//! stream (`strh` with fccType `auds`) reads the `strf` chunk — a
//! WAVEFORMATEX — for codec, sample rate, bit depth and channels. The huge
//! `movi` payload is never touched: everything needed sits in the front header.

use std::io::{Read, Seek};

use crate::codecs;
use crate::report::{Report, Track};

/// The `hdrl` header list lives at the front of the file; cap the head read so
/// a file with an implausibly large header can't pull in the whole `movi`.
const HEAD_BYTES: usize = 4 * 1024 * 1024;

pub fn probe<R: Read + Seek>(mut reader: R, _file_len: u64) -> Result<Report, String> {
    let mut head = vec![0u8; HEAD_BYTES];
    let got = super::read_up_to(&mut reader, &mut head).map_err(|e| e.to_string())?;
    head.truncate(got);

    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"AVI " {
        return Err("not an AVI file".into());
    }

    let mut report = Report {
        container: "AVI".into(),
        ..Report::default()
    };

    let (hdrl_start, hdrl_end) =
        find_list(&head, 12, head.len(), b"hdrl").ok_or("no hdrl header list found in AVI")?;

    let mut index = 0u32;
    let mut pos = hdrl_start;
    while let Some((id, size, body)) = chunk_at(&head, pos, hdrl_end) {
        if &id == b"LIST" && head.get(body..body + 4) == Some(b"strl") {
            index += 1;
            if let Some(track) = parse_strl(&head, body + 4, (body + size).min(hdrl_end), index) {
                report.tracks.push(track);
            }
        }
        pos = advance(pos, size, hdrl_end);
        if pos <= body.saturating_sub(8) {
            break; // malformed size that didn't advance
        }
    }

    Ok(report)
}

/// One stream list. Its `strh` names the stream type (we want `auds`) and its
/// `strf` is the format — WAVEFORMATEX for audio. An optional `strn` carries a
/// human-readable stream name we surface as the track title.
fn parse_strl(buf: &[u8], start: usize, end: usize, index: u32) -> Option<Track> {
    let mut is_audio = false;
    let mut fmt: Option<codecs::CodecInfo> = None;
    let mut title: Option<String> = None;

    let mut pos = start;
    while let Some((id, size, body)) = chunk_at(buf, pos, end) {
        let data = buf.get(body..(body + size).min(end)).unwrap_or(&[]);
        match &id {
            b"strh" => {
                // fccType is the first 4 bytes of the stream header.
                if data.len() >= 4 && &data[0..4] == b"auds" {
                    is_audio = true;
                }
            }
            b"strf" => {
                fmt = codecs::pcm::parse_waveformatex(data);
            }
            b"strn" => {
                let s: String = data
                    .iter()
                    .take_while(|&&b| b != 0)
                    .map(|&b| b as char)
                    .collect();
                let s = s.trim().to_string();
                if !s.is_empty() {
                    title = Some(s);
                }
            }
            _ => {}
        }
        pos = advance(pos, size, end);
        if pos <= body.saturating_sub(8) {
            break;
        }
    }

    if !is_audio {
        return None;
    }
    let mut track = Track {
        id: format!("stream {}", index),
        codec: "audio".into(),
        title,
        ..Track::default()
    };
    if let Some(info) = fmt {
        track.codec = info.name.unwrap_or_else(|| "audio".into());
        track.sample_rate = info.sample_rate;
        track.bit_depth = info.bit_depth;
        track.channels = info.channels;
        track.lfe = info.lfe;
    } else {
        track.note = Some("audio stream with no readable strf format".into());
    }
    Some(track)
}

/// Find the first `LIST` of the given type within `[start, end)`, returning the
/// byte range of its contents (after the 4-byte list type).
fn find_list(buf: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = start;
    while let Some((id, size, body)) = chunk_at(buf, pos, end) {
        if &id == b"LIST" && buf.get(body..body + 4) == Some(want) {
            return Some((body + 4, (body + size).min(end)));
        }
        let next = advance(pos, size, end);
        if next <= pos {
            break;
        }
        pos = next;
    }
    None
}

/// The chunk at `pos`: its 4-byte id, declared body size, and body start.
/// `None` if the header or body would run past `end`.
fn chunk_at(buf: &[u8], pos: usize, end: usize) -> Option<([u8; 4], usize, usize)> {
    if pos + 8 > end {
        return None;
    }
    let id = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
    let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
    let body = pos + 8;
    if body > end {
        return None;
    }
    Some((id, size, body))
}

/// Advance past a chunk: 8-byte header + body + RIFF's even-alignment pad byte.
fn advance(pos: usize, size: usize, end: usize) -> usize {
    (pos + 8 + size + (size & 1)).min(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() & 1 == 1 {
            v.push(0);
        }
        v
    }

    fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = kind.to_vec();
        inner.extend_from_slice(body);
        chunk(b"LIST", &inner)
    }

    #[test]
    fn probes_avi_pcm_stereo() {
        // WAVEFORMATEX: PCM, stereo, 48 kHz, 16-bit.
        let mut wfx = vec![0u8; 16];
        wfx[0..2].copy_from_slice(&1u16.to_le_bytes());
        wfx[2..4].copy_from_slice(&2u16.to_le_bytes());
        wfx[4..8].copy_from_slice(&48000u32.to_le_bytes());
        wfx[14..16].copy_from_slice(&16u16.to_le_bytes());

        let mut strh = b"auds".to_vec();
        strh.extend_from_slice(&[0u8; 52]); // rest of AVISTREAMHEADER, unused

        let strl = list(b"strl", &[chunk(b"strh", &strh), chunk(b"strf", &wfx)].concat());
        let hdrl = list(b"hdrl", &strl);

        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(&hdrl);
        // a tiny fake movi that must never be parsed
        body.extend_from_slice(&list(b"movi", &chunk(b"00wb", &[0u8; 8])));
        let file = chunk(b"RIFF", &body);

        let report = probe(Cursor::new(file), 0).expect("probe ok");
        assert_eq!(report.container, "AVI");
        assert_eq!(report.tracks.len(), 1);
        let t = &report.tracks[0];
        assert_eq!(t.codec, "PCM");
        assert_eq!(t.sample_rate, Some(48000));
        assert_eq!(t.bit_depth, Some(16));
        assert_eq!(t.channels, Some(2));
    }

    #[test]
    fn ignores_video_only_avi() {
        let mut strh = b"vids".to_vec();
        strh.extend_from_slice(&[0u8; 52]);
        let strl = list(b"strl", &chunk(b"strh", &strh));
        let hdrl = list(b"hdrl", &strl);
        let mut body = b"AVI ".to_vec();
        body.extend_from_slice(&hdrl);
        let file = chunk(b"RIFF", &body);

        let report = probe(Cursor::new(file), 0).expect("probe ok");
        assert!(report.tracks.is_empty());
    }
}
