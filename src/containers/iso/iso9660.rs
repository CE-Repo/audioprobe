//! Minimal read-only ISO9660 walker: enough to reach `VIDEO_TS` on a
//! DVD-Video image and resolve each `.VOB` file's byte extent. DVDs carry an
//! ISO9660 bridge alongside UDF, and `VIDEO_TS` files are single-extent, so a
//! plain ISO9660 directory walk suffices — no UDF needed for the DVD path.

type Result<T> = std::result::Result<T, String>;

const SECTOR: u64 = 2048;
/// A DVD directory is a handful of records; cap the walk defensively.
const MAX_RECORDS: usize = 65_536;

fn le16(d: &[u8], o: usize) -> u16 {
    match d.get(o..o + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}
fn le32(d: &[u8], o: usize) -> u32 {
    match d.get(o..o + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// A directory entry: its cleaned name, kind, and absolute byte range.
#[derive(Debug, Clone)]
pub struct DirRec {
    pub name: String,
    pub is_dir: bool,
    pub offset: u64,
    pub size: u64,
}

pub struct Iso9660<'a> {
    data: &'a [u8],
    block_size: u64,
    root: DirRec,
}

/// Cheap gate: the Primary Volume Descriptor's `CD001` identifier at sector 16.
pub fn looks_like_iso9660(data: &[u8]) -> bool {
    let o = (16 * SECTOR) as usize;
    data.get(o + 1..o + 6) == Some(b"CD001") && data.get(o) == Some(&1)
}

impl<'a> Iso9660<'a> {
    pub fn open(data: &'a [u8]) -> Result<Iso9660<'a>> {
        let pvd_off = (16 * SECTOR) as usize;
        let pvd = data
            .get(pvd_off..pvd_off + SECTOR as usize)
            .ok_or_else(|| "image too small for an ISO9660 volume descriptor".to_string())?;
        if &pvd[1..6] != b"CD001" || pvd[0] != 1 {
            return Err("no ISO9660 primary volume descriptor".into());
        }
        let block_size = match le16(pvd, 128) as u64 {
            0 => SECTOR,
            n => n,
        };
        // Root directory record lives at PVD offset 156 (34 bytes).
        let root_rec = &pvd[156..190];
        let lba = le32(root_rec, 2);
        let size = le32(root_rec, 10);
        let root = DirRec {
            name: String::new(),
            is_dir: true,
            offset: u64::from(lba) * block_size,
            size: u64::from(size),
        };
        Ok(Iso9660 { data, block_size, root })
    }

    pub fn root(&self) -> DirRec {
        self.root.clone()
    }

    /// List a directory's children (the `.`/`..` self and parent entries and
    /// associated-file records are skipped).
    pub fn read_dir(&self, dir: &DirRec) -> Result<Vec<DirRec>> {
        if !dir.is_dir {
            return Err("not a directory".into());
        }
        let start = dir.offset as usize;
        let end = start
            .checked_add(dir.size as usize)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| "directory extent outside the image".to_string())?;
        let buf = &self.data[start..end];

        let mut out = Vec::new();
        let mut p = 0usize;
        while p < buf.len() && out.len() < MAX_RECORDS {
            let len = buf[p] as usize;
            if len == 0 {
                // Records never span a logical sector; jump to the next one.
                let next = (p / self.block_size as usize + 1) * self.block_size as usize;
                if next <= p {
                    break;
                }
                p = next;
                continue;
            }
            if p + len > buf.len() || len < 33 {
                break;
            }
            let rec = &buf[p..p + len];
            let ext_attr = rec[1] as u64;
            let lba = le32(rec, 2);
            let size = le32(rec, 10);
            let flags = rec[25];
            let name_len = rec[32] as usize;
            if let Some(name_bytes) = rec.get(33..33 + name_len) {
                // Skip the '.' (0x00) and '..' (0x01) special entries.
                let special = name_len == 1 && (name_bytes[0] == 0 || name_bytes[0] == 1);
                let associated = flags & 0x04 != 0;
                if !special && !associated {
                    out.push(DirRec {
                        name: clean_name(name_bytes),
                        is_dir: flags & 0x02 != 0,
                        offset: (u64::from(lba) + ext_attr) * self.block_size,
                        size: u64::from(size),
                    });
                }
            }
            p += len;
        }
        Ok(out)
    }
}

/// ISO9660 file names carry a `;1` version suffix and are upper-cased; strip
/// the version and a trailing dot for display/matching.
fn clean_name(raw: &[u8]) -> String {
    let s: String = raw.iter().map(|&b| b as char).collect();
    let s = s.split(';').next().unwrap_or(&s);
    s.strip_suffix('.').unwrap_or(s).to_string()
}

#[cfg(test)]
pub(crate) mod testimg {
    use super::SECTOR;

    /// A file to place in the synthetic image's `VIDEO_TS` directory.
    pub(crate) struct FileSpec {
        pub name: String,
        pub data: Vec<u8>,
    }

    struct Img {
        data: Vec<u8>,
    }
    impl Img {
        fn write(&mut self, sector: u64, bytes: &[u8]) {
            let off = (sector * SECTOR) as usize;
            let end = off + bytes.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[off..end].copy_from_slice(bytes);
        }
    }

    /// A single ISO9660 directory record.
    fn dir_record(name: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
        let len = 33 + name.len() + ((name.len() + 1) % 2); // padded to even
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[25] = if is_dir { 0x02 } else { 0x00 };
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r
    }

    fn dir_block(self_lba: u32, parent_lba: u32, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&dir_record(&[0], self_lba, SECTOR as u32, true)); // "."
        b.extend_from_slice(&dir_record(&[1], parent_lba, SECTOR as u32, true)); // ".."
        for e in entries {
            b.extend_from_slice(e);
        }
        b.resize(SECTOR as usize, 0);
        b
    }

    /// Build a minimal DVD-shaped ISO9660 image: root -> VIDEO_TS -> files.
    /// Layout by sector: 16 PVD, 17 terminator, 18 root dir, 19 VIDEO_TS dir,
    /// 20.. file data.
    pub(crate) fn build(files: &[FileSpec]) -> Vec<u8> {
        let mut img = Img { data: Vec::new() };

        const ROOT_LBA: u32 = 18;
        const VTS_LBA: u32 = 19;
        let mut data_lba: u32 = 20;

        // Place file data and build VIDEO_TS records.
        let mut vts_records = Vec::new();
        for f in files {
            let blocks = ((f.data.len() as u64).div_ceil(SECTOR)).max(1) as u32;
            img.write(u64::from(data_lba), &f.data);
            let mut name = f.name.clone().into_bytes();
            name.extend_from_slice(b";1");
            vts_records.push(dir_record(&name, data_lba, f.data.len() as u32, false));
            data_lba += blocks;
        }

        // Directories.
        let video_ts = dir_record(b"VIDEO_TS", VTS_LBA, SECTOR as u32, true);
        img.write(u64::from(ROOT_LBA), &dir_block(ROOT_LBA, ROOT_LBA, &[video_ts]));
        img.write(u64::from(VTS_LBA), &dir_block(VTS_LBA, ROOT_LBA, &vts_records));

        // Primary Volume Descriptor at sector 16, terminator at 17.
        let mut pvd = vec![0u8; SECTOR as usize];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[128..130].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        pvd[130..132].copy_from_slice(&(SECTOR as u16).to_be_bytes());
        pvd[156..190].copy_from_slice(&{
            let mut r = dir_record(&[0], ROOT_LBA, SECTOR as u32, true);
            r.resize(34, 0);
            r
        });
        img.write(16, &pvd);
        let mut term = vec![0u8; SECTOR as usize];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        img.write(17, &term);

        img.data.resize((data_lba as usize) * SECTOR as usize, 0);
        img.data
    }
}

#[cfg(test)]
mod tests {
    use super::testimg::FileSpec;
    use super::*;

    #[test]
    fn walks_video_ts() {
        let img = testimg::build(&[
            FileSpec { name: "VIDEO_TS.VOB".into(), data: vec![0xAA; 100] },
            FileSpec { name: "VTS_01_1.VOB".into(), data: vec![0xBB; 5000] },
        ]);
        assert!(looks_like_iso9660(&img));
        let iso = Iso9660::open(&img).unwrap();
        let root = iso.root();
        let entries = iso.read_dir(&root).unwrap();
        let vts = entries.iter().find(|e| e.name == "VIDEO_TS").unwrap();
        assert!(vts.is_dir);
        let files = iso.read_dir(vts).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["VIDEO_TS.VOB", "VTS_01_1.VOB"]);
        let vob = files.iter().find(|f| f.name == "VTS_01_1.VOB").unwrap();
        assert_eq!(vob.size, 5000);
        assert_eq!(img[vob.offset as usize], 0xBB);
    }

    #[test]
    fn rejects_non_iso9660() {
        assert!(!looks_like_iso9660(&[]));
        assert!(!looks_like_iso9660(&vec![0u8; 1 << 20]));
        assert!(Iso9660::open(&vec![0u8; 1 << 20]).is_err());
    }
}
