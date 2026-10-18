//! Seed demo data matching the Paper designs into `$MUESLI_HOME` (required; refuses otherwise).
//!
//!   MUESLI_HOME=/tmp/muesli-ui-demo cargo run -p muesli-ui --example seed

use chrono::{Duration, Local, NaiveTime, TimeZone};
use muesli_core::detect::App;
use muesli_core::model::*;
use muesli_core::store;

fn at(days_ago: i64, h: u32, m: u32) -> i64 {
    let day = Local::now().date_naive() - Duration::days(days_ago);
    Local
        .from_local_datetime(&day.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap()))
        .single()
        .unwrap()
        .timestamp_millis()
}

fn seg(who: &str, t: f64, text: &str) -> Segment {
    let speaker = match who {
        "me" => Speaker::Me,
        l => Speaker::Them(l.to_string()),
    };
    Segment { speaker, t_start: t, t_end: t + 6.0, text: text.into() }
}

fn item(text: &str, from_user: bool, detail: Option<&str>) -> NoteItem {
    NoteItem { text: text.into(), from_user, detail: detail.map(String::from) }
}

fn action(text: &str, owner: &str, from_user: bool) -> ActionItem {
    ActionItem { text: text.into(), owner: Some(owner.into()), from_user, done: false }
}

fn done_pipeline() -> Pipeline {
    StepKind::ALL.iter().fold(Pipeline::default(), |p, &k| p.with(k, StepStatus::Done, None))
}

struct Seed {
    meeting: Meeting,
    user_md: &'static str,
    enhanced: Option<EnhancedNotes>,
    segments: Vec<Segment>,
}

fn ready(title: &str, app: App, hint: Option<&str>, start: i64, mins: i64) -> Meeting {
    let mut m = store::new_meeting(Some(app), hint.map(String::from), start);
    m.title = Some(title.into());
    m.title_confirmed = true;
    m.ended_at = Some(start + mins * 60_000);
    m.state = MeetingState::Ready;
    m.pipeline = done_pipeline();
    m
}

fn names(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn seeds() -> Vec<Seed> {
    // Post-call, still naming + enhancing (the "Name this meeting" artboard).
    let mut untitled = store::new_meeting(Some(App::Zoom), None, at(0, 13, 25));
    untitled.ended_at = Some(untitled.started_at + 37 * 60_000);
    untitled.state = MeetingState::Processing;
    untitled.title_suggestions =
        vec!["Offline sync spec review".into(), "Conflict handling with Marco".into(), "Dec 12 release check-in".into()];
    untitled.pipeline = Pipeline::default()
        .with(StepKind::Saved, StepStatus::Done, Some("37 min of you and the call, kept on this computer".into()))
        .with(StepKind::Cleaned, StepStatus::Done, Some("Removed speaker echo and 15 min of silence".into()))
        .with(StepKind::Transcribed, StepStatus::Done, Some("You and 3 others · cost $0.13".into()))
        .with(StepKind::Notes, StepStatus::Active, Some("Filling in your bullets with decisions and to-dos".into()));
    untitled.cost_usd = 0.13;

    let mut q4 = ready("Q4 roadmap with Lena", App::Zoom, Some("Q4 roadmap"), at(0, 10, 30), 42);
    q4.speaker_names = names(&[("c0s0", "Lena"), ("c0s1", "Marco"), ("c0s2", "Priya")]);
    let q4_notes = EnhancedNotes {
        sections: vec![NoteSection {
            title: "Decisions".into(),
            items: vec![
                item(
                    "Ship offline mode before Christmas",
                    true,
                    Some("Lena wants it in the Dec 12 release. Sync conflicts resolve last-write-wins for v1."),
                ),
                item("Drop the Windows beta", true, Some("Under 3% of signups. Revisit in Q1 if demand grows.")),
                item("Pricing page refresh moves to January, owned by Priya.", false, None),
            ],
        }],
        action_items: vec![
            action("Write the offline sync spec", "You", true),
            action("Share Windows signup numbers with the team", "Marco", false),
            action("Draft January pricing options", "Priya", false),
        ],
    };

    let mut hiking = ready("Hiking crew · #planning", App::Discord, Some("Hiking crew · #planning"), at(0, 8, 15), 18);
    hiking.speaker_names = names(&[("c0s0", "Anja"), ("c0s1", "Reto")]);

    let onboarding = ready("Onboarding design review", App::Zoom, None, at(1, 16, 0), 55);
    let marco = {
        let mut m = ready("1:1 with Marco", App::Zoom, None, at(1, 11, 0), 28);
        m.speaker_names = names(&[("c0s0", "Marco")]);
        m
    };
    let rust = ready("Rust Linux audio · #voice", App::Discord, Some("Rust Linux audio · #voice"), at(1, 21, 40), 72);

    vec![
        Seed {
            meeting: untitled,
            user_md: "- offline sync spec\n- conflicts: last write wins?\n- Dec 12 release",
            enhanced: None,
            segments: vec![
                seg("me", 4.0, "Okay, let's walk through the offline sync spec."),
                seg("c0s0", 12.0, "I read it last night. The conflict section needs work."),
                seg("c0s1", 25.0, "Last write wins is fine for v1, honestly."),
                seg("c0s2", 40.0, "As long as we log the overwritten version somewhere."),
                seg("me", 55.0, "Agreed. Let's target the December 12 release."),
            ],
        },
        Seed {
            meeting: q4,
            user_md: "- ship offline mode before xmas\n- drop the windows beta\n- spec: me",
            enhanced: Some(q4_notes),
            segments: vec![
                seg("me", 3.0, "Thanks for making time, Lena. Let's start with offline mode."),
                seg("c0s0", 10.0, "I want it in the December 12 release. Conflicts can be last-write-wins for now."),
                seg("me", 22.0, "That works. I'll write the sync spec this week."),
                seg("c0s1", 31.0, "On Windows — the beta is under three percent of signups."),
                seg("c0s0", 40.0, "Then let's drop it and revisit in Q1 if demand grows."),
                seg("c0s1", 52.0, "I'll share the signup numbers with the team."),
                seg("c0s2", 61.0, "And I'll move the pricing page refresh to January and draft options."),
            ],
        },
        Seed {
            meeting: hiking,
            user_md: "- Saturday: Hörnli hut?\n- check weather Friday",
            enhanced: Some(EnhancedNotes {
                sections: vec![NoteSection {
                    title: "Plan".into(),
                    items: vec![
                        item("Saturday: Hörnli hut", true, Some("Meet at Zermatt station, 07:10 train.")),
                        item("Check the weather on Friday", true, None),
                        item("Anja brings the spare crampons.", false, None),
                    ],
                }],
                action_items: vec![action("Book the hut for four", "Reto", false)],
            }),
            segments: vec![
                seg("c0s0", 2.0, "So, Hörnli hut on Saturday?"),
                seg("me", 6.0, "Yes, if the weather holds. I'll check Friday."),
                seg("c0s1", 12.0, "I can book it for four of us."),
            ],
        },
        Seed {
            meeting: onboarding,
            user_md: "- welcome screen too busy\n- skip the tour by default",
            enhanced: Some(EnhancedNotes {
                sections: vec![NoteSection {
                    title: "Feedback".into(),
                    items: vec![
                        item("Welcome screen is too busy", true, Some("Cut it to one sentence and the key field.")),
                        item("Skip the product tour by default", true, None),
                    ],
                }],
                action_items: vec![action("Send revised welcome copy", "You", true)],
            }),
            segments: vec![seg("me", 5.0, "Let's look at the welcome screen first."), seg("c0s0", 9.0, "It's a lot.")],
        },
        Seed {
            meeting: marco,
            user_md: "- career: tech lead track\n- vacation in Nov",
            enhanced: Some(EnhancedNotes {
                sections: vec![NoteSection {
                    title: "Discussion".into(),
                    items: vec![
                        item("Tech lead track", true, Some("Marco wants to lead the sync rewrite in Q1.")),
                        item("Vacation in November", true, Some("Two weeks, from Nov 10.")),
                    ],
                }],
                action_items: vec![],
            }),
            segments: vec![seg("c0s0", 3.0, "I'd like to lead the sync rewrite."), seg("me", 9.0, "Let's make that happen.")],
        },
        Seed {
            meeting: rust,
            user_md: "",
            enhanced: Some(EnhancedNotes {
                sections: vec![NoteSection {
                    title: "Discussion".into(),
                    items: vec![
                        item("PipeWire filter nodes for echo cancellation work well offline.", false, None),
                        item("webrtc-audio-processing 2.x links against the system library.", false, None),
                    ],
                }],
                action_items: vec![action("Try the AEC pass on laptop speakers", "You", false)],
            }),
            segments: vec![seg("c0s0", 1.0, "Has anyone tried the webrtc AEC crate?"), seg("me", 7.0, "Yes, version two.")],
        },
    ]
}

fn main() -> anyhow::Result<()> {
    if std::env::var_os("MUESLI_HOME").is_none() {
        anyhow::bail!("set MUESLI_HOME to a scratch directory; refusing to touch real data");
    }
    let db = muesli_core::paths::db_file();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let mut c = store::open_default()?;
    for s in seeds() {
        store::upsert_meeting(&c, &s.meeting)?;
        store::set_user_notes(&c, &s.meeting.id, s.user_md)?;
        if let Some(e) = &s.enhanced {
            store::set_enhanced(&c, &s.meeting.id, Some(e))?;
        }
        store::replace_segments(&mut c, &s.meeting.id, &s.segments)?;
        println!("{}  {}", s.meeting.id, s.meeting.display_title());
    }
    println!("seeded {}", db.display());
    Ok(())
}
