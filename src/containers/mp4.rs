//! Native ISO BMFF (MP4 / M4A / MOV) parser: walks moov → trak → mdia →
//! minf → stbl → stsd and reads the audio sample entries plus their codec
//! configuration boxes (esds, dac3, dec3, ddts, dfLa, dOps, alac).

use std::io::{Read, Seek, SeekFrom};

use crate::bits::BitReader;
use crate::codecs::{self, CodecInfo};
use crate::report::{Report, Track};

struct Boxes<'a, R> {
    r: &'a mut R,
    pos: u64,
    end: u64,
}

struct BoxHeader {
    kind: [u8; 4],
    body_start: u64,
    body_end: u64,
}

impl<'a, R: Read + Seek> Boxes<'a, R> {
    fn next(&mut self) -> Option<BoxHeader> {
        if self.pos + 8 > self.end {
            return None;
        }
        self.r.seek(SeekFrom::Start(self.pos)).ok()?;
        let mut hdr = [0u8; 8];
        self.r.read_exact(&mut hdr).ok()?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (size, hdr_len) = if size32 == 1 {
            let mut big = [0u8; 8];
            self.r.read_exact(&mut big).ok()?;
            (u64::from_be_bytes(big), 16)
        } else if size32 == 0 {
            (self.end - self.pos, 8)
        } else {
            (size32, 8)
        };
        if size < hdr_len {
            return None;
        }
        let h = BoxHeader {
            kind,
            body_start: self.pos + hdr_len,
            body_end: (self.pos + size).min(self.end),
        };
        self.pos += size;
        Some(h)
    }
}

fn read_range<R: Read + Seek>(r: &mut R, start: u64, end: u64, cap: usize) -> Vec<u8> {
    let len = ((end.saturating_sub(start)) as usize).min(cap);
    let mut buf = vec![0u8; len];
    if r.seek(SeekFrom::Start(start)).is_err() || r.read_exact(&mut buf).is_err() {
        return Vec::new();
    }
    buf
}

pub fn probe<R: Read + Seek>(mut reader: R, file_len: u64) -> Result<Report, String> {
    let mut report = Report {
        container: "MP4".into(),
        ..Report::default()
    };
    let mut top = Boxes {
        r: &mut reader,
        pos: 0,
        end: file_len,
    };
    let mut moov: Option<(u64, u64)> = None;
    while let Some(b) = top.next() {
        match &b.kind {
            b"ftyp" => {
                let body = read_range(top.r, b.body_start, b.body_end, 16);
                if body.len() >= 4 {
                    let brand = String::from_utf8_lossy(&body[0..4]).to_string();
                    match brand.as_str() {
                        "qt  " => report.container = "QuickTime".into(),
                        "M4A " => report.container = "M4A".into(),
                        _ => {}
                    }
                }
            }
            b"moov" => {
                moov = Some((b.body_start, b.body_end));
                break;
            }
            _ => {}
        }
    }
    let (moov_start, moov_end) = moov.ok_or_else(|| "no moov box found".to_string())?;

    let mut track_index = 0u32;
    let mut traks: Vec<(u64, u64)> = Vec::new();
    {
        let mut boxes = Boxes {
            r: &mut reader,
            pos: moov_start,
            end: moov_end,
        };
        while let Some(b) = boxes.next() {
            if &b.kind == b"trak" {
                traks.push((b.body_start, b.body_end));
            }
        }
    }
    for (start, end) in traks {
        track_index += 1;
        if let Some(track) = parse_trak(&mut reader, start, end, track_index) {
            report.tracks.push(track);
        }
    }
    Ok(report)
}

fn find_box<R: Read + Seek>(r: &mut R, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut boxes = Boxes { r, pos: start, end };
    while let Some(b) = boxes.next() {
        if &b.kind == kind {
            return Some((b.body_start, b.body_end));
        }
    }
    None
}

fn parse_trak<R: Read + Seek>(r: &mut R, start: u64, end: u64, index: u32) -> Option<Track> {
    let (tkhd_s, tkhd_e) = find_box(r, start, end, b"tkhd")?;
    let (mdia_s, mdia_e) = find_box(r, start, end, b"mdia")?;
    let (hdlr_s, hdlr_e) = find_box(r, mdia_s, mdia_e, b"hdlr")?;
    let hdlr = read_range(r, hdlr_s, hdlr_e, 12);
    if hdlr.len() < 12 || &hdlr[8..12] != b"soun" {
        return None; // not an audio track
    }
    let tkhd = read_range(r, tkhd_s, tkhd_e, 24);
    let track_id = if !tkhd.is_empty() {
        let off = if tkhd[0] == 1 { 20 } else { 12 };
        tkhd.get(off..off + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    } else {
        None
    };

    let language = find_box(r, mdia_s, mdia_e, b"mdhd").and_then(|(s, e)| {
        let mdhd = read_range(r, s, e, 36);
        let off = if mdhd.first() == Some(&1) { 28 } else { 20 };
        let packed = u16::from_be_bytes([*mdhd.get(off)?, *mdhd.get(off + 1)?]);
        if packed == 0 || packed == 0x7FFF {
            return None;
        }
        let chars: Vec<u8> = (0..3)
            .map(|i| (((packed >> (10 - i * 5)) & 0x1F) as u8) + 0x60)
            .collect();
        let s = String::from_utf8_lossy(&chars).to_string();
        if s.chars().all(|c| c.is_ascii_lowercase()) && s != "```" {
            Some(s)
        } else {
            None
        }
    });

    let (minf_s, minf_e) = find_box(r, mdia_s, mdia_e, b"minf")?;
    let (stbl_s, stbl_e) = find_box(r, minf_s, minf_e, b"stbl")?;
    let (stsd_s, stsd_e) = find_box(r, stbl_s, stbl_e, b"stsd")?;
    let stsd = read_range(r, stsd_s, stsd_e, 4096);
    if stsd.len() < 16 {
        return None;
    }
    // stsd: version+flags(4) entry_count(4), then sample entries
    let entry = &stsd[8..];
    let entry_size = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]) as usize;
    let format = [entry[4], entry[5], entry[6], entry[7]];
    let entry = entry.get(..entry_size.min(entry.len()))?;

    let mut track = Track {
        id: track_id
            .map(|i| i.to_string())
            .unwrap_or_else(|| index.to_string()),
        codec: String::from_utf8_lossy(&format).trim().to_string(),
        language,
        default: true,
        ..Track::default()
    };

    // AudioSampleEntry: 8 header + 6 reserved + 2 data_ref, then version(2)…
    if entry.len() >= 36 {
        let version = u16::from_be_bytes([entry[16], entry[17]]);
        let mut channels = u16::from_be_bytes([entry[24], entry[25]]) as u32;
        let mut sample_size = u16::from_be_bytes([entry[26], entry[27]]) as u32;
        let mut rate = u32::from_be_bytes([entry[32], entry[33], entry[34], entry[35]]) >> 16;
        let mut children_off = 36usize;
        if version == 1 {
            children_off = 36 + 16;
        } else if version == 2 && entry.len() >= 72 {
            // QuickTime version 2 sample entry
            let f = f64::from_be_bytes([
                entry[40], entry[41], entry[42], entry[43], entry[44], entry[45], entry[46],
                entry[47],
            ]);
            if f.is_finite() && f > 0.0 {
                rate = f.round() as u32;
            }
            channels = u32::from_be_bytes([entry[48], entry[49], entry[50], entry[51]]);
            sample_size = u32::from_be_bytes([entry[56], entry[57], entry[58], entry[59]]);
            children_off = 72;
        }
        if rate > 0 {
            track.sample_rate = Some(rate);
        }
        if channels > 0 {
            track.channels = Some(channels);
        }

        let info = parse_sample_entry(&format, entry.get(children_off..).unwrap_or(&[]));
        apply_entry_info(&mut track, &format, sample_size, info);
    }
    Some(track)
}

fn apply_entry_info(
    track: &mut Track,
    format: &[u8; 4],
    sample_size: u32,
    info: Option<CodecInfo>,
) {
    let is_pcm = matches!(
        format,
        b"lpcm" | b"sowt" | b"twos" | b"in24" | b"in32" | b"fl32" | b"fl64" | b"raw "
    );
    track.codec = match format {
        b"mp4a" => "AAC".into(),
        b"ac-3" | b"sac3" => "AC-3".into(),
        b"ec-3" => "E-AC-3".into(),
        b"dtsc" => "DTS".into(),
        b"dtsh" => "DTS-HD".into(),
        b"dtsl" => "DTS-HD MA".into(),
        b"dtse" => "DTS Express".into(),
        b"fLaC" => "FLAC".into(),
        b"Opus" => "Opus".into(),
        b"alac" => "ALAC".into(),
        b"lpcm" | b"raw " => "PCM".into(),
        b"sowt" => "PCM (LE)".into(),
        b"twos" => "PCM (BE)".into(),
        b"in24" => "PCM 24-bit".into(),
        b"in32" => "PCM 32-bit".into(),
        b"fl32" | b"fl64" => "PCM (float)".into(),
        b"samr" => "AMR-NB".into(),
        b"sawb" => "AMR-WB".into(),
        _ => track.codec.clone(),
    };
    if is_pcm && sample_size > 0 {
        track.bit_depth = Some(match format {
            b"in24" => 24,
            b"in32" | b"fl32" => 32,
            b"fl64" => 64,
            _ => sample_size,
        });
    }
    if let Some(i) = info {
        if let Some(name) = i.name {
            track.codec = name;
        }
        if i.sample_rate.is_some() {
            track.sample_rate = i.sample_rate;
        }
        if i.bit_depth.is_some() {
            track.bit_depth = i.bit_depth;
        }
        if i.channels.is_some() {
            track.channels = i.channels;
            track.lfe = i.lfe;
        }
        if i.bitrate.is_some() {
            track.bitrate = i.bitrate;
        }
        if i.immersive.is_some() {
            track.immersive = i.immersive;
        }
        track.note = i.note;
    }
    // Uncompressed PCM has a trivially exact bit rate the config boxes never
    // spell out; derive it once all three inputs are known.
    if is_pcm && track.bitrate.is_none() {
        if let (Some(r), Some(d), Some(ch)) = (track.sample_rate, track.bit_depth, track.channels) {
            track.bitrate = Some(r * d * ch);
        }
    }
}

/// Parse the child boxes of an audio sample entry (esds, dac3, ddts, …).
fn parse_sample_entry(format: &[u8; 4], children: &[u8]) -> Option<CodecInfo> {
    let mut p = 0usize;
    while p + 8 <= children.len() {
        let size = u32::from_be_bytes([
            children[p],
            children[p + 1],
            children[p + 2],
            children[p + 3],
        ]) as usize;
        if size < 8 || p + size > children.len() {
            break;
        }
        let kind = &children[p + 4..p + 8];
        let body = &children[p + 8..p + size];
        let parsed = match kind {
            b"esds" => parse_esds(body),
            b"dac3" => parse_dac3(body),
            b"dec3" => parse_dec3(body),
            b"ddts" => parse_ddts(body, format),
            b"dfLa" => body.get(4..).and_then(codecs::flac::parse),
            b"dOps" => parse_dops(body),
            b"alac" => body.get(4..).and_then(codecs::pcm::parse_alac_config),
            b"wave" => {
                // QuickTime: config boxes nested inside a 'wave' box
                let r = parse_sample_entry(format, body);
                if r.is_some() {
                    return r;
                }
                None
            }
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }
        p += size;
    }
    None
}

fn parse_esds(body: &[u8]) -> Option<CodecInfo> {
    // body: version+flags(4), then a descriptor tree
    let b = body.get(4..)?;
    let (tag, es) = read_descriptor(b)?;
    if tag != 0x03 {
        return None;
    }
    // ES_Descriptor: ES_ID(2), flags(1) [+ optional fields we don't need
    // because the flags are almost always 0], then DecoderConfigDescriptor
    let mut off = 3;
    let flags = *es.get(2)?;
    if flags & 0x80 != 0 {
        off += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        off += 1 + *es.get(off)? as usize; // URL
    }
    if flags & 0x20 != 0 {
        off += 2; // OCR_ES_ID
    }
    let (tag, dcd) = read_descriptor(es.get(off..)?)?;
    if tag != 0x04 {
        return None;
    }
    let oti = *dcd.first()?;
    match oti {
        0x40 | 0x66 | 0x67 | 0x68 => {
            // MPEG-4 / MPEG-2 AAC: DecSpecificInfo holds the ASC
            let (tag, dsi) = read_descriptor(dcd.get(13..)?)?;
            if tag == 0x05 {
                codecs::aac::parse_asc(dsi)
            } else {
                Some(CodecInfo {
                    name: Some("AAC".into()),
                    ..CodecInfo::default()
                })
            }
        }
        0x69 | 0x6B => Some(CodecInfo {
            name: Some("MP3".into()),
            ..CodecInfo::default()
        }),
        0xA9 => Some(CodecInfo {
            name: Some("DTS".into()),
            ..CodecInfo::default()
        }),
        0xA5 => Some(CodecInfo {
            name: Some("AC-3".into()),
            ..CodecInfo::default()
        }),
        0xA6 => Some(CodecInfo {
            name: Some("E-AC-3".into()),
            ..CodecInfo::default()
        }),
        _ => None,
    }
}

/// Read an MPEG-4 descriptor: tag byte + expandable length.
fn read_descriptor(b: &[u8]) -> Option<(u8, &[u8])> {
    let tag = *b.first()?;
    let mut len = 0usize;
    let mut i = 1;
    for _ in 0..4 {
        let byte = *b.get(i)?;
        i += 1;
        len = (len << 7) | (byte & 0x7F) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    Some((tag, b.get(i..(i + len).min(b.len()))?))
}

const AC3_RATES: [u32; 3] = [48000, 44100, 32000];
const ACMOD_CHANNELS: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
// AC-3 nominal bit rate in kbit/s, indexed by (frmsizecod >> 1).
const AC3_BITRATES: [u32; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

fn parse_dac3(body: &[u8]) -> Option<CodecInfo> {
    let mut r = BitReader::new(body);
    let fscod = r.read_u32(2)?;
    r.skip(5 + 3)?; // bsid, bsmod
    let acmod = r.read_u32(3)? as usize;
    let lfeon = r.read_u32(1)?;
    let frmsizecod = r.read_u32(6)?;
    Some(CodecInfo {
        name: Some("AC-3".into()),
        sample_rate: AC3_RATES.get(fscod as usize).copied(),
        bit_depth: None,
        channels: Some(ACMOD_CHANNELS[acmod] + lfeon),
        lfe: Some(lfeon == 1),
        bitrate: AC3_BITRATES
            .get((frmsizecod >> 1) as usize)
            .map(|kb| kb * 1000),
        ..CodecInfo::default()
    })
}

fn parse_dec3(body: &[u8]) -> Option<CodecInfo> {
    let mut r = BitReader::new(body);
    // data_rate is the overall nominal bit rate of the stream in kbit/s.
    let data_rate = r.read_u32(13)?;
    let num_ind_sub = r.read_u32(3)? + 1;
    let fscod = r.read_u32(2)?;
    r.skip(5 + 1 + 1 + 3)?; // bsid, reserved, asvc, bsmod
    let acmod = r.read_u32(3)? as usize;
    let lfeon = r.read_u32(1)?;
    r.skip(3)?; // reserved
    let num_dep_sub = r.read_u32(4)?;
    if num_dep_sub > 0 {
        r.skip(9)?; // chan_loc
    } else {
        r.skip(1)?; // reserved
    }
    // A single independent substream may carry a JOC (Atmos) extension,
    // flagged by flag_ec3_extension_type_a after the per-substream fields.
    let immersive = if num_ind_sub == 1 {
        match r.read_u32(7 + 1) {
            Some(v) if v & 1 == 1 => Some("Atmos".into()), // reserved(7) + flag_ec3_extension_type_a(1)
            _ => None,
        }
    } else {
        None
    };
    Some(CodecInfo {
        name: Some("E-AC-3".into()),
        sample_rate: AC3_RATES.get(fscod as usize).copied(),
        bit_depth: None,
        channels: Some(ACMOD_CHANNELS[acmod] + lfeon),
        lfe: Some(lfeon == 1),
        bitrate: if data_rate > 0 {
            Some(data_rate * 1000)
        } else {
            None
        },
        immersive,
        note: None,
    })
}

fn parse_ddts(body: &[u8], format: &[u8; 4]) -> Option<CodecInfo> {
    if body.len() < 13 {
        return None;
    }
    // DTSSpecificBox: SamplingFrequency(32) maxBitrate(32) avgBitrate(32)
    // pcmSampleDepth(8) …
    let rate = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let avg_bitrate = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let depth = body[12] as u32;
    let name = match format {
        b"dtsh" => "DTS-HD",
        b"dtsl" => "DTS-HD MA",
        b"dtse" => "DTS Express",
        _ => "DTS",
    };
    Some(CodecInfo {
        name: Some(name.into()),
        sample_rate: if rate > 0 { Some(rate) } else { None },
        bit_depth: if depth > 0 { Some(depth) } else { None },
        channels: None,
        lfe: None,
        bitrate: if avg_bitrate > 0 {
            Some(avg_bitrate)
        } else {
            None
        },
        ..CodecInfo::default()
    })
}

fn parse_dops(body: &[u8]) -> Option<CodecInfo> {
    // dOps: version(1), channel count(1), pre-skip(2), input rate(4) …
    let channels = *body.get(1)? as u32;
    Some(CodecInfo {
        name: Some("Opus".into()),
        sample_rate: Some(48000),
        bit_depth: None,
        channels: if channels > 0 { Some(channels) } else { None },
        lfe: None,
        ..CodecInfo::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn sample_entry(
        format: &[u8; 4],
        channels: u16,
        size: u16,
        rate: u32,
        children: &[u8],
    ) -> Vec<u8> {
        let mut body = vec![0u8; 28]; // reserved(6)+dref(2)+version..(8)+ch/size/etc(12)
        body[16..18].copy_from_slice(&channels.to_be_bytes());
        body[18..20].copy_from_slice(&size.to_be_bytes());
        body[24..28].copy_from_slice(&(rate << 16).to_be_bytes());
        body.extend_from_slice(children);
        boxed(format, &body)
    }

    fn minimal_mp4(entry: Vec<u8>) -> Vec<u8> {
        let mut stsd_body = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd_body.extend_from_slice(&entry);
        let stsd = boxed(b"stsd", &stsd_body);
        let stbl = boxed(b"stbl", &stsd);
        let minf = boxed(b"minf", &stbl);
        let mut hdlr_body = vec![0u8; 8];
        hdlr_body.extend_from_slice(b"soun");
        hdlr_body.extend_from_slice(&[0u8; 12]);
        let hdlr = boxed(b"hdlr", &hdlr_body);
        let mut mdhd_body = vec![0u8; 20];
        // language "deu": d=4,e=5,u=21 -> (4<<10)|(5<<5)|21
        let lang: u16 = (4 << 10) | (5 << 5) | 21;
        mdhd_body.extend_from_slice(&lang.to_be_bytes());
        mdhd_body.extend_from_slice(&[0, 0]);
        let mdhd = boxed(b"mdhd", &mdhd_body);
        let mut mdia_body = mdhd;
        mdia_body.extend_from_slice(&hdlr);
        mdia_body.extend_from_slice(&minf);
        let mdia = boxed(b"mdia", &mdia_body);
        let mut tkhd_body = vec![0u8; 12];
        tkhd_body.extend_from_slice(&2u32.to_be_bytes()); // track id 2
        tkhd_body.extend_from_slice(&[0u8; 8]);
        let tkhd = boxed(b"tkhd", &tkhd_body);
        let mut trak_body = tkhd;
        trak_body.extend_from_slice(&mdia);
        let trak = boxed(b"trak", &trak_body);
        let moov = boxed(b"moov", &trak);
        let ftyp = boxed(b"ftyp", b"isomiso2");
        let mut file = ftyp;
        file.extend_from_slice(&moov);
        file
    }

    #[test]
    fn probes_mp4_with_ac3_track() {
        // dac3: fscod=0 (48k), bsid=8, bsmod=0, acmod=7, lfeon=1
        let mut dac3 = Vec::new();
        // MSB-aligned in the top 3 bytes of the u32
        let bits: u32 = (8 << 25) | (7 << 19) | (1 << 18);
        dac3.extend_from_slice(&bits.to_be_bytes()[0..3]);
        let entry = sample_entry(b"ac-3", 6, 16, 48000, &boxed(b"dac3", &dac3));
        let file = minimal_mp4(entry);
        let len = file.len() as u64;
        let report = probe(Cursor::new(file), len).expect("probe ok");
        assert_eq!(report.tracks.len(), 1);
        let t = &report.tracks[0];
        assert_eq!(t.codec, "AC-3");
        assert_eq!(t.sample_rate, Some(48000));
        assert_eq!(t.channels, Some(6));
        assert_eq!(t.language.as_deref(), Some("deu"));
        assert_eq!(t.id, "2");
    }

    #[test]
    fn probes_mp4_with_eac3_atmos_track() {
        // dec3: data_rate=768, one independent substream (fscod=0, acmod=7,
        // lfeon=1, no dependent substreams), then the JOC extension flag.
        let dec3 = {
            use crate::bits::tests_support::BitWriter;
            let mut w = BitWriter::new();
            w.put(13, 768); // data_rate (kbit/s)
            w.put(3, 0); // num_ind_sub - 1 -> 1 substream
            w.put(2, 0); // fscod = 48 kHz
            w.put(5, 16); // bsid
            w.put(1, 0); // reserved
            w.put(1, 0); // asvc
            w.put(3, 0); // bsmod
            w.put(3, 7); // acmod = 3/2
            w.put(1, 1); // lfeon
            w.put(3, 0); // reserved
            w.put(4, 0); // num_dep_sub = 0
            w.put(1, 0); // reserved (num_dep_sub == 0 branch)
            w.put(7, 0); // reserved
            w.put(1, 1); // flag_ec3_extension_type_a -> Atmos
            w.put(8, 16); // complexity_index_type_a (object count)
            w.finish()
        };
        let entry = sample_entry(b"ec-3", 6, 16, 48000, &boxed(b"dec3", &dec3));
        let file = minimal_mp4(entry);
        let len = file.len() as u64;
        let report = probe(Cursor::new(file), len).expect("probe ok");
        let t = &report.tracks[0];
        assert_eq!(t.codec, "E-AC-3");
        assert_eq!(t.channels, Some(6));
        assert_eq!(t.lfe, Some(true));
        assert_eq!(t.bitrate, Some(768_000));
        assert_eq!(t.immersive.as_deref(), Some("Atmos"));
        assert_eq!(t.codec_display(), "E-AC-3 Atmos");
    }

    #[test]
    fn probes_mp4_with_aac_track() {
        // esds with ASC: AAC-LC, 44.1 kHz, stereo
        let asc = {
            use crate::bits::tests_support::BitWriter;
            let mut w = BitWriter::new();
            w.put(5, 2);
            w.put(4, 4);
            w.put(4, 2);
            w.put(3, 0);
            w.finish()
        };
        let mut dsi = vec![0x05, asc.len() as u8];
        dsi.extend_from_slice(&asc);
        let mut dcd = vec![0x04, (13 + dsi.len()) as u8, 0x40, 0x15];
        dcd.extend_from_slice(&[0; 11]); // buffer/bitrates
        dcd.extend_from_slice(&dsi);
        let mut es = vec![0x03, (3 + dcd.len()) as u8, 0, 1, 0];
        es.extend_from_slice(&dcd);
        let mut esds_body = vec![0, 0, 0, 0];
        esds_body.extend_from_slice(&es);
        let entry = sample_entry(b"mp4a", 2, 16, 44100, &boxed(b"esds", &esds_body));
        let file = minimal_mp4(entry);
        let len = file.len() as u64;
        let report = probe(Cursor::new(file), len).expect("probe ok");
        let t = &report.tracks[0];
        assert_eq!(t.codec, "AAC-LC");
        assert_eq!(t.sample_rate, Some(44100));
        assert_eq!(t.channels, Some(2));
    }
}
