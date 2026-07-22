//! AC-3 (ATSC A/52) and E-AC-3 syncframe header parsing.

use super::CodecInfo;
use crate::bits::BitReader;

const RATES: [u32; 3] = [48000, 44100, 32000];
const RATES2: [u32; 3] = [24000, 22050, 16000]; // E-AC-3 reduced rates (fscod == 3)
const ACMOD_CHANNELS: [u32; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
// AC-3 nominal bit rate in kbit/s, indexed by (frmsizecod >> 1).
const AC3_BITRATES: [u32; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];
// E-AC-3 audio blocks per syncframe, indexed by numblkscod.
const EAC3_NUMBLKS: [u32; 4] = [1, 2, 3, 6];

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
            // AC-3 is constant bit rate: frmsizecod selects it directly.
            bitrate: Some(AC3_BITRATES[(frmsizecod >> 1) as usize] * 1000),
            ..CodecInfo::default()
        })
    } else if (11..=16).contains(&bsid) {
        // E-AC-3: strmtyp(2) substreamid(3) frmsiz(11) fscod(2)
        //         [fscod2|numblkscod](2) acmod(3) lfeon(1) bsid(5) ...
        let strmtyp = r.read_u32(2)?;
        if strmtyp == 3 {
            return None;
        }
        r.skip(3)?; // substreamid
        let frmsiz = r.read_u32(11)?;
        let fscod = r.read_u32(2)?;
        let (rate, numblks) = if fscod == 3 {
            let fscod2 = r.read_u32(2)?;
            if fscod2 == 3 {
                return None;
            }
            (RATES2[fscod2 as usize], 6) // fscod == 3 implies numblkscod == 3
        } else {
            let numblkscod = r.read_u32(2)?;
            (RATES[fscod as usize], EAC3_NUMBLKS[numblkscod as usize])
        };
        let acmod = r.read_u32(3)? as usize;
        let lfeon = r.read_u32(1)?;
        // frmsiz is the frame length in 16-bit words minus one; each syncframe
        // carries numblks * 256 samples of the substream.
        let frame_bytes = ((frmsiz + 1) * 2) as u64;
        let bitrate = (frame_bytes * 8 * rate as u64 / (numblks as u64 * 256)) as u32;
        Some(CodecInfo {
            name: Some("E-AC-3".into()),
            sample_rate: Some(rate),
            bit_depth: None,
            channels: Some(ACMOD_CHANNELS[acmod] + lfeon),
            lfe: Some(lfeon == 1),
            bitrate: Some(bitrate),
            // Dolby Digital Plus carries Atmos as a JOC payload in the
            // independent substream's addbsi metadata.
            immersive: if eac3_has_joc(b) {
                Some("Atmos".into())
            } else {
                None
            },
            note: None,
        })
    } else {
        None
    }
}

/// Walk an E-AC-3 syncframe's bit-stream information (bsi) far enough to reach
/// the `addbsi` field, then peek at the embedded EMDF metadata. Dolby Digital
/// Plus with Atmos (Joint Object Coding) carries an EMDF payload with
/// `emdf_payload_id == 11` there. Returns `true` only when that payload is
/// found — a desynced walk fails closed to "no Atmos" rather than guessing.
///
/// `b` must start at the 0x0B77 syncword of an independent substream.
fn eac3_has_joc(b: &[u8]) -> bool {
    fn walk(b: &[u8]) -> Option<bool> {
        let mut r = BitReader::new(b);
        r.skip(16)?; // syncword
        let strmtyp = r.read_u32(2)?;
        r.skip(3 + 11)?; // substreamid, frmsiz
        let fscod = r.read_u32(2)?;
        let numblkscod = if fscod == 3 {
            r.skip(2)?; // fscod2
            3
        } else {
            r.read_u32(2)?
        };
        let numblks = EAC3_NUMBLKS[numblkscod as usize];
        let acmod = r.read_u32(3)?;
        let lfeon = r.read_u32(1)?;
        r.skip(5 + 5)?; // bsid, dialnorm
        if r.read_u32(1)? == 1 {
            r.skip(8)?; // compr
        }
        if acmod == 0 {
            r.skip(5)?; // dialnorm2
            if r.read_u32(1)? == 1 {
                r.skip(8)?; // compr2
            }
        }
        if strmtyp == 1 && r.read_u32(1)? == 1 {
            r.skip(16)?; // chanmap
        }
        if r.read_u32(1)? == 1 {
            // mixing metadata
            if acmod > 2 {
                r.skip(2)?; // dmixmod
            }
            if (acmod & 1) != 0 && acmod > 2 {
                r.skip(3 + 3)?; // ltrtcmixlev, lorocmixlev
            }
            if (acmod & 4) != 0 {
                r.skip(3 + 3)?; // ltrtsurmixlev, lorosurmixlev
            }
            if lfeon == 1 && r.read_u32(1)? == 1 {
                r.skip(5)?; // lfemixlevcod
            }
            if strmtyp == 0 {
                if r.read_u32(1)? == 1 {
                    r.skip(6)?; // pgmscl
                }
                if acmod == 0 && r.read_u32(1)? == 1 {
                    r.skip(6)?; // pgmscl2
                }
                if r.read_u32(1)? == 1 {
                    r.skip(6)?; // extpgmscl
                }
                match r.read_u32(2)? {
                    1 => r.skip(1 + 1 + 3)?, // premixcmpsel, drcsrc, premixcmpscl
                    2 => r.skip(12)?,        // mixdata
                    3 => {
                        let mixdeflen = r.read_u32(5)?;
                        r.skip(((mixdeflen + 2) * 8) as usize)?;
                    }
                    _ => {}
                }
                if acmod < 2 {
                    if r.read_u32(1)? == 1 {
                        r.skip(8 + 6)?; // panmean, paninfo
                    }
                    if acmod == 0 && r.read_u32(1)? == 1 {
                        r.skip(8 + 6)?; // panmean2, paninfo2
                    }
                }
                if r.read_u32(1)? == 1 {
                    // frmmixcfginfoe
                    if numblkscod == 0 {
                        r.skip(5)?; // blkmixcfginfo[0]
                    } else {
                        for _ in 0..numblks {
                            if r.read_u32(1)? == 1 {
                                r.skip(5)?; // blkmixcfginfo[blk]
                            }
                        }
                    }
                }
            }
        }
        if r.read_u32(1)? == 1 {
            // informational metadata
            r.skip(3 + 1 + 1)?; // bsmod, copyrightb, origbs
            if acmod == 2 {
                r.skip(2)?; // dsurmod
            }
            if acmod >= 6 {
                r.skip(2)?; // dsurexmod
            }
            if r.read_u32(1)? == 1 {
                r.skip(5 + 2 + 1)?; // mixlevel, roomtyp, adconvtyp
            }
            if acmod == 0 && r.read_u32(1)? == 1 {
                r.skip(5 + 2 + 1)?; // mixlevel2, roomtyp2, adconvtyp2
            }
            if fscod < 3 {
                r.skip(1)?; // sourcefscod
            }
        }
        if strmtyp == 0 && numblkscod != 3 {
            r.skip(1)?; // convsync
        }
        if strmtyp == 2 {
            let convexpstre = if numblkscod == 3 { 1 } else { r.read_u32(1)? };
            if convexpstre == 1 {
                r.skip(6)?; // frmsizecod
            }
        }
        if r.read_u32(1)? == 0 {
            return Some(false); // addbsie: no additional bit-stream information
        }
        r.skip(6)?; // addbsil
                    // EMDF preamble, then the first payload id (JOC == 11).
        if r.read_u32(2)? == 3 {
            r.skip(2)?; // emdf_version extension
        }
        if r.read_u32(3)? == 7 {
            r.skip(3)?; // key_id extension
        }
        let mut payload_id = r.read_u32(5)?;
        if payload_id == 0x1F {
            payload_id += r.read_u32(8)?;
        }
        Some(payload_id == 11)
    }
    walk(b).unwrap_or(false)
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
        // frmsizecod 20 -> index 10 -> 192 kbit/s
        assert_eq!(info.bitrate, Some(192_000));
        assert_eq!(info.immersive, None);
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
        // (100+1)*2 bytes * 8 * 44100 / (6*256 samples)
        assert_eq!(info.bitrate, Some(46_396));
        assert_eq!(info.immersive, None);
    }

    /// Build an E-AC-3 5.1 independent-substream frame, optionally carrying a
    /// JOC (Atmos) EMDF payload in addbsi.
    fn eac3_51_frame(with_joc: bool) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(16, 0x0B77);
        w.put(2, 0); // strmtyp = independent
        w.put(3, 0); // substreamid
        w.put(11, 300); // frmsiz
        w.put(2, 0); // fscod = 48 kHz
        w.put(2, 3); // numblkscod = 6 blocks
        w.put(3, 7); // acmod = 3/2
        w.put(1, 1); // lfeon
        w.put(5, 16); // bsid = 16 (E-AC-3)
        w.put(5, 31); // dialnorm
        w.put(1, 0); // compre
        w.put(1, 0); // mixmdate
        w.put(1, 0); // infomdate
                     // strmtyp==0 && numblkscod==3 -> no convsync; strmtyp!=2
        if with_joc {
            w.put(1, 1); // addbsie
            w.put(6, 1); // addbsil (2 bytes)
            w.put(2, 0); // emdf_version
            w.put(3, 0); // key_id
            w.put(5, 11); // emdf_payload_id = 11 (JOC)
        } else {
            w.put(1, 0); // addbsie
        }
        w.put(32, 0); // padding
        w.finish()
    }

    #[test]
    fn detects_eac3_joc_atmos() {
        let info = parse(&eac3_51_frame(true)).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("E-AC-3"));
        assert_eq!(info.channels, Some(6));
        assert_eq!(info.lfe, Some(true));
        assert_eq!(info.immersive.as_deref(), Some("Atmos"));
    }

    #[test]
    fn plain_eac3_is_not_atmos() {
        let info = parse(&eac3_51_frame(false)).expect("should parse");
        assert_eq!(info.name.as_deref(), Some("E-AC-3"));
        assert_eq!(info.immersive, None);
    }
}
