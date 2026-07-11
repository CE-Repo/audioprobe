//! DTS core substream and DTS extension substream (DTS-HD) header parsing.

use super::CodecInfo;
use crate::bits::{contains_pattern, find_pattern, BitReader};

const CORE_SYNC: [u8; 4] = [0x7F, 0xFE, 0x80, 0x01];
const EXSS_SYNC: [u8; 4] = [0x64, 0x58, 0x20, 0x25];
const XLL_SYNC: [u8; 4] = [0x41, 0xA2, 0x95, 0x47]; // lossless extension (DTS-HD MA)
const XBR_SYNC: [u8; 4] = [0x65, 0x5E, 0x31, 0x5E]; // extended bitrate (DTS-HD HRA)
const LBR_SYNC: [u8; 4] = [0x0A, 0x80, 0x19, 0x21]; // low bitrate (DTS Express)

// SFREQ -> Hz (core substream)
const CORE_RATES: [u32; 16] = [
    0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0,
];
// nuMaxSampleRate -> Hz (extension substream)
const EXSS_RATES: [u32; 16] = [
    8000, 16000, 32000, 64000, 128000, 22050, 44100, 88200, 176400, 352800, 12000, 24000, 48000,
    96000, 192000, 384000,
];
// AMODE -> channel count (core substream)
const AMODE_CHANNELS: [u32; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
// PCMR -> source resolution in bits (0 = invalid)
const PCMR_BITS: [u32; 8] = [16, 16, 20, 20, 0, 24, 24, 0];

struct Core {
    rate: u32,
    depth: Option<u32>,
    channels: u32,
    lfe: bool,
    ext_audio_id: Option<u32>, // Some(_) if the core carries a core extension
}

struct Exss {
    rate: Option<u32>,
    depth: Option<u32>,
    channels: Option<u32>,
}

pub fn parse(buf: &[u8]) -> Option<CodecInfo> {
    let core = find_pattern(buf, &CORE_SYNC).and_then(|i| parse_core(&buf[i..]));
    let exss_pos = find_pattern(buf, &EXSS_SYNC);
    let exss = exss_pos.and_then(|i| parse_exss(&buf[i..]));

    if core.is_none() && exss_pos.is_none() {
        return None;
    }

    let xll = contains_pattern(buf, &XLL_SYNC);
    let xbr = contains_pattern(buf, &XBR_SYNC);
    let lbr = contains_pattern(buf, &LBR_SYNC);

    let name = if xll {
        "DTS-HD MA"
    } else if xbr {
        "DTS-HD HRA"
    } else if lbr && core.is_none() {
        "DTS Express"
    } else if exss_pos.is_some() {
        "DTS-HD"
    } else {
        match core.as_ref().and_then(|c| c.ext_audio_id) {
            Some(0) | Some(3) => "DTS-ES", // XCH / XXCH
            Some(2) => "DTS 96/24",        // X96
            _ => "DTS",
        }
    };

    let mut info = CodecInfo {
        name: Some(name.into()),
        ..CodecInfo::default()
    };
    if let Some(c) = &core {
        info.sample_rate = Some(c.rate);
        info.bit_depth = c.depth;
        info.channels = Some(c.channels + c.lfe as u32);
        info.lfe = Some(c.lfe);
    }
    if let Some(e) = &exss {
        // The extension substream asset header describes the full (max)
        // quality of the track, so it wins over the core values.
        if e.rate.is_some() {
            info.sample_rate = e.rate;
        }
        if e.depth.is_some() {
            info.bit_depth = e.depth;
        }
        if let Some(ch) = e.channels {
            if ch >= info.channels.unwrap_or(0) {
                info.channels = Some(ch);
                if info.lfe.is_none() {
                    info.lfe = match ch {
                        6 => Some(true), // almost always 5.1
                        8 => Some(true), // almost always 7.1
                        _ => None,
                    };
                }
            }
        }
    }
    Some(info)
}

fn parse_core(b: &[u8]) -> Option<Core> {
    let mut r = BitReader::new(b);
    r.skip(32)?; // sync
    r.skip(1 + 5)?; // FTYPE, SHORT
    let cpf = r.read_u32(1)?;
    let nblks = r.read_u32(7)?;
    if nblks < 5 {
        return None;
    }
    let fsize = r.read_u32(14)?;
    if fsize < 95 {
        return None;
    }
    let amode = r.read_u32(6)? as usize;
    let sfreq = r.read_u32(4)? as usize;
    let rate = CORE_RATES[sfreq];
    if rate == 0 {
        return None;
    }
    r.skip(5)?; // RATE
    r.skip(1 + 1 + 1 + 1 + 1)?; // MIX, DYNF, TIMEF, AUXF, HDCD
    let ext_audio_id = r.read_u32(3)?;
    let ext_audio = r.read_u32(1)?;
    r.skip(1)?; // ASPF
    let lff = r.read_u32(2)?;
    r.skip(1)?; // HFLAG
    if cpf == 1 {
        r.skip(16)?; // HCRC
    }
    r.skip(1 + 4 + 2)?; // FILTS, VERNUM, CHIST
    let pcmr = r.read_u32(3)? as usize;
    let depth = match PCMR_BITS[pcmr] {
        0 => None,
        d => Some(d),
    };
    let channels = if amode < 16 {
        AMODE_CHANNELS[amode]
    } else {
        return None; // user-defined layouts
    };
    Some(Core {
        rate,
        depth,
        channels,
        lfe: lff == 1 || lff == 2,
        ext_audio_id: if ext_audio == 1 {
            Some(ext_audio_id)
        } else {
            None
        },
    })
}

/// Parse the DTS extension substream header up to the first asset descriptor,
/// which carries bit resolution, max sample rate and total channel count.
fn parse_exss(b: &[u8]) -> Option<Exss> {
    let mut r = BitReader::new(b);
    r.skip(32)?; // sync
    r.skip(8)?; // UserDefinedBits
    let exss_index = r.read_u32(2)?;
    let header_size_type = r.read_u32(1)?;
    let (bits4hdr, bits4fs) = if header_size_type == 0 {
        (8, 16)
    } else {
        (12, 20)
    };
    r.skip(bits4hdr)?; // nuExtSSHeaderSize
    r.skip(bits4fs)?; // nuExtSSFsize
    let static_fields = r.read_u32(1)?;
    if static_fields == 0 {
        return None; // asset metadata absent; nothing useful to read
    }
    r.skip(2 + 3)?; // nuRefClockCode, nuExSSFrameDurationCode
    if r.read_u32(1)? == 1 {
        r.skip(36)?; // timestamp
    }
    let num_audio_presnt = r.read_u32(3)? as usize + 1;
    let num_assets = r.read_u32(3)? as usize + 1;
    let mut masks = Vec::with_capacity(num_audio_presnt);
    for _ in 0..num_audio_presnt {
        masks.push(r.read_u32(exss_index + 1)?);
    }
    for mask in &masks {
        for j in 0..=exss_index {
            if (mask >> j) & 1 == 1 {
                r.skip(8)?;
            }
        }
    }
    if r.read_u32(1)? == 1 {
        // mixing metadata
        r.skip(2)?; // nuMixMetadataAdjLevel
        let bits4mask = ((r.read_u32(2)? + 1) * 4) as usize;
        let num_configs = r.read_u32(2)? as usize + 1;
        for _ in 0..num_configs {
            r.skip(bits4mask)?;
        }
    }
    for _ in 0..num_assets {
        r.skip(bits4fs)?; // nuAssetFsize
    }
    // First asset descriptor.
    r.skip(9)?; // nuAssetDescriptFsize
    r.skip(3)?; // nuAssetIndex
    if r.read_u32(1)? == 1 {
        r.skip(4)?; // nuAssetTypeDescriptor
    }
    if r.read_u32(1)? == 1 {
        r.skip(24)?; // language descriptor
    }
    if r.read_u32(1)? == 1 {
        let n = r.read_u32(10)? as usize + 1;
        r.skip(n * 8)?; // info text
    }
    let bit_res = r.read_u32(5)? + 1;
    let rate_code = r.read_u32(4)? as usize;
    let channels = r.read_u32(6)? + 1;
    Some(Exss {
        rate: Some(EXSS_RATES[rate_code]),
        depth: Some(bit_res),
        channels: Some(channels),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::tests_support::BitWriter;

    fn core_header(sfreq: u64, amode: u64, lff: u64, pcmr: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(32, 0x7FFE8001);
        w.put(1, 1); // FTYPE = normal frame
        w.put(5, 31); // SHORT
        w.put(1, 0); // CPF
        w.put(7, 15); // NBLKS
        w.put(14, 1007); // FSIZE
        w.put(6, amode);
        w.put(4, sfreq);
        w.put(5, 24); // RATE
        w.put(5, 0); // MIX..HDCD
        w.put(3, 0); // EXT_AUDIO_ID
        w.put(1, 0); // EXT_AUDIO
        w.put(1, 0); // ASPF
        w.put(2, lff);
        w.put(1, 0); // HFLAG
        w.put(1, 0); // FILTS
        w.put(4, 7); // VERNUM
        w.put(2, 0); // CHIST
        w.put(3, pcmr);
        w.put(32, 0);
        w.finish()
    }

    #[test]
    fn parses_dts_core_51_48k_24bit() {
        // sfreq=13 (48 kHz), amode=9 (5 ch), lff=1, pcmr=5 (24 bit)
        let info = parse(&core_header(13, 9, 1, 5)).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("DTS"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.bit_depth, Some(24));
        assert_eq!(info.channels, Some(6));
    }

    #[test]
    fn parses_exss_asset_descriptor() {
        let mut w = BitWriter::new();
        w.put(32, 0x64582025);
        w.put(8, 0); // UserDefinedBits
        w.put(2, 0); // nExtSSIndex
        w.put(1, 0); // bHeaderSizeType -> 8 / 16 bit sizes
        w.put(8, 100); // nuExtSSHeaderSize
        w.put(16, 2000); // nuExtSSFsize
        w.put(1, 1); // bStaticFieldsPresent
        w.put(2, 0); // nuRefClockCode
        w.put(3, 0); // nuExSSFrameDurationCode
        w.put(1, 0); // bTimeStampFlag
        w.put(3, 0); // nuNumAudioPresnt - 1
        w.put(3, 0); // nuNumAssets - 1
        w.put(1, 1); // nuActiveExSSMask[0] (1 bit)
        w.put(8, 0); // nuActiveAssetMask[0][0]
        w.put(1, 0); // bMixMetadataEnbl
        w.put(16, 1500); // nuAssetFsize[0]
        w.put(9, 50); // nuAssetDescriptFsize
        w.put(3, 0); // nuAssetIndex
        w.put(1, 0); // bAssetTypeDescrPresent
        w.put(1, 0); // bLanguageDescrPresent
        w.put(1, 0); // bInfoTextPresent
        w.put(5, 23); // nuBitResolution - 1 -> 24
        w.put(4, 13); // nuMaxSampleRate -> 96000
        w.put(6, 5); // nuTotalNumChs - 1 -> 6
        w.put(32, 0);
        let info = parse(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("DTS-HD"));
        assert_eq!(info.sample_rate, Some(96000));
        assert_eq!(info.bit_depth, Some(24));
        assert_eq!(info.channels, Some(6));
    }
}
