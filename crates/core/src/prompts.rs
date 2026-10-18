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
             (people addressing each other, introductions). Map every label you can.",
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

pub fn parse_enhanced(s: &str) -> Option<(EnhancedNotes, BTreeMap<String, String>)> {
    let r: EnhanceReply = serde_json::from_str(extract_json(s)?).ok()?;
    let speakers = r
        .speakers
        .into_iter()
        .map(|(k, v)| (k.trim_matches(['[', ']']).to_string(), v.trim().to_string()))
        .filter(|(_, v)| {
            let l = v.to_ascii_lowercase();
            !l.is_empty() && !l.starts_with("speaker") && !l.contains("unknown") && l != "you" && !l.starts_with("participant")
        })
        .collect();
    Some((r.notes, speakers))
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
    fn clip_keeps_ends() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let c = clip(&s, 50);
        assert!(c.starts_with("aaaa") && c.ends_with("bbbb") && c.len() < 120);
    }
}
