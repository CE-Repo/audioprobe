//! audioprobe — fast native audio-track inspector.
//!
//! Reports bit depth, sample rate, channel layout and codec for every audio
//! track in MKV/WebM, MPEG-TS/M2TS, MP4/MOV, FLAC, WAV, Ogg and raw
//! elementary streams — parsing everything natively, without launching
//! ffprobe or any other subprocess.

mod bits;
mod codecs;
mod containers;
mod prefetch;
mod report;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sniff block read from stdin before the head budget is chosen: enough for
/// every magic check the dispatcher makes (the TS sync-lock needs well under
/// 1 KiB) with generous slack.
const STDIN_SNIFF_BYTES: usize = 64 * 1024;

/// Head budget for a non-TS stdin stream. Matroska and MP4 declare their track
/// metadata up front and the elementary/FLAC/WAV/Ogg probers bound their head
/// reads well under this, so 16 MiB is comfortable slack; a stream that sniffs
/// as TS instead gets the transport-stream scan limit (`--limit-mb`), whose
/// audio PES can ride much deeper into the stream.
const STDIN_HEAD_BYTES: usize = 16 * 1024 * 1024;

const KNOWN_EXTENSIONS: &[&str] = &[
    "mkv", "mka", "mk3d", "webm", "ts", "m2ts", "mts", "tsv", "mp4", "m4a", "m4v", "mov", "flac",
    "wav", "ogg", "oga", "opus", "ac3", "eac3", "ec3", "dts", "dtshd", "dtsma", "thd", "truehd",
    "mlp", "aac", "adts", "latm", "loas", "mp3", "mp2", "mp1", "mpa", "avi", "vob", "mpg", "mpeg",
    "m2p", "ps", "iso",
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
    <PATH>...        Media files or directories to inspect;
                     '-' probes a stream head piped to stdin

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
BDAV M2TS (.m2ts .mts), MP4/MOV (.mp4 .m4a .mov), AVI (.avi), MPEG program
streams (.mpg .mpeg .vob), Blu-ray and DVD-Video disc images (.iso), FLAC,
WAV, Ogg and raw elementary streams (.ac3 .eac3 .dts .dtshd .thd .mlp .aac
.mp3 …).

Pass '-' to probe a stream piped to stdin (a bounded head is read, so the
result is marked truncated when the stream is larger than the head budget):
    cat movie.mkv | audioprobe -
    curl -s https://host/movie.ts | audioprobe --json -"
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

/// Read a bounded head from `r`: a sniff block first, then up to the sniffed
/// format's budget plus one byte. Reading one byte past the budget is what
/// makes truncation detectable — a full `budget + 1` read means the stream
/// held more (the extra byte is dropped and `truncated` is `true`); EOF at or
/// under the budget means the input is complete. Generic over the reader and
/// budget so tests can drive it with a `Cursor` and tiny budgets.
fn read_bounded_head(
    mut r: impl Read,
    sniff_bytes: usize,
    budget_for: impl FnOnce(&[u8]) -> usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.by_ref().take(sniff_bytes as u64).read_to_end(&mut buf)?;
    let budget = budget_for(&buf);
    // A short sniff read means EOF already arrived; only a full block can have
    // more bytes behind it.
    if buf.len() >= sniff_bytes {
        let remaining = (budget + 1).saturating_sub(buf.len());
        r.take(remaining as u64).read_to_end(&mut buf)?;
    }
    let truncated = buf.len() > budget;
    if truncated {
        buf.truncate(budget);
    }
    Ok((buf, truncated))
}

/// How many bytes of a sniffed stdin block are worth reading before parsing
/// begins. A block that dispatches to the TS backend earns the transport-stream
/// scan limit; everything else gets the flat non-TS head budget.
fn stdin_budget(head: &[u8], scan_limit: u64) -> usize {
    if containers::sniffs_as_ts(head) {
        scan_limit as usize
    } else {
        STDIN_HEAD_BYTES
    }
}

/// `audioprobe -`: probe a bounded head of stdin. The buffered head feeds the
/// same sniff-dispatched pipeline a file probe runs; a stream that ended within
/// the budget is complete and reports exactly like a file, one that exceeded it
/// is marked truncated so the report says the tracks came from a prefix.
fn probe_stdin(opts: &containers::Options) -> report::Report {
    let fail = |msg: String| report::Report {
        path: "-".into(),
        error: Some(msg),
        ..report::Report::default()
    };

    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return fail("stdin is a terminal; pipe stream data in or pass a file path".into());
    }

    let scan_limit = opts.scan_limit;
    let (buf, truncated) =
        match read_bounded_head(stdin.lock(), STDIN_SNIFF_BYTES, |h| stdin_budget(h, scan_limit)) {
            Ok(v) => v,
            Err(e) => return fail(format!("reading stdin: {e}")),
        };
    if buf.is_empty() {
        return fail("no data on stdin".into());
    }

    match containers::probe_stream(buf, opts) {
        Ok(mut report) => {
            report.path = "-".into();
            report.truncated = truncated;
            report
        }
        Err(err) => fail(err),
    }
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
    // `-` (stdin) can carry at most one stream per invocation.
    if args.paths.iter().filter(|p| p.as_os_str() == "-").count() > 1 {
        eprintln!("error: '-' (stdin) may be given at most once");
        return ExitCode::from(1);
    }

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
        .map(|f| {
            if f.as_os_str() == "-" {
                probe_stdin(&opts)
            } else {
                containers::probe_path(f, &opts)
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_head_flags_a_stream_past_its_budget() {
        // 100 bytes behind a 4-byte sniff block, budget 16: truncated to 16.
        let data = vec![0xAAu8; 100];
        let (buf, truncated) =
            read_bounded_head(Cursor::new(data), 4, |_| 16).unwrap();
        assert!(truncated);
        assert_eq!(buf.len(), 16);
    }

    #[test]
    fn bounded_head_keeps_a_stream_within_its_budget() {
        // Exactly-budget and under-budget streams are complete, not truncated,
        // and the extra probe byte is never surfaced.
        let (buf, truncated) =
            read_bounded_head(Cursor::new(vec![1u8; 16]), 4, |_| 16).unwrap();
        assert!(!truncated);
        assert_eq!(buf.len(), 16);

        let (buf, truncated) =
            read_bounded_head(Cursor::new(vec![1u8; 9]), 4, |_| 16).unwrap();
        assert!(!truncated);
        assert_eq!(buf.len(), 9);
    }

    #[test]
    fn bounded_head_handles_an_empty_stream() {
        let (buf, truncated) = read_bounded_head(Cursor::new(Vec::new()), 4, |_| 16).unwrap();
        assert!(!truncated);
        assert!(buf.is_empty());
    }

    #[test]
    fn stdin_budget_couples_ts_to_the_scan_limit() {
        // A block that sync-locks as 188-byte TS earns the scan limit; anything
        // else gets the flat non-TS head budget.
        let mut ts = vec![0u8; 188 * 5 + 1];
        for i in 0..5 {
            ts[i * 188] = 0x47;
        }
        assert_eq!(stdin_budget(&ts, 64 * 1024 * 1024), 64 * 1024 * 1024);
        assert_eq!(stdin_budget(&[0u8; 1024], 64 * 1024 * 1024), STDIN_HEAD_BYTES);
        assert_eq!(stdin_budget(&[], 64 * 1024 * 1024), STDIN_HEAD_BYTES);
    }
}
