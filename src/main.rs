//! audioprobe — fast native audio-track inspector.
//!
//! Reports bit depth, sample rate, channel layout and codec for every audio
//! track in MKV/WebM, MPEG-TS/M2TS, MP4/MOV, FLAC, WAV, Ogg and raw
//! elementary streams — parsing everything natively, without launching
//! ffprobe or any other subprocess.

mod bits;
mod codecs;
mod containers;
mod report;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const KNOWN_EXTENSIONS: &[&str] = &[
    "mkv", "mka", "mk3d", "webm", "ts", "m2ts", "mts", "tsv", "mp4", "m4a", "m4v", "mov", "flac",
    "wav", "ogg", "oga", "opus", "ac3", "eac3", "ec3", "dts", "dtshd", "dtsma", "thd", "truehd",
    "mlp", "aac", "adts", "latm", "loas", "mp3", "mp2", "mp1", "mpa",
];

struct Args {
    paths: Vec<PathBuf>,
    json: bool,
    quiet: bool,
    recursive: bool,
    fast: bool,
    scan_limit_mb: u64,
}

fn usage() -> String {
    format!(
        "audioprobe {VERSION} — audio track inspector (bit depth, sample rate, channels)

USAGE:
    audioprobe [OPTIONS] <PATH>...

ARGS:
    <PATH>...        Media files or directories to inspect

OPTIONS:
    -j, --json       Machine-readable JSON output
    -q, --quiet      One-line summary per file
    -r, --recursive  Recurse into directories
        --fast       Trust container metadata; skip frame-level bitstream sampling
        --limit-mb <N>
                     Max megabytes to scan per transport stream [default: 64]
    -h, --help       Print help
    -V, --version    Print version

EXIT CODES:
    0  success
    1  usage error
    2  at least one file could not be probed

Supported containers: Matroska/WebM (.mkv .mka .webm), MPEG-TS (.ts),
BDAV M2TS (.m2ts .mts), MP4/MOV (.mp4 .m4a .mov), FLAC, WAV, Ogg and raw
elementary streams (.ac3 .eac3 .dts .dtshd .thd .mlp .aac .mp3 …)."
    )
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        paths: Vec::new(),
        json: false,
        quiet: false,
        recursive: false,
        fast: false,
        scan_limit_mb: 64,
    };
    let mut it = std::env::args().skip(1);
    let mut no_more_flags = false;
    while let Some(a) = it.next() {
        if no_more_flags || !a.starts_with('-') || a == "-" {
            args.paths.push(PathBuf::from(a));
            continue;
        }
        match a.as_str() {
            "--" => no_more_flags = true,
            "-j" | "--json" => args.json = true,
            "-q" | "--quiet" => args.quiet = true,
            "-r" | "--recursive" => args.recursive = true,
            "--fast" => args.fast = true,
            "--limit-mb" => {
                let v = it.next().ok_or("--limit-mb requires a value")?;
                args.scan_limit_mb = v
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --limit-mb value: {v}"))?
                    .max(1);
            }
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("audioprobe {VERSION}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if args.paths.is_empty() {
        return Err("no input files given".into());
    }
    Ok(args)
}

fn has_known_extension(path: &Path) -> bool {
    path.extension()
        .map(|e| {
            let e = e.to_string_lossy().to_lowercase();
            KNOWN_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

fn collect_files(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            collect_dir(p, recursive, &mut out).map_err(|e| format!("{}: {e}", p.display()))?;
        } else {
            // Explicitly named files are always probed, whatever the extension.
            out.push(p.clone());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_dir(&path, recursive, out)?;
            }
        } else if has_known_extension(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            eprintln!("{}", usage());
            return ExitCode::from(1);
        }
    };
    let files = match collect_files(&args.paths, args.recursive) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    if files.is_empty() {
        eprintln!("error: no media files found");
        return ExitCode::from(1);
    }

    let opts = containers::Options {
        scan_limit: args.scan_limit_mb * 1024 * 1024,
        deep: !args.fast,
    };

    let reports: Vec<report::Report> = files
        .iter()
        .map(|f| containers::probe_path(f, &opts))
        .collect();

    if args.json {
        print!("{}", report::render_json(&reports));
    } else {
        let mut out = String::new();
        for (i, r) in reports.iter().enumerate() {
            if args.quiet {
                report::render_quiet(r, &mut out);
            } else {
                if i > 0 {
                    out.push('\n');
                }
                report::render_text(r, &mut out);
            }
        }
        print!("{out}");
    }

    if reports.iter().any(|r| r.error.is_some()) {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
