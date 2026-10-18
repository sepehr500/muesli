//! Recording session state machine. Pure: `step(state, event, policy) -> (state, effects)`.
//! The daemon owns time, I/O and IDs; this module only decides.

use crate::detect::{App, Candidate};
use serde::{Deserialize, Serialize};

pub type Millis = i64;

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Global default; `per_app` overrides.
    pub auto_record: bool,
    pub per_app: Vec<(App, bool)>,
    pub start_debounce_ms: Millis,
    pub end_grace_ms: Millis,
}

impl Policy {
    pub fn auto_for(&self, app: App) -> bool {
        self.per_app.iter().find(|(a, _)| *a == app).map(|(_, v)| *v).unwrap_or(self.auto_record)
    }
}

impl Default for Policy {
    fn default() -> Self {
        Policy { auto_record: true, per_app: vec![], start_debounce_ms: 5_000, end_grace_ms: 20_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    Idle,
    /// A call was seen; waiting out the start debounce.
    Seen { candidate: Candidate, since: Millis },
    /// auto_record is off: asking the user.
    Prompting { candidate: Candidate, since: Millis },
    Recording {
        app: Option<App>,
        hint: Option<String>,
        started: Millis,
        manual: bool,
        call_present: bool,
        gone_since: Option<Millis>,
    },
    /// User declined (or stopped) during this call; stay quiet until it ends.
    Ignored { app: App },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Latest detector verdict. Send on every change and periodically while not Idle.
    Observed { candidate: Option<Candidate>, now: Millis },
    Tick { now: Millis },
    Record { now: Millis },
    Ignore,
    Start { now: Millis },
    Stop,
    Discard,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    RecordingStarted { app: Option<App>, hint: Option<String> },
    AskToRecord { app: App, hint: Option<String> },
    Dismiss,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    StartRecording { app: Option<App>, hint: Option<String>, started: Millis },
    StopRecording,
    DiscardRecording,
    Notify(Notice),
}

fn recording(c: &Candidate, now: Millis, manual: bool) -> State {
    State::Recording {
        app: Some(c.app),
        hint: c.hint.clone(),
        started: now,
        manual,
        call_present: true,
        gone_since: None,
    }
}

fn start_effects(c: &Candidate, now: Millis, notify: bool) -> Vec<Effect> {
    let start = Effect::StartRecording { app: Some(c.app), hint: c.hint.clone(), started: now };
    match notify {
        true => vec![
            start,
            Effect::Notify(Notice::RecordingStarted { app: Some(c.app), hint: c.hint.clone() }),
        ],
        false => vec![start],
    }
}

/// After the debounce: record or ask.
fn mature(c: Candidate, now: Millis, p: &Policy) -> (State, Vec<Effect>) {
    match p.auto_for(c.app) {
        true => (recording(&c, now, false), start_effects(&c, now, true)),
        false => {
            let ask = Effect::Notify(Notice::AskToRecord { app: c.app, hint: c.hint.clone() });
            (State::Prompting { candidate: c, since: now }, vec![ask])
        }
    }
}

/// End a recording; if the call is still going, stay Ignored so we don't re-trigger.
fn end_recording(app: Option<App>, call_present: bool, effect: Effect) -> (State, Vec<Effect>) {
    let next = match (app, call_present) {
        (Some(app), true) => State::Ignored { app },
        _ => State::Idle,
    };
    (next, vec![effect])
}

pub fn step(state: State, event: Event, p: &Policy) -> (State, Vec<Effect>) {
    use Event as E;
    use State as S;
    match (state, event) {
        // ---- Idle
        (S::Idle, E::Observed { candidate: Some(c), now }) => {
            (S::Seen { candidate: c, since: now }, vec![])
        }
        (S::Idle, E::Start { now }) => (
            S::Recording {
                app: None,
                hint: None,
                started: now,
                manual: true,
                call_present: false,
                gone_since: None,
            },
            vec![
                Effect::StartRecording { app: None, hint: None, started: now },
                Effect::Notify(Notice::RecordingStarted { app: None, hint: None }),
            ],
        ),

        // ---- Seen (debounce)
        (S::Seen { .. }, E::Observed { candidate: None, .. }) => (S::Idle, vec![]),
        (S::Seen { candidate, since }, E::Observed { candidate: Some(c), now })
            if now - since >= p.start_debounce_ms =>
        {
            mature(Candidate { hint: c.hint.or(candidate.hint), ..c }, now, p)
        }
        (S::Seen { candidate, since }, E::Observed { candidate: Some(c), .. }) => {
            (S::Seen { candidate: Candidate { hint: c.hint.or(candidate.hint), ..c }, since }, vec![])
        }
        (S::Seen { candidate, since }, E::Tick { now }) if now - since >= p.start_debounce_ms => {
            mature(candidate, now, p)
        }
        (S::Seen { candidate, .. }, E::Start { now }) | (S::Seen { candidate, .. }, E::Record { now }) => {
            (recording(&candidate, now, false), start_effects(&candidate, now, false))
        }

        // ---- Prompting
        (S::Prompting { candidate, .. }, E::Record { now })
        | (S::Prompting { candidate, .. }, E::Start { now }) => (
            recording(&candidate, now, false),
            [vec![Effect::Notify(Notice::Dismiss)], start_effects(&candidate, now, false)].concat(),
        ),
        (S::Prompting { candidate, .. }, E::Ignore) => {
            (S::Ignored { app: candidate.app }, vec![Effect::Notify(Notice::Dismiss)])
        }
        (S::Prompting { .. }, E::Observed { candidate: None, .. }) => {
            (S::Idle, vec![Effect::Notify(Notice::Dismiss)])
        }

        // ---- Recording
        (S::Recording { app, hint, started, manual, gone_since, .. }, E::Observed { candidate, now }) => {
            let present = candidate.is_some();
            let gone_since = match present {
                true => None,
                false => gone_since.or(Some(now)),
            };
            // Adopt a late-arriving hint (e.g. window title appeared after the stream).
            let hint = hint.or(candidate.as_ref().and_then(|c| c.hint.clone()));
            let app = app.or(candidate.map(|c| c.app));
            (S::Recording { app, hint, started, manual, call_present: present, gone_since }, vec![])
        }
        (S::Recording { manual: false, gone_since: Some(g), app, .. }, E::Tick { now })
            if now - g >= p.end_grace_ms =>
        {
            end_recording(app, false, Effect::StopRecording)
        }
        (S::Recording { app, call_present, .. }, E::Stop) => {
            end_recording(app, call_present, Effect::StopRecording)
        }
        (S::Recording { app, call_present, .. }, E::Discard) => {
            end_recording(app, call_present, Effect::DiscardRecording)
        }

        // ---- Ignored
        (S::Ignored { .. }, E::Observed { candidate: None, .. }) => (S::Idle, vec![]),
        (S::Ignored { app }, E::Start { now }) | (S::Ignored { app }, E::Record { now }) => {
            let c = Candidate { app, hint: None, node_id: 0 };
            (recording(&c, now, true), start_effects(&c, now, false))
        }

        // Everything else is a no-op.
        (s, _) => (s, vec![]),
    }
}

/// Does this state need periodic ticks (debounce / grace timers)?
pub fn needs_ticks(s: &State) -> bool {
    matches!(s, State::Seen { .. } | State::Recording { gone_since: Some(_), .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(app: App) -> Candidate {
        Candidate { app, hint: Some("Hiking crew · #planning".into()), node_id: 9 }
    }
    fn obs(app: Option<App>, now: Millis) -> Event {
        Event::Observed { candidate: app.map(cand), now }
    }
    fn run(p: &Policy, events: Vec<Event>) -> (State, Vec<Effect>) {
        events.into_iter().fold((State::Idle, vec![]), |(s, mut acc), e| {
            let (s, fx) = step(s, e, p);
            acc.extend(fx);
            (s, acc)
        })
    }

    #[test]
    fn auto_record_after_debounce_then_stops_after_grace() {
        let p = Policy::default();
        let (s, fx) = run(
            &p,
            vec![
                obs(Some(App::Discord), 0),
                Event::Tick { now: 4_000 },
                Event::Tick { now: 5_000 },
                obs(None, 60_000),
                Event::Tick { now: 70_000 },
                Event::Tick { now: 80_000 },
            ],
        );
        assert_eq!(s, State::Idle);
        assert!(matches!(fx[0], Effect::StartRecording { app: Some(App::Discord), started: 5_000, .. }));
        assert!(matches!(fx[1], Effect::Notify(Notice::RecordingStarted { .. })));
        assert_eq!(fx[2], Effect::StopRecording);
        assert_eq!(fx.len(), 3);
    }

    #[test]
    fn short_mic_test_never_records() {
        let (s, fx) = run(&Policy::default(), vec![obs(Some(App::Discord), 0), obs(None, 3_000)]);
        assert_eq!((s, fx), (State::Idle, vec![]));
    }

    #[test]
    fn call_flapping_within_grace_keeps_recording() {
        let (s, fx) = run(
            &Policy::default(),
            vec![
                obs(Some(App::Zoom), 0),
                Event::Tick { now: 5_000 },
                obs(None, 10_000),
                obs(Some(App::Zoom), 20_000),
                Event::Tick { now: 40_000 },
            ],
        );
        assert!(matches!(s, State::Recording { gone_since: None, .. }));
        assert!(!fx.contains(&Effect::StopRecording));
    }

    #[test]
    fn ask_mode_waits_for_record() {
        let p = Policy { auto_record: false, ..Default::default() };
        let (s, fx) = run(&p, vec![obs(Some(App::Discord), 0), Event::Tick { now: 5_000 }]);
        assert!(matches!(s, State::Prompting { .. }));
        assert!(matches!(fx[..], [Effect::Notify(Notice::AskToRecord { app: App::Discord, .. })]));
        let (s, fx) = step(s, Event::Record { now: 7_000 }, &p);
        assert!(matches!(s, State::Recording { started: 7_000, .. }));
        assert!(fx.iter().any(|e| matches!(e, Effect::StartRecording { .. })));
    }

    #[test]
    fn ignore_suppresses_until_call_ends() {
        let p = Policy { auto_record: false, ..Default::default() };
        let (s, _) = run(
            &p,
            vec![obs(Some(App::Discord), 0), Event::Tick { now: 5_000 }, Event::Ignore],
        );
        assert_eq!(s, State::Ignored { app: App::Discord });
        let (s, fx) = step(s, obs(Some(App::Discord), 9_000), &p);
        assert_eq!((s.clone(), fx), (State::Ignored { app: App::Discord }, vec![]));
        let (s, _) = step(s, obs(None, 10_000), &p);
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn stopping_mid_call_does_not_retrigger() {
        let (s, fx) = run(
            &Policy::default(),
            vec![obs(Some(App::Zoom), 0), Event::Tick { now: 5_000 }, Event::Stop],
        );
        assert_eq!(s, State::Ignored { app: App::Zoom });
        assert!(fx.contains(&Effect::StopRecording));
    }

    #[test]
    fn per_app_override() {
        let p = Policy { auto_record: true, per_app: vec![(App::Discord, false)], ..Default::default() };
        let (s, _) = run(&p, vec![obs(Some(App::Discord), 0), Event::Tick { now: 5_000 }]);
        assert!(matches!(s, State::Prompting { .. }));
    }

    #[test]
    fn manual_recording_ignores_call_end() {
        let p = Policy::default();
        let (s, fx) = run(
            &p,
            vec![Event::Start { now: 0 }, obs(None, 1_000), Event::Tick { now: 60_000 }],
        );
        assert!(matches!(s, State::Recording { manual: true, .. }));
        assert!(!fx.contains(&Effect::StopRecording));
    }
}
