# audioprobe

Fast native audio-track inspector. Point it at an MKV, TS, M2TS, MP4 (or a
whole directory) and get **bit depth, sample rate, channel layout, codec and
language for every audio track** — without launching `ffprobe`, `mediainfo`
or any other subprocess.

Like [hdrprobe](https://github.com/matthane/hdrprobe) does for HDR metadata,
audioprobe does all the work in-process: it parses the container structures
and the codec sync headers natively and reads only the bytes it needs, so
probing a 40 GB remux takes milliseconds.

```
$ audioprobe movie.mkv

movie.mkv  [Matroska]
  #  CODEC      SAMPLE RATE  BIT DEPTH  CHANNELS    LANG
  1  TrueHD     48000 Hz     24-bit     7.1 (8 ch)  eng   [default]
  2  DTS-HD MA  48000 Hz     24-bit     5.1 (6 ch)  deu
  3  AC-3       48000 Hz     —          5.1 (6 ch)  deu
```

## Features

- **Zero dependencies, zero subprocesses.** One static binary, pure Rust,
  `cargo build` and done.
- **Bit depth where the container doesn't know it.** Matroska often omits
  `BitDepth` for DTS or TrueHD tracks; audioprobe samples the first frames
  from the clusters and reads the value straight out of the bitstream
  (DTS core `PCMR`, DTS-HD extension substream asset headers, FLAC
  STREAMINFO, Blu-ray LPCM headers, …).
- **Blu-ray aware.** BDAV M2TS (192-byte packets) is detected automatically,
  including HDMV LPCM, TrueHD (with its embedded AC-3 compatibility core),
  DTS-HD MA/HRA and E-AC-3 stream types.
- **Machine-readable output** with `--json`, one-liners with `--quiet`.
- **Reads from a pipe.** `audioprobe -` probes a bounded head piped to stdin,
  so a stream over ranged HTTP or a VFS plugin can be inspected without
  materializing the whole file; truncated heads are reported honestly.

## Supported inputs

| Containers | Matroska/WebM (`.mkv .mka .webm`), MPEG-TS (`.ts`), BDAV (`.m2ts .mts`), MP4/MOV (`.mp4 .m4a .mov`), FLAC, WAV, Ogg |
|---|---|
| **Codecs** | AC-3, E-AC-3, DTS, DTS-ES, DTS 96/24, DTS-HD MA/HRA, DTS Express, TrueHD, MLP, AAC (ADTS, LATM/LOAS, ASC), HE-AAC, MP1/MP2/MP3, FLAC, PCM/LPCM, ALAC, Opus, Vorbis, WAVEFORMATEX (A_MS/ACM) |
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

Requires a stable Rust toolchain (1.75+). No further dependencies.

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
- **Codec headers parsed natively:** AC-3/E-AC-3 syncframes, DTS core +
  extension substream asset descriptors (with XLL/XBR/LBR classification),
  TrueHD/MLP major sync, ADTS and LATM/LOAS AAC, AudioSpecificConfig,
  MPEG audio headers, FLAC STREAMINFO, OpusHead, Vorbis ID headers,
  Blu-ray LPCM headers and WAVEFORMATEX.

## Limitations

- E-AC-3 channel counts come from the independent substream's `acmod`;
  dependent substreams (7.1 extensions) and Atmos/JOC are not evaluated.
- For DTS-HD, classification into MA/HRA relies on extension-substream sync
  patterns within the scanned window plus the PMT stream type; exotic
  streams may fall back to the generic "DTS-HD" label.
- MPEG program streams (`.vob`, `.mpg`) are not supported.
