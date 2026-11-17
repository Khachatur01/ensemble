//! Plans: a piece of work too big for one agent, broken down into the tasks
//! agents do. An issue is a person's, such as "a panorama video app with
//! rotating cameras"; a task is one agent's, with one goal and one check. A
//! plan is the bridge: its **steps**, which agent does each, what each comes
//! after, and how each is checked.
//!
//! The person's own agent drafts a plan with them, in conversation (the
//! `plan` tool), and the person approves it in a terminal (`ensemble plan
//! approve`): nothing of it runs before. Approving makes each step a task on
//! the hub, `after` the tasks of the steps it comes after, so the hub releases
//! each as the ones before it are done. A plan changed later is a new
//! revision, approved again: the tasks of steps that did not change are kept,
//! and those of steps that changed or went, if they have not run, are
//! cancelled. Agents never add steps on their own.

use crate::tasks::{Status, Task, Ticket, entry_line, valid_id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One step of a plan: its key, a short name the plan's other steps use in
/// their `after`, and the ticket its task is made from. The ticket's `after`
/// names steps of the plan, not tasks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Step {
    pub key: String,
    pub ticket: Ticket,
}

/// A plan as it is proposed: what it is for, and its steps, in order.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Draft {
    pub title: String,
    /// The work as the person wants it, in their words.
    pub goal: String,
    /// The tracker issue it breaks down, if any.
    pub issue: Option<String>,
    pub steps: Vec<Step>,
}

impl Draft {
    /// What is wrong with it as a plan, if anything: what the hub refuses
    /// before it keeps one.
    pub fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() || self.title.contains('\n') {
            return Err("the plan's title is one non-empty line".into());
        }
        if self.goal.trim().is_empty() {
            return Err("the plan has no goal: say what the work is for".into());
        }
        if self.steps.is_empty() {
            return Err("the plan has no steps".into());
        }
        let mut seen: Vec<&str> = Vec::new();
        for step in &self.steps {
            let key = step.key.as_str();
            if !valid_id(key) {
                return Err(format!(
                    "step '{key}': a key is one word of letters, digits, '-' or '_'"
                ));
            }
            if seen.contains(&key) {
                return Err(format!("step '{key}' is named twice"));
            }
            if let Some(dep) = step
                .ticket
                .after
                .iter()
                .find(|a| !seen.contains(&a.as_str()))
            {
                return Err(format!(
                    "step '{key}' comes after '{dep}', which is not a step before it"
                ));
            }
            Ticket {
                after: Vec::new(),
                ..step.ticket.clone()
            }
            .check()
            .map_err(|e| format!("step '{key}': {e}"))?;
            seen.push(key);
        }
        Ok(())
    }
}

/// Where a plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    /// Proposed, waiting for the person to approve it: nothing of it runs.
    Draft,
    /// Approved: its steps are tasks.
    Approved,
    /// Cancelled by the person: its unfinished tasks were cancelled.
    Cancelled,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Draft => "draft",
            PlanStatus::Approved => "approved",
            PlanStatus::Cancelled => "cancelled",
        }
    }
}

/// A plan as the hub keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Plan {
    /// Given by the hub: `plan-0001-<title>`.
    pub id: String,
    pub title: String,
    pub goal: String,
    pub issue: Option<String>,
    /// Its steps, as last proposed.
    pub steps: Vec<Step>,
    pub status: PlanStatus,
    /// 1 for the first proposal, one more for each change.
    pub revision: u32,
    /// Each step's task, by key, once approved.
    pub tasks: BTreeMap<String, String>,
    /// Each step as it was when its task was made, so a revision can tell
    /// what changed.
    pub made: BTreeMap<String, Step>,
}

impl Plan {
    /// Its tasks, as `all` has them, in its steps' order.
    pub fn its_tasks<'a>(&self, all: &'a [Task]) -> Vec<&'a Task> {
        self.steps
            .iter()
            .filter_map(|s| self.tasks.get(&s.key))
            .filter_map(|id| all.iter().find(|t| t.id() == id))
            .collect()
    }

    /// One line for it: its id, where it stands, and its title.
    pub fn line(&self, all: &[Task]) -> String {
        let tasks = self.its_tasks(all);
        let state = match self.status {
            PlanStatus::Approved => {
                let done = tasks
                    .iter()
                    .filter(|t| t.status == Some(Status::Done))
                    .count();
                let stuck = tasks
                    .iter()
                    .filter(|t| matches!(t.status, Some(Status::Failed | Status::Cancelled)))
                    .count();
                if done == self.steps.len() {
                    "done".to_string()
                } else if stuck > 0 {
                    format!(
                        "approved, {done} of {} done, {stuck} failed or cancelled",
                        self.steps.len()
                    )
                } else {
                    format!("approved, {done} of {} done", self.steps.len())
                }
            }
            PlanStatus::Draft if self.revision > 1 => {
                format!("revision {}, waiting for approval", self.revision)
            }
            PlanStatus::Draft => "waiting for approval".to_string(),
            PlanStatus::Cancelled => "cancelled".to_string(),
        };
        format!("{} · {state} · {}", self.id, self.title)
    }

    /// The whole of it: the line, its goal and issue, and a line per step,
    /// with its task once it has one.
    pub fn describe(&self, all: &[Task]) -> String {
        let mut out = self.line(all);
        out.push_str(&format!("\n  goal: {}", self.goal.trim()));
        if let Some(issue) = &self.issue {
            out.push_str(&format!("\n  issue: {issue}"));
        }
        for (n, step) in self.steps.iter().enumerate() {
            let t = &step.ticket;
            let agent = if t.agent.is_empty() {
                "backlog"
            } else {
                &t.agent
            };
            let mut line = format!("\n  {}. {} ({agent}): {}", n + 1, step.key, t.title);
            if !t.after.is_empty() {
                line.push_str(&format!(", after {}", t.after.join(", ")));
            }
            if let Some(gate) = &t.gate {
                line.push_str(&format!(
                    ", gate {} ({} attempts)",
                    gate.name, gate.attempts
                ));
            }
            out.push_str(&line);
            let task = self
                .tasks
                .get(&step.key)
                .filter(|_| self.made.get(&step.key) == Some(step))
                .and_then(|id| all.iter().find(|t| t.id() == id));
            if let Some(task) = task {
                out.push_str(&format!("\n     {}", entry_line(task, all, None)));
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::tests::draft;

    pub(crate) fn step(key: &str, agent: &str, after: &[&str]) -> Step {
        Step {
            key: key.into(),
            ticket: Ticket {
                after: after.iter().map(|a| a.to_string()).collect(),
                ..draft(&format!("Do {key}"), agent, &format!("{key} it"))
            },
        }
    }

    pub(crate) fn plan(steps: Vec<Step>) -> Draft {
        Draft {
            title: "Panorama rig".into(),
            goal: "A video app with rotating cameras.".into(),
            issue: Some("7".into()),
            steps,
        }
    }

    #[test]
    fn a_plan_names_each_step_once_and_after_only_steps_before_it() {
        assert!(
            plan(vec![
                step("rig", "web/coder", &[]),
                step("calib", "web/coder", &["rig"])
            ])
            .check()
            .is_ok()
        );
        for (steps, said) in [
            (vec![], "no steps"),
            (vec![step("a b", "x", &[])], "one word"),
            (
                vec![step("a", "x", &[]), step("a", "x", &[])],
                "named twice",
            ),
            (
                vec![step("a", "x", &["b"]), step("b", "x", &[])],
                "not a step before it",
            ),
            (vec![step("a", "x", &["a"])], "not a step before it"),
        ] {
            let err = plan(steps).check().unwrap_err();
            assert!(err.contains(said), "{err}");
        }
        let mut no_goal = plan(vec![step("a", "x", &[])]);
        no_goal.goal = " ".into();
        assert!(no_goal.check().unwrap_err().contains("no goal"));
        let mut bad = plan(vec![step("a", "x", &[])]);
        bad.steps[0].ticket.text.clear();
        assert!(bad.check().unwrap_err().contains("step 'a'"));
    }
}
