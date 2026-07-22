# Changelog

All notable changes to audioprobe are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-07-22

### Added

- **Immersive audio detection (Dolby Atmos / DTS:X).** Tracks are now labelled
  `TrueHD Atmos`, `E-AC-3 Atmos` and `DTS-HD MA + DTS:X`, with an `"immersive"`
  field in JSON output.
  - TrueHD Atmos from the major-sync substream count.
  - Dolby Digital Plus Atmos from the JOC payload in the E-AC-3 `addbsi`
    (native bit-stream walk) and from the MP4 `dec3` extension flag.
  - DTS:X from an object-based (non 1:1 speaker-mapped) asset in the DTS-HD
    extension substream.
- **Audio bit rate reporting.** A new `BITRATE` column in the text table, a
  figure in the `--quiet` one-liner, and a `"bitrate"` field (bits per second)
  in JSON. Populated from the header where it is reliable — constant rate for
  AC-3, DTS core, MP1/2/3 and WAVEFORMATEX; per-frame rate for E-AC-3; peak
  rate for TrueHD; exact rate for uncompressed PCM/LPCM; and the avg/max rate
  from MP4 `dec3`/`ddts` boxes. Variable lossless and perceptual streams that
  carry no header figure (FLAC, AAC, Opus, Vorbis) report no bit rate.

### Changed

- The text-table `CODEC` column folds in the immersive tag; JSON keeps `codec`
  as the base name and exposes `immersive` separately. The JSON per-track
  object gains two new keys (`bitrate`, `immersive`).

## [0.1.0]

Initial release: native audio-track inspection for Matroska/WebM, MPEG-TS,
BDAV M2TS, MP4/MOV, AVI, MPEG program streams, FLAC, WAV, Ogg, raw elementary
streams and Blu-ray/DVD ISO images — reporting codec, sample rate, bit depth,
channel layout and language with no subprocesses, plus `--json`/`--quiet`
output, stdin head probing and network-filesystem prefetch warming.

[0.2.0]: https://github.com/ce-repo/audioprobe/releases/tag/v0.2.0
[0.1.0]: https://github.com/ce-repo/audioprobe/releases/tag/v0.1.0
