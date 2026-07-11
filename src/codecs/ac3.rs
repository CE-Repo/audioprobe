//! AC-3 (ATSC A/52) and E-AC-3 syncframe header parsing.

use super::CodecInfo;
use crate::bits::BitReader;

const RATES: [u32; 3] = [48000, 44100, 32000];
const RATES2: [u32; 3] = [24000, 22050, 16000]; // E-AC-3 reduced rates (fscod == 3)
const ACMOD_CHANNELS: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Scan `buf` for an (E-)AC-3 syncword and parse the first plausible frame.
pub fn parse(buf: &[u8]) -> Option<CodecInfo> {
    let mut i = 0;
    while i + 8 <= buf.len() {
        if buf[i] == 0x0B && buf[i + 1] == 0x77 {
            if let Some(info) = parse_frame(&buf[i..]) {
                return Some(info);
            }
        }
        i += 1;
    }
    None
}

fn parse_frame(b: &[u8]) -> Option<CodecInfo> {
    // In both AC-3 and E-AC-3 the 5-bit bsid field sits at bit offset 40.
    let bsid = {
        let mut r = BitReader::new(b);
        r.skip(40)?;
        r.read_u32(5)?
    };
    let mut r = BitReader::new(b);
    r.skip(16)?; // syncword

    if bsid <= 10 {
        // AC-3: crc1(16) fscod(2) frmsizecod(6) bsid(5) bsmod(3) acmod(3) ...
        r.skip(16)?;
        let fscod = r.read_u32(2)?;
        if fscod == 3 {
            return None;
        }
        let frmsizecod = r.read_u32(6)?;
        if frmsizecod >= 38 {
            return None;
        }
        r.skip(5 + 3)?; // bsid, bsmod
        let acmod = r.read_u32(3)? as usize;
        if acmod & 1 != 0 && acmod != 1 {
            r.skip(2)?; // cmixlev
        }
        if acmod & 4 != 0 {
            r.skip(2)?; // surmixlev
        }
        if acmod == 2 {
            r.skip(2)?; // dsurmod
        }
        let lfeon = r.read_u32(1)?;
        Some(CodecInfo {
            name: Some("AC-3".into()),
            sample_rate: Some(RATES[fscod as usize]),
            bit_depth: None,
            channels: Some(ACMOD_CHANNELS[acmod] + lfeon),
            lfe: Some(lfeon == 1),
            note: None,
        })
    } else if (11..=16).contains(&bsid) {
        // E-AC-3: strmtyp(2) substreamid(3) frmsiz(11) fscod(2)
        //         [fscod2|numblkscod](2) acmod(3) lfeon(1) bsid(5) ...
        let strmtyp = r.read_u32(2)?;
        if strmtyp == 3 {
            return None;
        }
        r.skip(3 + 11)?;
        let fscod = r.read_u32(2)?;
        let rate = if fscod == 3 {
            let fscod2 = r.read_u32(2)?;
            if fscod2 == 3 {
                return None;
            }
            RATES2[fscod2 as usize]
        } else {
            r.skip(2)?; // numblkscod
            RATES[fscod as usize]
        };
        let acmod = r.read_u32(3)? as usize;
        let lfeon = r.read_u32(1)?;
        Some(CodecInfo {
            name: Some("E-AC-3".into()),
            sample_rate: Some(rate),
            bit_depth: None,
            channels: Some(ACMOD_CHANNELS[acmod] + lfeon),
            lfe: Some(lfeon == 1),
            note: None,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::tests_support::BitWriter;

    #[test]
    fn parses_ac3_51_48k() {
        // Build an AC-3 header: fscod=0 (48 kHz), acmod=7 (3/2), lfeon=1 -> 5.1
        let mut w = BitWriter::new();
        w.put(16, 0x0B77);
        w.put(16, 0); // crc1
        w.put(2, 0); // fscod = 48 kHz
        w.put(6, 20); // frmsizecod
        w.put(5, 8); // bsid
        w.put(3, 0); // bsmod
        w.put(3, 7); // acmod = 3/2
        w.put(2, 0); // cmixlev (acmod & 1)
        w.put(2, 0); // surmixlev (acmod & 4)
        w.put(1, 1); // lfeon
        w.put(32, 0); // padding
        let info = parse(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("AC-3"));
        assert_eq!(info.sample_rate, Some(48000));
        assert_eq!(info.channels, Some(6));
        assert_eq!(info.lfe, Some(true));
    }

    #[test]
    fn parses_eac3_stereo_44k() {
        let mut w = BitWriter::new();
        w.put(16, 0x0B77);
        w.put(2, 0); // strmtyp
        w.put(3, 0); // substreamid
        w.put(11, 100); // frmsiz
        w.put(2, 1); // fscod = 44.1 kHz
        w.put(2, 3); // numblkscod
        w.put(3, 2); // acmod = 2/0
        w.put(1, 0); // lfeon
        w.put(5, 16); // bsid
        w.put(32, 0);
        let info = parse(&w.finish()).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("E-AC-3"));
        assert_eq!(info.sample_rate, Some(44100));
        assert_eq!(info.channels, Some(2));
        assert_eq!(info.lfe, Some(false));
    }
}
