# audioprobe

Fast native audio-track inspector. Point it at an MKV, TS, M2TS, MP4 (or a
whole directory) and get **bit depth, sample rate, channel layout, codec, bit
rate, immersive-audio format (Dolby Atmos / DTS:X) and language for every audio
track** — without launching `ffprobe`, `mediainfo` or any other subprocess.

Like [hdrprobe](https://github.com/matthane/hdrprobe) does for HDR metadata,
audioprobe does all the work in-process: it parses the container structures
and the codec sync headers natively and reads only the bytes it needs, so
probing a 40 GB remux takes milliseconds.

```
$ audioprobe movie.mkv

movie.mkv  [Matroska]
  #  CODEC              SAMPLE RATE  BIT DEPTH  CHANNELS    BITRATE    LANG
  1  TrueHD Atmos       48000 Hz     24-bit     7.1 (8 ch)  4500 kb/s  eng   [default]
  2  DTS-HD MA + DTS:X  48000 Hz     24-bit     5.1 (6 ch)  —          deu
  3  AC-3               48000 Hz     —          5.1 (6 ch)  640 kb/s   deu
```

## Features

- **No subprocesses, one small dependency.** Every container and codec is
  parsed natively in pure Rust; the only crate pulled in is `memmap2`, which
  backs file probes so the network-filesystem warmer can accelerate them.
- **NAS-aware.** On a file living on a mounted SMB/NFS share, audioprobe
  memory-maps it and pre-streams exactly the regions it is about to parse
  (front metadata, plus an MP4 `moov` wherever it sits), turning the scattered
  synchronous page faults a naive read would make into a few pipelined reads —
  the "same file is 20 ms local / 700 ms on the NAS" gap. Local probes detect
  they are local and skip the warm entirely, so the fast path is untouched.
- **Bit depth where the container doesn't know it.** Matroska often omits
  `BitDepth` for DTS or TrueHD tracks; audioprobe samples the first frames
  from the clusters and reads the value straight out of the bitstream
  (DTS core `PCMR`, DTS-HD extension substream asset headers, FLAC
  STREAMINFO, Blu-ray LPCM headers, …).
- **Immersive audio (Dolby Atmos / DTS:X).** TrueHD Atmos is read from the
  major-sync substream count, Dolby Digital Plus Atmos from the JOC payload in
  the E-AC-3 `addbsi` (or the MP4 `dec3` extension flag), and DTS:X from the
  object-based asset in the DTS-HD extension substream. Detected tracks are
  labelled `TrueHD Atmos`, `E-AC-3 Atmos`, `DTS-HD MA + DTS:X`, and carry an
  `"immersive"` field in JSON.
- **Bit rate.** Reported wherever the header carries a rate the parser can
  trust: the constant rate of AC-3, DTS core, MP1/2/3 and WAVEFORMATEX streams,
  the frame-derived rate of E-AC-3, the peak rate of TrueHD, and the exact
  rate of uncompressed PCM/LPCM. Variable-bitrate lossless and perceptual
  streams with no header figure (FLAC, AAC, Opus, Vorbis) show `—` (`null`).
- **Blu-ray aware.** BDAV M2TS (192-byte packets) is detected automatically,
  including HDMV LPCM, TrueHD (with its embedded AC-3 compatibility core),
  DTS-HD MA/HRA and E-AC-3 stream types.
- **Machine-readable output** with `--json`, one-liners with `--quiet`.
- **Reads from a pipe.** `audioprobe -` probes a bounded head piped to stdin,
  so a stream over ranged HTTP or a VFS plugin can be inspected without
  materializing the whole file; truncated heads are reported honestly.

## Supported inputs

| Containers | Matroska/WebM (`.mkv .mka .webm`), MPEG-TS (`.ts`), BDAV (`.m2ts .mts`), MP4/MOV (`.mp4 .m4a .mov`), AVI (`.avi`), MPEG program streams (`.mpg .mpeg .vob`), FLAC, WAV, Ogg |
|---|---|
| **Disc images** | Blu-ray ISO (BDMV, UDF) and DVD-Video ISO (VIDEO_TS, ISO9660) — `.iso` |
| **Codecs** | AC-3, E-AC-3, DTS, DTS-ES, DTS 96/24, DTS-HD MA/HRA, DTS Express, TrueHD, MLP, AAC (ADTS, LATM/LOAS, ASC), HE-AAC, MP1/MP2/MP3, FLAC, PCM/LPCM (Blu-ray + DVD), ALAC, Opus, Vorbis, WAVEFORMATEX (A_MS/ACM) |
| **Elementary streams** | `.ac3 .eac3 .dts .dtshd .thd .mlp .aac .mp3 …` |

Bit depth is reported where the format defines one (PCM, FLAC, ALAC, DTS,
TrueHD, Blu-ray LPCM). Perceptual codecs like AC-3, AAC or Opus have no
meaningful bit depth; those show `—` (`null` in JSON).

## Usage

```
audioprobe [OPTIONS] <PATH>...

  <PATH>...          media files or directories (use '-' for stdin)
  -j, --json         machine-readable JSON output
  -q, --quiet        one-line summary per file
  -r, --recursive    recurse into directories
      --fast         trust container metadata, skip frame-level sampling
      --limit-mb <N> max megabytes to scan per transport stream [default: 64]
  -h, --help         print help
  -V, --version      print version
```

Examples:

```sh
audioprobe movie.mkv                 # one file
audioprobe *.m2ts                    # several files
audioprobe -r -q /media/movies       # whole library, one line per file
audioprobe --json movie.ts | jq '.files[0].audio_tracks[].sample_rate'
```

### Reading from stdin

Pass `-` to probe a stream piped to stdin instead of a file on disk:

```sh
cat movie.mkv | audioprobe -
curl -s https://host/movie.ts | audioprobe --json -
```

audioprobe reads a **bounded head** of the pipe — enough to resolve the tracks
without pulling the whole stream — then stops. It reads up to 16 MiB for
container/elementary formats, or the transport-stream scan limit (`--limit-mb`,
64 MiB by default) for a stream that sniffs as MPEG-TS/M2TS. A writer feeding
the pipe sees a broken-pipe once audioprobe has enough; that is the normal,
expected outcome, not an error.

If the stream is larger than the head budget the report is marked truncated —
`[truncated]` in `--quiet`, a `note:` line in the default output, and
`"input_truncated": true` in `--json` — since a track whose first frames sit
beyond the cut may be missing. A stream that ends within the budget reports
exactly like a file probe.

Limits: `-` may be given at most once per run, and stdin is a pipe (no seeking),
so this is always a head probe.

JSON output:

```json
{
  "files": [
    {
      "path": "movie.mkv",
      "container": "Matroska",
      "error": null,
      "audio_tracks": [
        {
          "id": "1",
          "codec": "DTS-HD MA",
          "sample_rate": 48000,
          "bit_depth": 24,
          "channels": 6,
          "layout": "5.1",
          "bitrate": null,
          "immersive": "DTS:X",
          "language": "deu",
          "title": null,
          "note": null,
          "default": true
        }
      ]
    }
  ]
}
```

## Building

Requires a stable Rust toolchain (1.75+). The only dependency is `memmap2`
(fetched by cargo); everything else is parsed natively.

```sh
cargo build --release
./target/release/audioprobe --help
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | usage error (bad flag, no input) |
| 2 | at least one file could not be probed |

## How it works

- **Matroska/WebM** — a minimal EBML reader walks the `Tracks` element for
  codec ID, sampling frequency, bit depth, channels and language. For codecs
  whose container metadata is incomplete, the first frames of each audio
  track are pulled out of the leading `Cluster`s (SimpleBlock/BlockGroup,
  all lacing modes) and parsed natively. `--fast` skips this second step.
- **MPEG-TS / M2TS** — packet size and alignment are auto-detected
  (188/192/204 bytes), PAT and PMT are parsed (including DVB descriptors
  `0x6A/0x7A/0x7B/0x7C`, registration descriptors and the HDMV program
  registration), then PES payloads are collected per audio PID and their
  sync headers decoded. Scanning stops as soon as every track is resolved.
- **MP4/MOV** — the `moov` box tree is walked down to the `stsd` sample
  entries; codec configuration boxes (`esds`, `dac3`, `dec3`, `ddts`,
  `dfLa`, `dOps`, `alac`) provide exact parameters.
- **AVI** — the `hdrl` header list is walked to each stream's `strl`; every
  audio stream's `strf` (a WAVEFORMATEX) gives the codec and parameters. The
  `movi` payload is never read.
- **MPEG program streams** (`.mpg .mpeg .vob`) — the pack / system / PES
  layers are walked and each audio stream's payload collected: MPEG audio
  (`0xC0–0xDF`) and the DVD `private_stream_1` sub-streams (AC-3, DTS and
  DVD-LPCM), decoded by the same native codec parsers.
- **Disc images** (`.iso`) — a Blu-ray image is walked as a UDF filesystem to
  `BDMV/PLAYLIST`; the playlists are ranked by deduped duration and the
  winner's largest `BDMV/STREAM/*.m2ts` clip is probed as a transport stream.
  A DVD-Video image is walked as ISO9660 to `VIDEO_TS`; the title set with the
  most VOB bytes is probed as a program stream. The clip is read in place from
  the image — no extraction.
- **Codec headers parsed natively:** AC-3/E-AC-3 syncframes (with the bsi
  walk to the `addbsi` JOC payload for Dolby Digital Plus Atmos), DTS core +
  extension substream asset descriptors (with XLL/XBR/LBR classification and
  the object-asset flag for DTS:X), TrueHD/MLP major sync (peak data rate and
  the substream count that marks TrueHD Atmos), ADTS and LATM/LOAS AAC,
  AudioSpecificConfig, MPEG audio headers, FLAC STREAMINFO, OpusHead, Vorbis
  ID headers, Blu-ray LPCM headers and WAVEFORMATEX. Bit rate is taken from
  the header where it is constant, computed per frame for E-AC-3, and read as
  the peak rate for TrueHD.
- **Network-filesystem warming** — a file probe maps the file and, when it
  detects the file lives on a mounted network share (Windows: the open
  handle's remote-protocol info; Linux: the fstype of the holding mount in
  `/proc/self/mounts` — `cifs/smb3/nfs/nfs4/9p/sshfs/…`), pre-streams the
  ranges it is about to parse before parsing starts. Local files and other
  platforms skip the warm, so nothing changes on the fast local path. This is
  a timing-only optimization — the report is byte-identical either way. (A
  stdin probe reads its bounded head straight from the pipe and is unaffected.)

## Limitations

- E-AC-3 channel counts come from the independent substream's `acmod`;
  dependent substreams (7.1 channel extensions) are not summed. Dolby Digital
  Plus Atmos *is* detected (the JOC payload in `addbsi`, or the `dec3` flag in
  MP4), but the reported channel count is the 5.1/7.1 bed, not the object count.
- For DTS-HD, classification into MA/HRA relies on extension-substream sync
  patterns within the scanned window plus the PMT stream type; exotic
  streams may fall back to the generic "DTS-HD" label. DTS:X is inferred from
  a lossless (XLL) asset whose channels are not mapped 1:1 to speakers — a
  best-effort signal that will not flag an object stream lacking that marker.
- Bit rate is a nominal or (for TrueHD) peak figure read from the sync header,
  not a measured average; variable lossless and perceptual streams that carry
  no such figure report no bit rate rather than guess.
- ISO disc images require file access (random access across the image), so
  they cannot be probed from stdin. AACS-encrypted Blu-ray images and
  CSS-scrambled DVDs cannot be read; probe a decrypted backup.
- A main-feature clip fragmented into non-adjacent extents inside a Blu-ray
  ISO is reported as unsupported rather than probed from the wrong offset.
