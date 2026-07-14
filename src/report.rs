//! Result model plus text / JSON rendering.

#[derive(Debug, Default, Clone)]
pub struct Track {
    /// Container-specific identifier (Matroska track number, TS PID, MP4 track id).
    pub id: String,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    /// Whether an LFE channel is known to be present (drives "5.1"-style layout naming).
    pub lfe: Option<bool>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub note: Option<String>,
}

impl Track {
    pub fn layout(&self) -> Option<String> {
        let ch = self.channels?;
        match self.lfe {
            Some(true) if ch >= 2 => Some(format!("{}.1", ch - 1)),
            Some(false) => Some(format!("{}.0", ch)),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub path: String,
    pub container: String,
    pub tracks: Vec<Track>,
    pub error: Option<String>,
    /// Set only for `audioprobe -` when the piped stream exceeded the head
    /// budget: the tracks reported come from the probed prefix, so a track
    /// whose first frames sit beyond the cut may be missing or lack a
    /// sampled bit depth. Always `false` for file probes.
    pub truncated: bool,
}

fn fmt_rate(rate: Option<u32>) -> String {
    match rate {
        Some(r) => format!("{} Hz", r),
        None => "?".into(),
    }
}

fn fmt_depth(depth: Option<u32>) -> String {
    match depth {
        Some(d) => format!("{}-bit", d),
        None => "\u{2014}".into(), // em dash: not applicable / unknown
    }
}

fn fmt_channels(t: &Track) -> String {
    match (t.layout(), t.channels) {
        (Some(l), Some(ch)) => format!("{} ({} ch)", l, ch),
        (None, Some(ch)) => format!("{} ch", ch),
        _ => "?".into(),
    }
}

pub fn render_text(r: &Report, out: &mut String) {
    out.push_str(&format!("{}  [{}]\n", r.path, r.container));
    if let Some(err) = &r.error {
        out.push_str(&format!("  error: {}\n", err));
        return;
    }
    if r.truncated {
        out.push_str(
            "  note: input truncated at the head budget; later tracks may be missing\n",
        );
    }
    if r.tracks.is_empty() {
        out.push_str("  no audio tracks found\n");
        return;
    }
    // dynamic column widths
    let rows: Vec<[String; 6]> = r
        .tracks
        .iter()
        .map(|t| {
            [
                t.id.clone(),
                t.codec.clone(),
                fmt_rate(t.sample_rate),
                fmt_depth(t.bit_depth),
                fmt_channels(t),
                t.language.clone().unwrap_or_else(|| "\u{2014}".into()),
            ]
        })
        .collect();
    let header = ["#", "CODEC", "SAMPLE RATE", "BIT DEPTH", "CHANNELS", "LANG"];
    let mut w = [0usize; 6];
    for (i, h) in header.iter().enumerate() {
        w[i] = h.chars().count();
    }
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let pad = |s: &str, width: usize| {
        let mut s = s.to_string();
        while s.chars().count() < width {
            s.push(' ');
        }
        s
    };
    out.push_str("  ");
    for (i, h) in header.iter().enumerate() {
        out.push_str(&pad(h, w[i] + 2));
    }
    out.push('\n');
    for (row, t) in rows.iter().zip(&r.tracks) {
        out.push_str("  ");
        for (i, c) in row.iter().enumerate() {
            out.push_str(&pad(c, w[i] + 2));
        }
        if t.default {
            out.push_str("[default]");
        }
        if let Some(n) = &t.note {
            out.push_str(&format!("  ({})", n));
        }
        out.push('\n');
    }
}

pub fn render_quiet(r: &Report, out: &mut String) {
    out.push_str(&r.path);
    out.push_str(": ");
    if let Some(err) = &r.error {
        out.push_str(&format!("error: {}", err));
    } else if r.tracks.is_empty() {
        out.push_str("no audio tracks");
    } else {
        let parts: Vec<String> = r
            .tracks
            .iter()
            .map(|t| {
                let mut s = t.codec.clone();
                if let Some(rate) = t.sample_rate {
                    s.push_str(&format!(" {}Hz", rate));
                }
                if let Some(d) = t.bit_depth {
                    s.push_str(&format!("/{}bit", d));
                }
                if let Some(l) = t.layout() {
                    s.push_str(&format!(" {}", l));
                } else if let Some(ch) = t.channels {
                    s.push_str(&format!(" {}ch", ch));
                }
                if let Some(lang) = &t.language {
                    s.push_str(&format!(" ({})", lang));
                }
                s
            })
            .collect();
        out.push_str(&parts.join("; "));
    }
    if r.truncated {
        out.push_str(" [truncated]");
    }
    out.push('\n');
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_opt_str(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("\"{}\"", json_escape(s)),
        None => "null".into(),
    }
}

fn json_opt_num(v: Option<u32>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".into(),
    }
}

pub fn render_json(reports: &[Report]) -> String {
    let mut out = String::from("{\n  \"files\": [\n");
    for (fi, r) in reports.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"path\": \"{}\",\n", json_escape(&r.path)));
        out.push_str(&format!(
            "      \"container\": {},\n",
            if r.container.is_empty() {
                "null".into()
            } else {
                format!("\"{}\"", json_escape(&r.container))
            }
        ));
        out.push_str(&format!("      \"error\": {},\n", json_opt_str(&r.error)));
        if r.truncated {
            out.push_str("      \"input_truncated\": true,\n");
        }
        out.push_str("      \"audio_tracks\": [\n");
        for (ti, t) in r.tracks.iter().enumerate() {
            out.push_str("        {\n");
            out.push_str(&format!("          \"id\": \"{}\",\n", json_escape(&t.id)));
            out.push_str(&format!(
                "          \"codec\": \"{}\",\n",
                json_escape(&t.codec)
            ));
            out.push_str(&format!(
                "          \"sample_rate\": {},\n",
                json_opt_num(t.sample_rate)
            ));
            out.push_str(&format!(
                "          \"bit_depth\": {},\n",
                json_opt_num(t.bit_depth)
            ));
            out.push_str(&format!(
                "          \"channels\": {},\n",
                json_opt_num(t.channels)
            ));
            out.push_str(&format!(
                "          \"layout\": {},\n",
                json_opt_str(&t.layout())
            ));
            out.push_str(&format!(
                "          \"language\": {},\n",
                json_opt_str(&t.language)
            ));
            out.push_str(&format!(
                "          \"title\": {},\n",
                json_opt_str(&t.title)
            ));
            out.push_str(&format!("          \"note\": {},\n", json_opt_str(&t.note)));
            out.push_str(&format!("          \"default\": {}\n", t.default));
            out.push_str(if ti + 1 == r.tracks.len() {
                "        }\n"
            } else {
                "        },\n"
            });
        }
        out.push_str("      ]\n");
        out.push_str(if fi + 1 == reports.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    out.push_str("  ]\n}\n");
    out
}
