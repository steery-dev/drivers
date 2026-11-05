//! What a rollout's newest records say of its conversation now: the model and thinking it
//! last ran on, the share of its context window left, its newest turn, and the calls of
//! that turn still running and lately ended. `idle` samples it ([`Tail::sample`]), and the
//! live state said of a thread a window or an app server owns carries it
//! ([`crate::router::Followers`]). Codex writes a call's `function_call` record as the
//! model asks for it and its `function_call_output` once it returns, so a call begun in the
//! turn and not answered runs.

use steery_sdk::wire::harness::{
    Event, Execution, LiveCall, LiveChild, LiveEnded, Sample, TurnState,
};

/// How many ended calls are kept, as `ended` takes at most.
const ENDED: usize = 64;

/// What the newest records of a rollout say.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub context_left: Option<f64>,
    /// The newest turn, where one is read.
    pub turn: Option<Turned>,
    /// When the newest record was written.
    pub written: Option<u64>,
    /// The top-level calls of the newest turn begun and not answered, oldest first.
    pub calls: Vec<LiveCall>,
    /// Top-level calls answered, oldest first, at most [`ENDED`].
    pub ended: Vec<LiveEnded>,
}

/// A turn as its records say it.
#[derive(Debug, Clone, PartialEq)]
pub struct Turned {
    pub began: u64,
    /// How and when it ended, with the harness's own line where it failed; none while it
    /// has not.
    pub ended: Option<(Execution, u64, Option<String>)>,
}

impl Tail {
    pub fn of(events: &[Event]) -> Self {
        let mut tail = Self::default();
        for event in events {
            tail.written = Some(tail.written.unwrap_or(0).max(written(event)));
            match event {
                Event::Model {
                    model, thinking, ..
                } => {
                    tail.model = Some(model.clone());
                    tail.thinking = thinking.clone();
                }
                Event::Tokens {
                    context_left: Some(left),
                    ..
                } => tail.context_left = Some(*left),
                Event::TurnBegan { at, .. } => {
                    tail.turn = Some(Turned {
                        began: *at,
                        ended: None,
                    });
                    tail.calls.clear();
                }
                Event::TurnEnded {
                    at, state, failure, ..
                } => {
                    let execution = match state {
                        TurnState::Finished => Execution::Finished,
                        TurnState::Failed => Execution::Failed,
                        TurnState::Interrupted => Execution::Interrupted,
                    };
                    let ended = Some((execution, *at, failure.as_ref().map(|f| f.line.clone())));
                    match &mut tail.turn {
                        Some(turn) if turn.ended.is_none() => turn.ended = ended,
                        _ => tail.turn = Some(Turned { began: *at, ended }),
                    }
                    tail.calls.clear();
                }
                Event::CallBegan { at, call } => match &call.parent {
                    None => tail.calls.push(LiveCall {
                        id: call.id.clone(),
                        tool: call.tool.clone(),
                        since: *at,
                        children: Vec::new(),
                        jobs: Vec::new(),
                    }),
                    Some(parent) => {
                        if let Some(running) = tail.calls.iter_mut().find(|c| c.id == *parent) {
                            running.children.push(LiveChild {
                                id: call.id.clone(),
                                tool: call.tool.clone(),
                            });
                        }
                    }
                },
                Event::CallEnded { at, id, .. } => {
                    if let Some(at_call) = tail.calls.iter().position(|call| call.id == *id) {
                        tail.calls.remove(at_call);
                        tail.ended.push(LiveEnded {
                            id: id.clone(),
                            at: *at,
                        });
                        if tail.ended.len() > ENDED {
                            tail.ended.remove(0);
                        }
                    } else {
                        for call in &mut tail.calls {
                            call.children.retain(|child| child.id != *id);
                        }
                    }
                }
                _ => {}
            }
        }
        tail
    }

    /// The turn running now, as the newest records say: when it began. None where the
    /// newest turn ended or none is read.
    pub fn running_since(&self) -> Option<u64> {
        self.turn
            .as_ref()
            .filter(|turn| turn.ended.is_none())
            .map(|turn| turn.began)
    }

    /// Where the rollout says its conversation stands, standing only while no process
    /// holds the rollout (`if_unheld`): a turn ended is how it ended; a turn begun and never
    /// ended with nothing holding the rollout was cut off, so it is `Interrupted` at the
    /// rollout's last record and vacant. Every live Codex is caught before this: the app's
    /// windows and a running app server answer in `idle` first, and a terminal or IDE
    /// session holds its rollout, which the runtime reads as held.
    pub fn sample(&self, now: u64) -> Option<Sample> {
        let turn = self.turn.as_ref()?;
        let (state, ended_at, error) = match &turn.ended {
            Some((state, at, error)) => (*state, Some(*at), error.clone()),
            None => (Execution::Interrupted, self.written, None),
        };
        Some(Sample {
            state,
            observed_at: now,
            started_at: Some(turn.began),
            ended_at,
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            error,
            vacant: true,
            if_unheld: true,
        })
    }

    /// A sample of what an owner says, `state`, carrying what the rollout says of the
    /// model, the thinking and the turn: when the running one began, or when the ended
    /// one began and ended where nothing runs now.
    pub fn owned(&self, state: Execution, now: u64) -> Sample {
        let (started_at, ended_at) = match (state, &self.turn) {
            (Execution::Running, Some(turn)) if turn.ended.is_none() => (Some(turn.began), None),
            (Execution::Running, _) => (None, None),
            (
                _,
                Some(Turned {
                    began,
                    ended: Some((_, at, _)),
                }),
            ) => (Some(*began), Some(*at)),
            _ => (None, None),
        };
        Sample {
            state,
            observed_at: now,
            started_at,
            ended_at,
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            error: None,
            vacant: false,
            if_unheld: false,
        }
    }
}

/// When a rollout record was written.
fn written(event: &Event) -> u64 {
    match event {
        Event::TurnBegan { at, .. }
        | Event::TurnEnded { at, .. }
        | Event::Words { at, .. }
        | Event::Rollback { at, .. }
        | Event::CallBegan { at, .. }
        | Event::CallEnded { at, .. }
        | Event::Tokens { at, .. }
        | Event::Model { at, .. }
        | Event::SubAgent { at, .. }
        | Event::Fork { at, .. }
        | Event::Compacted { at, .. } => *at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use steery_sdk::wire::harness::{Call, Count, Group};

    fn call(id: &str, parent: Option<&str>) -> Call {
        Call {
            id: id.into(),
            tool: "exec_command".into(),
            input: Value::Null,
            group: Group::Shell,
            server: None,
            steery: None,
            asks: None,
            parent: parent.map(str::to_owned),
            response: None,
        }
    }

    fn ended(at: u64, id: &str) -> Event {
        Event::CallEnded {
            at,
            id: id.into(),
            ok: true,
            chars: 0,
            tail: None,
            waited: Vec::new(),
        }
    }

    /// Where a rollout says its conversation stands, only while nothing holds the rollout:
    /// a turn begun and never ended was cut off (interrupted at the last record, vacant);
    /// an ended one is how it ended.
    #[test]
    fn a_rollout_s_newest_turn_says_where_it_stands() {
        let running = Tail::of(&[
            Event::TurnBegan { at: 5, turn: None },
            Event::Model {
                at: 8,
                model: "m".into(),
                provider: None,
                thinking: None,
            },
        ])
        .sample(9)
        .unwrap();
        assert_eq!(
            (
                running.state,
                running.started_at,
                running.ended_at,
                running.vacant,
                running.if_unheld,
                running.model.as_deref()
            ),
            (
                Execution::Interrupted,
                Some(5),
                Some(8),
                true,
                true,
                Some("m")
            )
        );
        let ended = Tail::of(&[
            Event::TurnBegan { at: 5, turn: None },
            Event::TurnEnded {
                at: 7,
                turn: None,
                state: TurnState::Interrupted,
                failure: None,
            },
        ])
        .sample(9)
        .unwrap();
        assert_eq!(
            (ended.state, ended.started_at, ended.ended_at, ended.vacant),
            (Execution::Interrupted, Some(5), Some(7), true)
        );
        assert!(Tail::of(&[]).sample(9).is_none());
    }

    /// The calls of the turn running are those begun and not answered, each sub-agent's
    /// own call under the one that started it; an answered one has ended when its answer
    /// was written; a new turn starts with none running.
    #[test]
    fn calls_run_until_answered() {
        let tail = Tail::of(&[
            Event::TurnBegan { at: 1, turn: None },
            Event::CallBegan {
                at: 2,
                call: call("stale", None),
            },
            Event::TurnEnded {
                at: 3,
                turn: None,
                state: TurnState::Interrupted,
                failure: None,
            },
            Event::TurnBegan { at: 4, turn: None },
            Event::CallBegan {
                at: 5,
                call: call("a", None),
            },
            Event::CallBegan {
                at: 6,
                call: call("b", None),
            },
            Event::CallBegan {
                at: 6,
                call: call("b.1", Some("b")),
            },
            ended(7, "a"),
            Event::Tokens {
                at: 8,
                count: Count::Total,
                response: None,
                model: None,
                input: 10,
                cached: 0,
                cache_write: None,
                output: 1,
                reasoning: 0,
                context_left: Some(0.25),
            },
        ]);
        assert_eq!(tail.running_since(), Some(4));
        assert_eq!(
            tail.calls
                .iter()
                .map(|c| (c.id.as_str(), c.since, c.children.len()))
                .collect::<Vec<_>>(),
            [("b", 6, 1)]
        );
        assert_eq!(
            tail.ended,
            [LiveEnded {
                id: "a".into(),
                at: 7
            }]
        );
        assert_eq!(tail.context_left, Some(0.25));
        let owned = tail.owned(Execution::Running, 9);
        assert_eq!(
            (owned.started_at, owned.ended_at, owned.vacant),
            (Some(4), None, false)
        );
        let done = Tail::of(&[
            Event::TurnBegan { at: 4, turn: None },
            Event::TurnEnded {
                at: 6,
                turn: None,
                state: TurnState::Finished,
                failure: None,
            },
        ])
        .owned(Execution::Finished, 9);
        assert_eq!((done.started_at, done.ended_at), (Some(4), Some(6)));
    }
}
