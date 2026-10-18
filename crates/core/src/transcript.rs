//! Pure transcript logic: time mapping, STT response parsing, channel merge, echo backstop.

use crate::model::{Segment, Speaker};
use serde::{Deserialize, Serialize};

/// Maps time in a trimmed (silence-removed) chunk back to meeting time.
/// Each span: `len` seconds that start at `src` in the meeting and at `dst` in the chunk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeMap {
    pub spans: Vec<MapSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MapSpan {
    pub src: f64,
    pub dst: f64,
    pub len: f64,
}

impl TimeMap {
    /// Build from kept spans `(src_start, len)` laid end to end.
    pub fn from_kept(kept: &[(f64, f64)]) -> Self {
        let spans = kept
            .iter()
            .scan(0.0, |dst, &(src, len)| {
                let s = MapSpan { src, dst: *dst, len };
                *dst += len;
                Some(s)
            })
            .collect();
        TimeMap { spans }
    }

    pub fn total(&self) -> f64 {
        self.spans.last().map(|s| s.dst + s.len).unwrap_or(0.0)
    }

    /// Chunk time → meeting time. Clamps into the nearest span.
    pub fn to_src(&self, t: f64) -> f64 {
        let span = self
            .spans
            .iter()
            .rev()
            .find(|s| s.dst <= t)
            .or(self.spans.first());
        match span {
            Some(s) => s.src + (t - s.dst).clamp(0.0, s.len),
            None => t,
        }
    }
}

/// One segment as returned by the STT endpoint (`verbose_json`), chunk-relative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub speaker: Option<serde_json::Value>,
}

/// One word with timing (chunk-relative), from `verbose_json` `words`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawWord {
    pub word: String,
    pub start: f64,
    pub end: f64,
    /// Diarization label, when the provider returns one (e.g. ElevenLabs with `diarize`).
    #[serde(default)]
    pub speaker: Option<serde_json::Value>,
}

/// A pause this long ends a turn at a sentence end (meeting time, so trimmed silence counts)…
const TURN_PAUSE_S: f64 = 1.2;
/// …and a pause this long ends it anywhere.
const TURN_GAP_S: f64 = 3.0;
/// Long monologues are split at the next sentence end past this length.
const TURN_MAX_S: f64 = 25.0;

/// Remove non-speech tags: `[clears throat]`, `[pause]`, `<|speaker:0|>`; collapse whitespace.
pub fn strip_tags(text: &str) -> String {
    let (out, _) = text.chars().fold((String::new(), 0u32), |(mut out, depth), c| match c {
        '[' => (out, depth + 1),
        ']' if depth > 0 => (out, depth - 1),
        _ if depth > 0 => (out, depth),
        c => {
            out.push(c);
            (out, depth)
        }
    });
    let out = strip_between(&out, "<|", "|>");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_between(s: &str, open: &str, close: &str) -> String {
    match s.find(open) {
        None => s.to_string(),
        Some(a) => match s[a..].find(close) {
            None => s.to_string(),
            Some(b) => format!("{}{}", &s[..a], strip_between(&s[a + b + close.len()..], open, close)),
        },
    }
}

/// Group timed words into turns (meeting time): a new turn starts on a speaker change, a long
/// gap, a pause after a sentence end, or the first sentence end once a turn runs past `TURN_MAX_S`.
pub fn turns_from_words(words: &[RawWord], map: &TimeMap) -> Vec<(Option<String>, f64, f64, String)> {
    let sentence_end = |w: &str| w.trim_end_matches(['"', '\'', ')']).ends_with(['.', '?', '!']);
    words
        .iter()
        .filter(|w| !w.word.trim().is_empty())
        .map(|w| (w.speaker.as_ref().and_then(speaker_label), map.to_src(w.start), map.to_src(w.end.max(w.start)), w.word.trim()))
        .fold(Vec::<(Option<String>, f64, f64, String)>::new(), |mut turns, (who, a, b, word)| {
            let split = match turns.last() {
                None => true,
                Some((prev, t0, t1, text)) => {
                    let last = text.rsplit(' ').next().unwrap_or("");
                    let (gap, ends) = (a - t1, sentence_end(last));
                    *prev != who || gap > TURN_GAP_S || (ends && (gap > TURN_PAUSE_S || t1 - t0 > TURN_MAX_S))
                }
            };
            match (split, turns.last_mut()) {
                (false, Some(t)) => {
                    t.2 = b;
                    t.3.push(' ');
                    t.3.push_str(word);
                }
                _ => turns.push((who, a, b, word.to_string())),
            }
            turns
        })
}

/// Split inline speaker markers some models emit, e.g.
/// "[Speaker 1]: hi [Speaker 2]: hello", "<speaker:0> hi", "Speaker A: hi".
/// Returns (speaker label, text) turns; label None when no markers present.
pub fn split_inline_speakers(text: &str) -> Vec<(Option<String>, String)> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    // Find marker positions: optional '[' or '<', "speaker", optional sep, id, optional ']' '>', optional ':'.
    let mut marks: Vec<(usize, usize, String)> = vec![]; // (start, end, label)
    let mut i = 0;
    while let Some(pos) = lower[i..].find("speaker") {
        let at = i + pos;
        let start = (0..at).rev().take_while(|&p| matches!(bytes[p], b'[' | b'<' | b'(' | b'|')).last().unwrap_or(at);
        let mut j = at + "speaker".len();
        while j < bytes.len() && matches!(bytes[j], b' ' | b'_' | b':' | b'-' | b'=') {
            j += 1;
        }
        let id_start = j;
        while j < bytes.len() && bytes[j].is_ascii_alphanumeric() && j - id_start < 3 {
            j += 1;
        }
        let id = &text[id_start..j];
        let mut k = j;
        while k < bytes.len() && matches!(bytes[k], b']' | b'>' | b')' | b'|') {
            k += 1;
        }
        let closed = k > j || (k < bytes.len() && bytes[k] == b':');
        while k < bytes.len() && bytes[k] == b':' {
            k += 1;
        }
        let at_turn_boundary = start == 0 || text[..start].trim_end().ends_with(['.', '?', '!', ']', '>', '\n']) || closed;
        if !id.is_empty() && closed && at_turn_boundary {
            marks.push((start, k, id.to_ascii_uppercase()));
        }
        i = at + "speaker".len();
    }
    if marks.is_empty() {
        return vec![(None, text.trim().to_string())];
    }
    let lead = text[..marks[0].0].trim();
    let first = (!lead.is_empty()).then(|| (None, lead.to_string()));
    let turns = marks.iter().enumerate().filter_map(|(n, (_, end, label))| {
        let stop = marks.get(n + 1).map(|m| m.0).unwrap_or(text.len());
        let t = text[*end..stop].trim();
        (!t.is_empty()).then(|| (Some(label.clone()), t.to_string()))
    });
    first.into_iter().chain(turns).collect()
}

fn speaker_label(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Turn one STT response into meeting-time segments.
///
/// * `me`: mic channel — everything is `Speaker::Me`.
/// * otherwise speakers become `Them("c{chunk}s{label}")` (chunk-local; linked later by the LLM).
pub fn segments_from_stt(
    text: &str,
    raw: &[RawSegment],
    words: &[RawWord],
    map: &TimeMap,
    me: bool,
    chunk: usize,
) -> Vec<Segment> {
    let who = |label: Option<String>| match me {
        true => Speaker::Me,
        false => Speaker::Them(format!("c{chunk}s{}", label.unwrap_or_else(|| "0".into()))),
    };
    let clean = |segs: Vec<Segment>| -> Vec<Segment> {
        segs.into_iter()
            .map(|s| Segment { text: strip_tags(&s.text), ..s })
            .filter(|s| s.text.chars().any(char::is_alphanumeric))
            .collect()
    };
    // Word timings give real turns. Use them unless speakers only exist as inline markers
    // (fish-audio), which word lists don't carry.
    let word_speakers = words.iter().any(|w| w.speaker.as_ref().and_then(speaker_label).is_some());
    let has_markers = |t: &str| split_inline_speakers(t).iter().any(|(l, _)| l.is_some());
    let inline_speakers = has_markers(text) || raw.iter().any(|r| has_markers(&r.text));
    if !words.is_empty() && (me || word_speakers || !inline_speakers) {
        let segs = turns_from_words(words, map)
            .into_iter()
            .map(|(label, a, b, t)| Segment { speaker: who(label), t_start: a, t_end: b, text: t })
            .collect();
        return clean(segs);
    }
    // Global word cursor: lets multi-turn segments be timed by their words.
    let mut cursor = 0usize;
    let mut word_time = |n_words: usize, fallback: (f64, f64)| -> (f64, f64) {
        let (a, b) = (cursor, cursor + n_words);
        cursor = b;
        match (words.get(a), words.get(b.saturating_sub(1).max(a))) {
            (Some(w0), Some(w1)) if n_words > 0 => (w0.start, w1.end.max(w0.start)),
            _ => fallback,
        }
    };
    let mut segs = vec![];
    for r in raw.iter().filter(|r| !r.text.trim().is_empty()) {
        let turns = split_inline_speakers(&r.text);
        let n = turns.len().max(1) as f64;
        let step = (r.end - r.start).max(0.0) / n;
        let seg_label = r.speaker.as_ref().and_then(speaker_label);
        for (k, (label, t)) in turns.into_iter().enumerate() {
            let even = (r.start + step * k as f64, r.start + step * (k as f64 + 1.0));
            let (a, b) = word_time(t.split_whitespace().count(), even);
            segs.push(Segment {
                speaker: who(label.or(seg_label.clone())),
                t_start: map.to_src(a),
                t_end: map.to_src(b),
                text: t,
            });
        }
    }
    if !segs.is_empty() {
        return clean(segs);
    }
    // No timestamps: spread inline turns evenly over the chunk.
    let turns = split_inline_speakers(text);
    let turns: Vec<_> = turns.into_iter().filter(|(_, t)| !t.is_empty()).collect();
    let n = turns.len().max(1) as f64;
    let total = map.total();
    let segs = turns
        .into_iter()
        .enumerate()
        .map(|(k, (label, t))| Segment {
            speaker: who(label),
            t_start: map.to_src(total * k as f64 / n),
            t_end: map.to_src(total * (k as f64 + 1.0) / n),
            text: t,
        })
        .collect();
    clean(segs)
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Is `me` likely residual echo of `them`? Fuzzy text match + time overlap (±window s).
pub fn is_echo(me: &Segment, them: &Segment, window: f64) -> bool {
    let overlaps = me.t_start <= them.t_end + window && them.t_start <= me.t_end + window;
    if !overlaps {
        return false;
    }
    let (a, b) = (normalize(&me.text), normalize(&them.text));
    if a.len() < 4 {
        return false;
    }
    b.contains(&a) || strsim::normalized_levenshtein(&a, &b) >= 0.75
}

/// Drop "Me" segments that duplicate what the other side said around the same time.
pub fn drop_echo(me: Vec<Segment>, them: &[Segment]) -> Vec<Segment> {
    me.into_iter().filter(|m| !them.iter().any(|t| is_echo(m, t, 2.0))).collect()
}

/// Interleave both channels by start time.
pub fn merge(me: Vec<Segment>, them: Vec<Segment>) -> Vec<Segment> {
    let mut all: Vec<Segment> = me.into_iter().chain(them).collect();
    all.sort_by(|a, b| a.t_start.total_cmp(&b.t_start));
    all
}

/// "Me" / mapped name / "Speaker 2".
pub fn display_speaker(s: &Speaker, names: &std::collections::BTreeMap<String, String>) -> String {
    match s {
        Speaker::Me => "You".into(),
        Speaker::Them(l) => names.get(l).cloned().unwrap_or_else(|| {
            let n = l.rsplit('s').next().unwrap_or("0");
            match n.parse::<u32>() {
                Ok(i) => format!("Speaker {}", i + 1),
                Err(_) => format!("Speaker {n}"),
            }
        }),
    }
}

/// Name map after renaming the person shown as `from` to `to`. One visible person can span
/// several chunk-local labels, so every label currently displayed as `from` is renamed.
/// An empty `to` resets them to "Speaker N". `None` if nobody is shown as `from`.
pub fn rename_speaker(
    segs: &[Segment],
    names: &std::collections::BTreeMap<String, String>,
    from: &str,
    to: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let labels: std::collections::BTreeSet<String> = segs
        .iter()
        .filter_map(|s| match &s.speaker {
            Speaker::Them(l) if display_speaker(&s.speaker, names) == from => Some(l.clone()),
            _ => None,
        })
        .collect();
    let to = to.trim();
    (!labels.is_empty()).then(|| {
        names
            .iter()
            .filter(|(k, _)| !labels.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(labels.iter().filter(|_| !to.is_empty()).map(|l| (l.clone(), to.to_string())))
            .collect()
    })
}

/// Distinct other speakers after name linking.
pub fn other_speaker_count(segs: &[Segment], names: &std::collections::BTreeMap<String, String>) -> usize {
    let mut v: Vec<String> = segs
        .iter()
        .filter(|s| s.speaker != Speaker::Me)
        .map(|s| display_speaker(&s.speaker, names))
        .collect();
    v.sort();
    v.dedup();
    v.len()
}

/// Plain-text transcript for prompts: "[03:12] You: ..." (raw labels for Them).
pub fn render_for_prompt(segs: &[Segment]) -> String {
    segs.iter()
        .map(|s| {
            let t = s.t_start.max(0.0) as u64;
            let who = match &s.speaker {
                Speaker::Me => "You".to_string(),
                Speaker::Them(l) => format!("[{l}]"),
            };
            format!("[{:02}:{:02}] {who}: {}", t / 60, t % 60, s.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(sp: Speaker, a: f64, b: f64, t: &str) -> Segment {
        Segment { speaker: sp, t_start: a, t_end: b, text: t.into() }
    }

    fn w(word: &str, start: f64, end: f64, spk: Option<u32>) -> RawWord {
        RawWord { word: word.into(), start, end, speaker: spk.map(|n| serde_json::json!(n)) }
    }

    #[test]
    fn strips_sound_tags() {
        assert_eq!(strip_tags("[clear throat] For what purposes? [click noise]"), "For what purposes?");
        assert_eq!(strip_tags("<|speaker:0|> Yo, [pause] hello"), "Yo, hello");
        assert_eq!(strip_tags("[the other man is saying \"Yeah.\" simultaneously]"), "");
    }

    #[test]
    fn word_turns_split_on_speaker_and_pause() {
        // Trimmed audio: 0-4 s maps to 0-4 s, 4-8 s maps to 10-14 s (6 s of silence removed).
        let map = TimeMap::from_kept(&[(0.0, 4.0), (10.0, 14.0)]);
        let words = [
            w("Yo,", 0.3, 0.4, Some(0)),
            w("hello.", 0.5, 0.7, Some(0)),
            w("You", 2.6, 2.7, Some(1)),
            w("up?", 2.8, 3.0, Some(1)),
            w("Same", 3.1, 3.3, Some(1)),
            w("guy.", 4.2, 4.4, Some(1)), // after the trimmed gap: 6 s pause in meeting time
            w("And", 5.7, 5.8, Some(1)),  // 1.3 s pause after a sentence end
            w("then,", 5.9, 6.0, Some(1)),
            w("more.", 7.5, 7.6, Some(1)), // 1.5 s pause mid-sentence: same turn
        ];
        let turns = turns_from_words(&words, &map);
        let texts: Vec<_> = turns.iter().map(|t| (t.0.as_deref(), t.3.as_str())).collect();
        assert_eq!(
            texts,
            [(Some("0"), "Yo, hello."), (Some("1"), "You up? Same"), (Some("1"), "guy."), (Some("1"), "And then, more.")]
        );
        assert!((turns[2].1 - 10.2).abs() < 1e-9);
    }

    #[test]
    fn long_monologue_splits_at_sentence_end() {
        let map = TimeMap::from_kept(&[(0.0, 100.0)]);
        let words: Vec<RawWord> = (0..60)
            .map(|i| w(if i % 10 == 9 { "end." } else { "word" }, i as f64, i as f64 + 0.5, None))
            .collect();
        let turns = turns_from_words(&words, &map);
        assert!(turns.len() >= 2);
        assert!(turns.iter().all(|t| t.2 - t.1 <= TURN_MAX_S + 10.0));
        assert!(turns[..turns.len() - 1].iter().all(|t| t.3.ends_with("end.")));
    }

    #[test]
    fn diarized_words_become_them_turns() {
        let map = TimeMap::from_kept(&[(0.0, 10.0)]);
        let words = [w("Hi", 0.0, 0.2, Some(0)), w("there.", 0.3, 0.5, Some(0)), w("Barry!", 0.7, 1.0, Some(2))];
        let segs = segments_from_stt("Hi there. Barry!", &[], &words, &map, false, 1);
        let who: Vec<_> = segs.iter().map(|s| (s.speaker.clone(), s.text.as_str())).collect();
        assert_eq!(who, [(Speaker::Them("c1s0".into()), "Hi there."), (Speaker::Them("c1s2".into()), "Barry!")]);
    }

    #[test]
    fn timemap_maps_back() {
        let m = TimeMap::from_kept(&[(10.0, 5.0), (30.0, 5.0)]);
        assert_eq!(m.total(), 10.0);
        assert_eq!(m.to_src(0.0), 10.0);
        assert_eq!(m.to_src(4.0), 14.0);
        assert_eq!(m.to_src(6.0), 31.0);
        assert_eq!(m.to_src(99.0), 35.0);
    }

    #[test]
    fn inline_speakers() {
        let t = split_inline_speakers("[Speaker 1]: Hi there. [Speaker 2]: Hello!");
        assert_eq!(
            t,
            vec![(Some("1".into()), "Hi there.".into()), (Some("2".into()), "Hello!".into())]
        );
        assert_eq!(split_inline_speakers("Just text."), vec![(None, "Just text.".into())]);
        let t = split_inline_speakers("<speaker:0> yes <speaker:1> no");
        assert_eq!(t.len(), 2);
        let t = split_inline_speakers("<|speaker:0|> Ideologically, yes. <|speaker:1|> No.");
        assert_eq!(t, vec![(Some("0".into()), "Ideologically, yes.".into()), (Some("1".into()), "No.".into())]);
        // "speaker" as a normal word is not a marker.
        assert_eq!(split_inline_speakers("The speaker was loud.").len(), 1);
    }

    #[test]
    fn stt_segments_mapped() {
        let map = TimeMap::from_kept(&[(100.0, 10.0)]);
        let raw = vec![RawSegment {
            start: 1.0,
            end: 3.0,
            text: "hello".into(),
            speaker: Some(serde_json::json!(1)),
        }];
        let s = segments_from_stt("hello", &raw, &[], &map, false, 2);
        assert_eq!(s, vec![seg(Speaker::Them("c2s1".into()), 101.0, 103.0, "hello")]);
        let s = segments_from_stt("[Speaker 1]: a. [Speaker 2]: b.", &[], &[], &map, false, 0);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].speaker, Speaker::Them("c0s2".into()));
        // fish-audio style: one segment, inline markers, word timings.
        let raw = vec![RawSegment {
            start: 0.0,
            end: 6.0,
            text: "<|speaker:0|> hi there <|speaker:1|> hello you two".into(),
            speaker: None,
        }];
        let w = |word: &str, start: f64| RawWord { word: word.into(), start, end: start + 0.5, speaker: None };
        let words = vec![w("hi", 0.0), w("there", 0.5), w("hello", 3.0), w("you", 3.5), w("two", 4.0)];
        let s = segments_from_stt("", &raw, &words, &map, false, 1);
        assert_eq!(s[0], seg(Speaker::Them("c1s0".into()), 100.0, 101.0, "hi there"));
        assert_eq!(s[1], seg(Speaker::Them("c1s1".into()), 103.0, 104.5, "hello you two"));
    }

    #[test]
    fn rename_covers_every_label_shown_as_that_person() {
        let segs = vec![
            seg(Speaker::Them("c1s1".into()), 0.0, 1.0, "a"),
            seg(Speaker::Them("c3s0".into()), 2.0, 3.0, "b"),
            seg(Speaker::Them("c3s1".into()), 4.0, 5.0, "c"),
            seg(Speaker::Me, 6.0, 7.0, "d"),
        ];
        let names: std::collections::BTreeMap<String, String> = [("c3s0".to_string(), "Lena".to_string())].into();
        // c1s1 and c3s1 both show as "Speaker 2".
        let n = rename_speaker(&segs, &names, "Speaker 2", " Barry ").unwrap();
        assert_eq!(n.get("c1s1").map(String::as_str), Some("Barry"));
        assert_eq!(n.get("c3s1").map(String::as_str), Some("Barry"));
        assert_eq!(n.get("c3s0").map(String::as_str), Some("Lena"));
        // Merge: naming Lena "Barry" too makes them one person.
        let n = rename_speaker(&segs, &n, "Lena", "Barry").unwrap();
        assert_eq!(other_speaker_count(&segs, &n), 1);
        // Reset back to "Speaker N".
        let n = rename_speaker(&segs, &n, "Barry", "").unwrap();
        assert!(n.is_empty());
        assert_eq!(rename_speaker(&segs, &n, "Nobody", "X"), None);
        assert_eq!(rename_speaker(&segs, &n, "You", "X"), None);
    }

    #[test]
    fn echo_backstop() {
        let them = vec![seg(Speaker::Them("c0s0".into()), 10.0, 14.0, "We should ship offline mode first.")];
        let me = vec![
            seg(Speaker::Me, 10.3, 14.2, "we should ship offline mode first"),
            seg(Speaker::Me, 15.0, 17.0, "Agreed, let's do it."),
            seg(Speaker::Me, 40.0, 44.0, "We should ship offline mode first."), // far away in time
        ];
        let kept = drop_echo(me, &them);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].text, "Agreed, let's do it.");
    }

    #[test]
    fn names_and_counts() {
        let names = [("c0s0".to_string(), "Lena".to_string()), ("c1s3".to_string(), "Lena".to_string())]
            .into_iter()
            .collect();
        let segs = vec![
            seg(Speaker::Me, 0.0, 1.0, "a"),
            seg(Speaker::Them("c0s0".into()), 1.0, 2.0, "b"),
            seg(Speaker::Them("c1s3".into()), 2.0, 3.0, "c"),
            seg(Speaker::Them("c1s1".into()), 3.0, 4.0, "d"),
        ];
        assert_eq!(other_speaker_count(&segs, &names), 2);
        assert_eq!(display_speaker(&Speaker::Them("c1s1".into()), &names), "Speaker 2");
    }
}
