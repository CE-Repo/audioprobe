//! Container detection and dispatch, plus probers for the simple formats
//! (FLAC, WAV, Ogg and raw elementary streams).

pub mod matroska;
pub mod mp4;
pub mod mpegts;

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::codecs::{self, CodecInfo};
use crate::report::{Report, Track};

pub struct Options {
    /// Maximum number of bytes to scan in transport streams.
    pub scan_limit: u64,
    /// Sample frames from MKV clusters for exact codec parameters.
    pub deep: bool,
}

pub fn probe_path(path: &Path, opts: &Options) -> Report {
    let display = path.display().to_string();
    match probe_inner(path, opts) {
        Ok(mut report) => {
            report.path = display;
            report
        }
        Err(err) => Report {
            path: display,
            error: Some(err),
            ..Report::default()
        },
    }
}

fn probe_inner(path: &Path, opts: &Options) -> Result<Report, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 16];
    let got = read_up_to(&mut reader, &mut magic).map_err(|e| e.to_string())?;
    let magic = &magic[..got];
    reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;

    if magic.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return matroska::probe(reader, file_len, opts.deep);
    }
    if magic.len() >= 12
        && matches!(
            &magic[4..8],
            b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
        )
    {
        return mp4::probe(reader, file_len);
    }
    if magic.starts_with(b"fLaC") || (magic.starts_with(b"ID3") && is_flac_after_id3(&mut reader)) {
        return probe_flac(reader);
    }
    if magic.starts_with(b"RIFF") && magic.len() >= 12 && &magic[8..12] == b"WAVE" {
        return probe_wav(reader);
    }
    if magic.starts_with(b"OggS") {
        return probe_ogg(reader);
    }
    // MPEG-TS detection needs a larger window (sync pattern check).
    {
        let mut head = vec![0u8; 4096];
        let got = read_up_to(&mut reader, &mut head).map_err(|e| e.to_string())?;
        head.truncate(got);
        reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        if looks_like_ts(&head) {
            return mpegts::probe(reader, opts.scan_limit);
        }
    }
    probe_elementary(reader, path)
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(m) => n += m,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn looks_like_ts(head: &[u8]) -> bool {
    for &size in &[188usize, 192, 204] {
        'start: for start in 0..size.min(head.len()) {
            if head.len() < start + size * 4 + 1 {
                break;
            }
            for i in 0..5 {
                if head[start + i * size] != 0x47 {
                    continue 'start;
                }
            }
            return true;
        }
    }
    false
}

fn is_flac_after_id3<R: Read + Seek>(reader: &mut R) -> bool {
    let mut hdr = [0u8; 10];
    if reader.seek(SeekFrom::Start(0)).is_err() || read_up_to(reader, &mut hdr).unwrap_or(0) < 10 {
        return false;
    }
    let size = synchsafe(&hdr[6..10]);
    let mut marker = [0u8; 4];
    let ok = reader.seek(SeekFrom::Start(10 + size)).is_ok()
        && read_up_to(reader, &mut marker).unwrap_or(0) == 4
        && &marker == b"fLaC";
    let _ = reader.seek(SeekFrom::Start(0));
    ok
}

fn synchsafe(b: &[u8]) -> u64 {
    b.iter()
        .fold(0u64, |acc, &x| (acc << 7) | (x & 0x7F) as u64)
}

fn single_track_report(container: &str, info: CodecInfo) -> Report {
    Report {
        container: container.into(),
        tracks: vec![Track {
            id: "1".into(),
            codec: info.name.unwrap_or_else(|| container.into()),
            sample_rate: info.sample_rate,
            bit_depth: info.bit_depth,
            channels: info.channels,
            lfe: info.lfe,
            note: info.note,
            default: true,
            ..Track::default()
        }],
        ..Report::default()
    }
}

fn probe_flac<R: Read + Seek>(mut reader: R) -> Result<Report, String> {
    let mut head = vec![0u8; 64 * 1024];
    let got = read_up_to(&mut reader, &mut head).map_err(|e| e.to_string())?;
    head.truncate(got);
    let start = if head.starts_with(b"ID3") {
        (10 + synchsafe(&head[6..10])) as usize
    } else {
        0
    };
    let info = head
        .get(start..)
        .and_then(codecs::flac::parse)
        .ok_or("invalid FLAC STREAMINFO")?;
    Ok(single_track_report("FLAC", info))
}

fn probe_wav<R: Read + Seek>(mut reader: R) -> Result<Report, String> {
    let mut head = vec![0u8; 64 * 1024];
    let got = read_up_to(&mut reader, &mut head).map_err(|e| e.to_string())?;
    head.truncate(got);
    // Walk RIFF chunks looking for 'fmt '.
    let mut p = 12usize;
    while p + 8 <= head.len() {
        let id = &head[p..p + 4];
        let len = u32::from_le_bytes([head[p + 4], head[p + 5], head[p + 6], head[p + 7]]) as usize;
        if id == b"fmt " {
            let info = head
                .get(p + 8..p + 8 + len)
                .and_then(codecs::pcm::parse_waveformatex)
                .ok_or("invalid WAV fmt chunk")?;
            let container = if head.get(p + 8..p + 10) == Some(&[0xFE, 0xFF][..]) {
                "WAV (extensible)"
            } else {
                "WAV"
            };
            return Ok(single_track_report(container, info));
        }
        p += 8 + len + (len & 1);
    }
    Err("no fmt chunk found in WAV file".into())
}

fn probe_ogg<R: Read + Seek>(mut reader: R) -> Result<Report, String> {
    let mut head = vec![0u8; 64 * 1024];
    let got = read_up_to(&mut reader, &mut head).map_err(|e| e.to_string())?;
    head.truncate(got);
    let info = codecs::xiph::parse_opus_head(&head)
        .or_else(|| codecs::xiph::parse_vorbis(&head))
        .or_else(|| {
            // Ogg FLAC: "\x7fFLAC" + version + header count, then native stream
            crate::bits::find_pattern(&head, b"\x7fFLAC")
                .and_then(|i| head.get(i + 9..))
                .and_then(codecs::flac::parse)
        })
        .ok_or("unsupported Ogg stream (no Opus/Vorbis/FLAC header found)")?;
    Ok(single_track_report("Ogg", info))
}

fn probe_elementary<R: Read + Seek>(mut reader: R, path: &Path) -> Result<Report, String> {
    let mut buf = vec![0u8; 1024 * 1024];
    let got = read_up_to(&mut reader, &mut buf).map_err(|e| e.to_string())?;
    buf.truncate(got);
    let start = if buf.starts_with(b"ID3") {
        ((10 + synchsafe(&buf[6..10])) as usize).min(buf.len())
    } else {
        0
    };
    let data = &buf[start..];

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Extension gives the priority order; magic scanning does the real work.
    let candidates: &[&str] = match ext.as_str() {
        "ac3" | "eac3" | "ec3" => &["ac3"],
        "dts" | "dtshd" | "dtsma" | "dtshr" => &["dts"],
        "thd" | "truehd" | "mlp" => &["truehd"],
        "aac" | "adts" => &["aac", "latm"],
        "latm" | "loas" => &["latm", "aac"],
        "mp3" | "mp2" | "mp1" | "mpa" => &["mpeg"],
        _ => &["ac3", "dts", "truehd", "aac", "mpeg"],
    };
    for c in candidates {
        let info = match *c {
            "ac3" => codecs::ac3::parse(data),
            "dts" => codecs::dts::parse(data),
            "truehd" => codecs::truehd::parse(data),
            "aac" => codecs::aac::parse_adts(data),
            "latm" => codecs::aac::parse_loas_latm(data),
            "mpeg" => codecs::mpeg_audio::parse(data),
            _ => None,
        };
        if let Some(info) = info {
            let name = info.name.clone().unwrap_or_else(|| "audio".into());
            let mut report = single_track_report(&format!("{} elementary stream", name), info);
            report.tracks[0].codec = name;
            return Ok(report);
        }
    }
    Err("unrecognized file format".into())
}
