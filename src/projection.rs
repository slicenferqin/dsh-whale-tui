//! Session projections (docs/04 section 3).
//!
//! DSH's canonical read model: plugins publish whole-value projections of
//! session state and the harness folds them over the event log. Reading these
//! is strictly better than re-deriving state from raw events — the projections
//! are replay-safe, carry persisted checkpoints, and the key table is
//! merge-extensible, so a client that reads them generically follows the
//! platform without code changes.
//!
//! Keys we model get a typed field. Keys we do not are still retained in
//! `extra`, so a projection a future plugin adds is visible in `/context`
//! instead of being silently dropped.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::transcript::{TodoItem, TodoStatus};

/// `goal` projection (dsh-goal). Phase drives the GoalBar's badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    /// Needed to match admitted-round events to this goal — a cleared and
    /// recreated goal restarts its round count.
    pub id: String,
    pub objective: String,
    pub phase: GoalPhase,
    /// Present exactly while `phase` is `Blocked`.
    pub blocked_reason: Option<String>,
    pub max_rounds: u64,
    /// Rounds as of the last goal *mutation*. This is a floor, not the live
    /// count: the projection folds `goal/change` only, so an admitted
    /// continuation round does not move it. Use
    /// `Transcript::goal_rounds` for the live figure.
    pub rounds_started: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalPhase {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }
}

/// `contextPressure` (dsh-token-meter). All three are optional until a provider
/// has reported usage — a UI must render an unknown state, not zeros.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextPressure {
    /// Provider-reported prompt size of the most recent request.
    pub pressure_tokens: Option<u64>,
    /// What the NEXT request's prompt would cost. Anchored on the provider
    /// sample and repriced only for the delta, so it reacts to compaction
    /// immediately — `pressure_tokens` cannot, since compaction reports no
    /// usage of its own. This is the figure to show as occupancy.
    pub projected_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

/// `contextBreakdown` (dsh-token-meter).
///
/// These are fixed-density estimates that systematically underprice CJK text
/// and JSON schemas. The upstream contract is explicit: present them as
/// composition, **never** summed into a total — the total is
/// `ContextPressure::projected_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub system_tokens: u64,
    pub tools_tokens: u64,
    pub message_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Projections {
    /// Seq of the newest projection change applied.
    pub seq: u64,
    /// `None` = the harness has published no todo list; `Some(vec![])` = an
    /// explicit empty list ("all done"). The distinction is meaningful.
    pub todos: Option<Vec<TodoItem>>,
    pub goal: Option<Goal>,
    pub context_pressure: Option<ContextPressure>,
    pub context_breakdown: Option<ContextBreakdown>,
    /// Projection keys we do not model yet, verbatim.
    pub extra: BTreeMap<String, Value>,
}

impl Projections {
    /// Fold one `session.projection` notification in. Unknown keys land in
    /// `extra`; a malformed known key is dropped rather than clobbering good
    /// state, so one bad payload cannot blank the UI.
    pub fn apply(&mut self, key: &str, value: &Value, seq: u64) {
        self.seq = self.seq.max(seq);
        match key {
            "todos" => {
                if value.is_null() {
                    self.todos = None;
                } else if let Some(items) = parse_todos(value) {
                    self.todos = Some(items);
                }
            }
            "goal" => {
                if value.is_null() {
                    self.goal = None;
                } else if let Some(goal) = parse_goal(value) {
                    self.goal = Some(goal);
                }
            }
            "contextPressure" => {
                if value.is_null() {
                    self.context_pressure = None;
                } else if let Some(obj) = value.as_object() {
                    self.context_pressure = Some(ContextPressure {
                        pressure_tokens: obj.get("pressureTokens").and_then(Value::as_u64),
                        projected_tokens: obj.get("projectedTokens").and_then(Value::as_u64),
                        context_window: obj.get("contextWindow").and_then(Value::as_u64),
                    });
                }
            }
            "contextBreakdown" => {
                if value.is_null() {
                    self.context_breakdown = None;
                } else if let Some(obj) = value.as_object() {
                    self.context_breakdown = Some(ContextBreakdown {
                        system_tokens: obj.get("systemTokens").and_then(Value::as_u64).unwrap_or(0),
                        tools_tokens: obj.get("toolsTokens").and_then(Value::as_u64).unwrap_or(0),
                        message_tokens: obj
                            .get("messageTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    });
                }
            }
            other => {
                self.extra.insert(other.to_string(), value.clone());
            }
        }
    }

    /// Occupancy as a 0.0–1.0 fraction, when both figures are known.
    pub fn context_fraction(&self) -> Option<f64> {
        let p = self.context_pressure?;
        let window = p.context_window?;
        if window == 0 {
            return None;
        }
        let used = p.projected_tokens.or(p.pressure_tokens)?;
        Some((used as f64 / window as f64).clamp(0.0, 1.0))
    }
}

/// `todos: TodoItem[] | null` — the authoritative shape from dsh-session.
/// `content` and a 3-value `status`; `tool-todo` runs with
/// `allowParallelInProgress: true`, so several rows may be in progress at once.
fn parse_todos(value: &Value) -> Option<Vec<TodoItem>> {
    let array = value.as_array()?;
    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        let text = entry.get("content").and_then(Value::as_str)?.trim();
        if text.is_empty() {
            continue;
        }
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .map(TodoStatus::parse_wire)
            .unwrap_or(TodoStatus::Pending);
        items.push(TodoItem {
            text: text.to_string(),
            status,
        });
    }
    Some(items)
}

fn parse_goal(value: &Value) -> Option<Goal> {
    let obj = value.as_object()?;
    // GoalProjection nests the snapshot under `goal` and keeps the replay
    // counters beside it.
    let snapshot = obj.get("goal").and_then(Value::as_object)?;
    let objective = snapshot.get("objective").and_then(Value::as_str)?.to_string();
    let phase = GoalPhase::parse(snapshot.get("phase").and_then(Value::as_str)?)?;
    Some(Goal {
        id: snapshot
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        objective,
        phase,
        blocked_reason: snapshot
            .get("blockedReason")
            .and_then(Value::as_object)
            .and_then(|r| r.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string),
        max_rounds: snapshot
            .get("maxGoalRounds")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        rounds_started: obj.get("roundsStarted").and_then(Value::as_u64).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn todos_projection_replaces_the_list_wholesale() {
        let mut p = Projections::default();
        assert!(p.todos.is_none(), "no projection yet is not an empty list");

        p.apply(
            "todos",
            &json!([
                {"content": "one", "status": "completed"},
                {"content": "two", "status": "in_progress"},
                {"content": "three", "status": "in_progress"}
            ]),
            7,
        );
        let todos = p.todos.as_ref().unwrap();
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].status, TodoStatus::Completed);
        // allowParallelInProgress: several rows may be in progress at once
        assert_eq!(
            todos
                .iter()
                .filter(|t| t.status == TodoStatus::InProgress)
                .count(),
            2
        );
        assert_eq!(p.seq, 7);

        // an explicit empty list is a real snapshot, distinct from absent
        p.apply("todos", &json!([]), 8);
        assert_eq!(p.todos.as_deref(), Some(&[][..]));
        // and null clears it back to absent
        p.apply("todos", &Value::Null, 9);
        assert!(p.todos.is_none());
    }

    #[test]
    fn goal_projection_maps_phase_and_rounds() {
        let mut p = Projections::default();
        p.apply(
            "goal",
            &json!({
                "goal": {
                    "id": "g1", "revision": 3,
                    "objective": "ship the TUI",
                    "phase": "blocked",
                    "blockedReason": {"code": "needs-input", "message": "waiting on approval"},
                    "maxGoalRounds": 10
                },
                "roundsStarted": 4,
                "createdAt": 1, "updatedAt": 2
            }),
            11,
        );
        let goal = p.goal.as_ref().unwrap();
        assert_eq!(goal.objective, "ship the TUI");
        assert_eq!(goal.phase, GoalPhase::Blocked);
        assert_eq!(goal.blocked_reason.as_deref(), Some("waiting on approval"));
        assert_eq!((goal.rounds_started, goal.max_rounds), (4, 10));

        p.apply("goal", &Value::Null, 12);
        assert!(p.goal.is_none(), "clear tombstone removes the goal");
    }

    #[test]
    fn context_pressure_keeps_absent_figures_absent() {
        let mut p = Projections::default();
        // before any provider usage every figure is optional
        p.apply("contextPressure", &json!({}), 1);
        let cp = p.context_pressure.unwrap();
        assert_eq!(cp.projected_tokens, None);
        assert_eq!(p.context_fraction(), None, "no fraction without a window");

        p.apply(
            "contextPressure",
            &json!({"pressureTokens": 1000, "projectedTokens": 1500, "contextWindow": 10000}),
            2,
        );
        assert_eq!(p.context_fraction(), Some(0.15));
    }

    #[test]
    fn context_fraction_prefers_projected_and_falls_back_to_pressure() {
        let mut p = Projections::default();
        p.apply(
            "contextPressure",
            &json!({"pressureTokens": 4000, "contextWindow": 8000}),
            1,
        );
        assert_eq!(p.context_fraction(), Some(0.5), "falls back to pressure");
        p.apply(
            "contextPressure",
            &json!({"pressureTokens": 4000, "projectedTokens": 2000, "contextWindow": 8000}),
            2,
        );
        assert_eq!(
            p.context_fraction(),
            Some(0.25),
            "projected wins, and it can drop after a compaction"
        );
    }

    #[test]
    fn unmodelled_keys_are_retained_rather_than_dropped() {
        let mut p = Projections::default();
        p.apply("somethingNew", &json!({"a": 1}), 5);
        assert_eq!(p.extra.get("somethingNew"), Some(&json!({"a": 1})));
        assert_eq!(p.seq, 5);
    }

    #[test]
    fn a_malformed_known_key_does_not_clobber_good_state() {
        let mut p = Projections::default();
        p.apply("todos", &json!([{"content": "keep me", "status": "pending"}]), 1);
        // not an array, and a goal missing its phase
        p.apply("todos", &json!({"nope": true}), 2);
        p.apply("goal", &json!({"goal": {"objective": "x"}}), 3);
        assert_eq!(p.todos.as_ref().unwrap().len(), 1, "previous list survives");
        assert!(p.goal.is_none(), "half-formed goal is rejected, not stored");
    }

    #[test]
    fn an_attach_snapshot_of_a_fresh_session_clears_rather_than_populates() {
        // The bridge replays `snapshot().values`, which carries EVERY registered
        // key — a domain that has produced nothing yet reports its init view,
        // and for goal/todos that is null. A fresh session must therefore end up
        // empty, not half-populated with defaults.
        let mut p = Projections::default();
        p.apply("todos", &json!([{"content": "stale", "status": "pending"}]), 5);
        p.apply(
            "goal",
            &json!({"goal":{"id":"g","revision":1,"objective":"old","phase":"active","maxGoalRounds":3},
                    "roundsStarted":1,"createdAt":1,"updatedAt":1}),
            5,
        );

        // now a fresh-session snapshot arrives (asOfSeq -1 lands as seq 0)
        for key in ["todos", "goal"] {
            p.apply(key, &Value::Null, 0);
        }
        assert!(p.todos.is_none());
        assert!(p.goal.is_none());
        assert_eq!(p.seq, 5, "seq is a high-water mark, not the latest arrival");
    }

    #[test]
    fn seq_never_goes_backwards() {
        let mut p = Projections::default();
        p.apply("todos", &json!([]), 9);
        p.apply("todos", &json!([]), 4);
        assert_eq!(p.seq, 9);
    }
}
