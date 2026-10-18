//! Prompt builders and lenient response parsers. Pure.

use crate::model::EnhancedNotes;
use crate::openrouter::Message;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Trim very long transcripts to fit comfortably in context (keep head + tail).
pub fn clip(transcript: &str, max_chars: usize) -> String {
    if transcript.len() <= max_chars {
        return transcript.to_string();
    }
    let half = max_chars / 2;
    let head_end = transcript.char_indices().map(|(i, _)| i).take_while(|&i| i <= half).last().unwrap_or(0);
    let tail_start = transcript
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= transcript.len() - half)
        .unwrap_or(transcript.len());
    format!("{}\n[… middle of transcript omitted …]\n{}", &transcript[..head_end], &transcript[tail_start..])
}

pub fn title_messages(app_label: &str, hint: Option<&str>, transcript: &str, user_notes: &str) -> Vec<Message> {
    let hint = hint.map(|h| format!("Window/channel hint: {h}\n")).unwrap_or_default();
    let notes = if !user_notes.trim().is_empty() { format!("The user's own notes:\n{user_notes}\n\n") } else { Default::default() };
    vec![
        Message::system(
            "You name meetings. Reply with JSON only: {\"titles\": [\"...\", \"...\", \"...\"]}. \
             Give exactly 3 distinct titles, best first. Each 2–6 words in sentence case \
             (capitalize only the first word and proper nouns), specific to what was discussed, \
             no quotes, no dates, no trailing punctuation, never generic like \"Team meeting\".",
        ),
        Message::user(format!(
            "App: {app_label}\n{hint}\n{notes}Transcript:\n{}",
            clip(transcript, 24_000)
        )),
    ]
}

#[derive(Deserialize)]
struct Titles {
    titles: Vec<String>,
}

/// Extract the first JSON object in `s` (models sometimes wrap JSON in prose or fences).
pub fn extract_json(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| &s[start..=end])
}

pub fn parse_titles(s: &str) -> Vec<String> {
    extract_json(s)
        .and_then(|j| serde_json::from_str::<Titles>(j).ok())
        .map(|t| t.titles)
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.trim().trim_matches(['"', '.', '\'']).trim().to_string())
        .filter(|t| !t.is_empty() && t.len() <= 80)
        .take(3)
        .collect()
}

pub const DEFAULT_TEMPLATE: &str = "Sections to use when relevant (omit empty ones): \
\"Decisions\", \"Discussion\", \"Open questions\". Put to-dos in action_items, not in sections.";

pub fn enhance_messages(
    transcript: &str,
    user_notes: &str,
    template: &str,
    speaker_labels: &[String],
) -> Vec<Message> {
    let labels = match speaker_labels.is_empty() {
        true => String::new(),
        false => format!(
            "\nOther-speaker labels in the transcript: {}. Infer real names from context \
             (people addressing each other, introductions). In \"speakers\", include only labels whose real \
             name you are confident of; leave the others out.",
            speaker_labels.join(", ")
        ),
    };
    vec![
        Message::system(format!(
            "You turn a meeting transcript and the user's rough notes into crisp meeting notes.\n\
             The transcript has two channels: \"You\" is the user (their microphone), bracketed labels \
             like [c0s1] are other participants. Labels are chunk-local, so the same person may appear \
             under several labels.\n\
             Rules:\n\
             - The user's own notes are the skeleton. Keep every point they wrote, as written (fix only typos), \
               with from_user=true, and add a short 'detail' line with context from the transcript when useful.\n\
             - Add important points they missed with from_user=false. Be concise; no filler, no preamble.\n\
             - Action items: concrete, start with a verb, owner = a person's name or \"You\".\n\
             - Never invent facts not supported by the transcript.\n\
             - Never write the bracketed labels (like c0s1) in notes or owners. Use a person's real name when \
               the transcript makes it clear, otherwise a neutral phrase such as \"a participant\".\n\
             {template}{labels}\n\
             Reply with JSON only, exactly this shape:\n\
             {{\"sections\":[{{\"title\":\"Decisions\",\"items\":[{{\"text\":\"...\",\"from_user\":true,\"detail\":\"...\"}}]}}],\
             \"action_items\":[{{\"text\":\"...\",\"owner\":\"You\",\"from_user\":false}}],\
             \"speakers\":{{\"c0s1\":\"Lena\"}}}}"
        )),
        Message::user(format!(
            "User's notes:\n{}\n\nTranscript:\n{}",
            if user_notes.trim().is_empty() { "(none)" } else { user_notes },
            clip(transcript, 400_000)
        )),
    ]
}

#[derive(Deserialize)]
struct EnhanceReply {
    #[serde(flatten)]
    notes: EnhancedNotes,
    #[serde(default)]
    speakers: BTreeMap<String, String>,
}

/// Reject placeholders the model sometimes returns instead of a name.
fn is_real_name(v: &str) -> bool {
    let l = v.trim().to_ascii_lowercase();
    let generic = ["speaker", "unknown", "participant", "someone", "person", "user", "other", "guest", "friend"];
    !l.is_empty() && l != "you" && !generic.iter().any(|g| l.contains(g))
}

pub fn parse_enhanced(s: &str) -> Option<(EnhancedNotes, BTreeMap<String, String>)> {
    let r: EnhanceReply = serde_json::from_str(extract_json(s)?).ok()?;
    let speakers = r
        .speakers
        .into_iter()
        .map(|(k, v)| (k.trim_matches(['[', ']']).to_string(), v.trim().to_string()))
        .filter(|(_, v)| is_real_name(v))
        .collect();
    Some((r.notes, speakers))
}

/// Replace any raw speaker labels (`c0s1`, `[c0s1]`) that slipped into note text with the
/// linked name, or the same "Speaker N" the transcript shows.
pub fn scrub_labels(notes: EnhancedNotes, names: &BTreeMap<String, String>) -> EnhancedNotes {
    let fix = |t: String| scrub_text(&t, names);
    EnhancedNotes {
        sections: notes
            .sections
            .into_iter()
            .map(|s| crate::model::NoteSection {
                items: s
                    .items
                    .into_iter()
                    .map(|i| crate::model::NoteItem { text: fix(i.text), detail: i.detail.map(fix), ..i })
                    .collect(),
                ..s
            })
            .collect(),
        action_items: notes
            .action_items
            .into_iter()
            .map(|a| crate::model::ActionItem { text: fix(a.text), owner: a.owner.map(fix), ..a })
            .collect(),
    }
}

/// Is `w` a raw chunk speaker label like `c12s3`?
fn is_label(w: &str) -> bool {
    let rest = match w.strip_prefix('c') {
        Some(r) => r,
        None => return false,
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let tail = &rest[digits..];
    digits > 0
        && tail.strip_prefix('s').is_some_and(|id| (1..=3).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn scrub_text(text: &str, names: &BTreeMap<String, String>) -> String {
    let name = |l: &str| crate::transcript::display_speaker(&crate::model::Speaker::Them(l.to_string()), names);
    // Walk word-ish tokens, keeping all separators intact.
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        match is_label(word) {
            true => {
                if out.ends_with('[') {
                    out.pop();
                }
                out.push_str(&name(word));
            }
            false => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        match c.is_ascii_alphanumeric() {
            true => word.push(c),
            false => {
                let was_label = is_label(&word);
                flush(&mut word, &mut out);
                if !(was_label && c == ']') {
                    out.push(c);
                }
            }
        }
    }
    flush(&mut word, &mut out);
    out
}

pub fn ask_messages(question: &str, transcript: &str, notes_md: &str, history: &[Message]) -> Vec<Message> {
    let sys = Message::system(format!(
        "You answer questions about one meeting, using only the notes and transcript below. \
         Be brief and direct. If the answer isn't in the meeting, say so.\n\nNotes:\n{notes_md}\n\nTranscript:\n{}",
        clip(transcript, 400_000)
    ));
    std::iter::once(sys)
        .chain(history.iter().cloned())
        .chain(std::iter::once(Message::user(question)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lenient() {
        let r = "Sure!\n```json\n{\"titles\": [\"Offline sync spec review.\", \"\\\"Dec 12 release\\\"\", \"x\", \"y\"]}\n```";
        assert_eq!(parse_titles(r), vec!["Offline sync spec review", "Dec 12 release", "x"]);
        assert!(parse_titles("nope").is_empty());
    }

    #[test]
    fn enhanced_parse() {
        let r = r#"{"sections":[{"title":"Decisions","items":[{"text":"Ship","from_user":true,"detail":"Dec 12"}]}],
                    "action_items":[{"text":"Write spec","owner":"You"}],
                    "speakers":{"[c0s1]":"Lena","c0s2":"Speaker 2","c0s3":"Unknown Speaker"}}"#;
        let (n, s) = parse_enhanced(r).unwrap();
        assert_eq!(n.sections[0].items[0].detail.as_deref(), Some("Dec 12"));
        assert_eq!(n.action_items[0].owner.as_deref(), Some("You"));
        assert_eq!(s.len(), 1);
        assert_eq!(s["c0s1"], "Lena");
    }

    #[test]
    fn scrubs_raw_labels() {
        let names: BTreeMap<String, String> = [("c1s2".to_string(), "Barry".to_string())].into();
        assert_eq!(scrub_text("c1s0 wakes up early", &names), "Speaker 1 wakes up early");
        assert_eq!(scrub_text("Ask [c1s2] about it.", &names), "Ask Barry about it.");
        assert_eq!(scrub_text("disc1s0 and c3po", &names), "disc1s0 and c3po");
        assert!(!is_real_name("a participant") && !is_real_name("Unknown speaker") && is_real_name("Barry"));
    }

    #[test]
    fn clip_keeps_ends() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let c = clip(&s, 50);
        assert!(c.starts_with("aaaa") && c.ends_with("bbbb") && c.len() < 120);
    }
}
