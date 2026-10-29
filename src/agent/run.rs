//! The agent loop.

use super::sandbox::Sandbox;
use super::tools;
use crate::config::Assignment;
use crate::port::{Error, Message, ModelPort, Request, Usage};
use std::path::Path;

const PREAMBLE: &str = "\
You are an autonomous agent working inside a project directory. Use your tools \
to look at the real files before you answer or change anything; paths are \
relative to the project root. Work until the task is done, then reply with a \
short plain-text report of what you found or changed. Do not ask the user \
questions: make reasonable assumptions and state them.";

const OUT_OF_STEPS: &str = "\
You have used all your steps. Do not call any more tools. Reply now with what \
you did, what is still unfinished, and what you would do next.";

/// What one agent run produced.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub text: String,
    pub model: String,
    /// Summed over every model call in the run.
    pub usage: Usage,
    /// Model calls made, the final answer included.
    pub steps: u32,
    /// Tool calls executed, whether or not they succeeded.
    pub tool_calls: u32,
}

pub fn run(
    port: &dyn ModelPort,
    assignment: &Assignment,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
) -> Result<AgentRun, Error> {
    let sandbox = Sandbox::new(workdir).map_err(|detail| Error::Unsupported {
        backend: port.name().to_string(),
        detail: format!(
            "role '{}' cannot start its agent: {detail}",
            assignment.role
        ),
    })?;
    let defs = tools::definitions(&assignment.tools);

    let mut system = PREAMBLE.to_string();
    if let Some(p) = assignment
        .prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        system.push_str("\n\n");
        system.push_str(p);
    }
    let mut messages = vec![Message::system(system)];
    if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
        messages.push(Message::user(format!("Context:\n{ctx}")));
    }
    messages.push(Message::user(task.to_string()));

    let mut out = AgentRun {
        text: String::new(),
        model: assignment.model.clone(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
    };

    let request = |messages: &[Message], offer_tools: bool| {
        let mut r = Request::new(assignment.model.clone(), messages.to_vec());
        r.temperature = assignment.temperature;
        r.workdir = Some(sandbox.root().display().to_string());
        if offer_tools {
            r.tools = defs.clone();
        }
        r
    };

    for _ in 0..assignment.max_steps {
        let response = port.complete(&request(&messages, true))?;
        out.steps += 1;
        out.usage.prompt_tokens += response.usage.prompt_tokens;
        out.usage.completion_tokens += response.usage.completion_tokens;
        out.model = response.model;

        if response.tool_calls.is_empty() {
            out.text = response.text;
            return Ok(out);
        }

        messages.push(Message::assistant_calling(
            response.text,
            response.tool_calls.clone(),
        ));
        for call in &response.tool_calls {
            out.tool_calls += 1;
            let result = tools::execute(&sandbox, &assignment.tools, &call.name, &call.arguments)
                .unwrap_or_else(|e| format!("error: {e}"));
            messages.push(Message::tool_result(call.id.clone(), result));
        }
    }

    // Out of steps. One last call with no tools on offer, so the work done so
    // far is reported instead of discarded.
    messages.push(Message::user(OUT_OF_STEPS));
    let response = port.complete(&request(&messages, false))?;
    out.steps += 1;
    out.usage.prompt_tokens += response.usage.prompt_tokens;
    out.usage.completion_tokens += response.usage.completion_tokens;
    out.model = response.model;
    out.text = format!(
        "[stopped at the {}-step limit]\n{}",
        assignment.max_steps, response.text
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::sandbox::testing::TempDir;
    use super::*;
    use crate::config::AgentTool;
    use crate::port::Role;
    use crate::port::testing::{FakePort, Scripted, call};

    fn assignment(tools: Vec<AgentTool>, max_steps: u32) -> Assignment {
        Assignment {
            role: "coder".into(),
            backend: "b".into(),
            model: "m".into(),
            prompt: Some("Be careful.".into()),
            temperature: Some(0.1),
            tools,
            max_steps,
        }
    }

    fn project() -> TempDir {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("a.txt"), "old\n").unwrap();
        tmp
    }

    fn go(port: &FakePort, a: &Assignment, dir: &Path) -> Result<AgentRun, Error> {
        run(port, a, "do it", None, dir)
    }

    #[test]
    fn a_model_that_asks_for_nothing_just_answers() {
        let tmp = project();
        let port = FakePort::ok("b", "done");
        let r = go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        assert_eq!((r.text.as_str(), r.steps, r.tool_calls), ("done", 1, 0));
        let req = port.last().unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "read_file");
        assert_eq!(req.temperature, Some(0.1));
        let system = &req.messages[0];
        assert_eq!(system.role, Role::System);
        assert!(system.content.contains("autonomous agent"));
        assert!(system.content.ends_with("Be careful."));
    }

    #[test]
    fn tool_calls_run_and_their_results_go_back_to_the_model() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "unused",
            vec![
                Scripted::call("c1", "read_file", r#"{"path":"a.txt"}"#),
                Scripted::call(
                    "c2",
                    "edit_file",
                    r#"{"path":"a.txt","old_string":"old","new_string":"new"}"#,
                ),
                Scripted::text("changed a.txt"),
            ],
        );
        let a = assignment(vec![AgentTool::Read, AgentTool::Edit], 5);
        let r = go(&port, &a, &tmp.0).unwrap();
        assert_eq!(r.text, "changed a.txt");
        assert_eq!((r.steps, r.tool_calls), (3, 2));
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (3, 6));
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("a.txt")).unwrap(),
            "new\n"
        );

        let third = port.nth(2).unwrap();
        let results: Vec<_> = third
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| (m.tool_call_id.clone().unwrap(), m.content.clone()))
            .collect();
        assert_eq!(
            results,
            vec![
                ("c1".to_string(), "old".to_string()),
                ("c2".to_string(), "edited a.txt".to_string())
            ]
        );
    }

    #[test]
    fn a_failing_tool_call_is_reported_to_the_model_not_raised() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "recovered",
            vec![
                Scripted::call("c1", "read_file", r#"{"path":"../secret"}"#),
                Scripted::calls(vec![call("c2", "bash", "{}")]),
            ],
        );
        let r = go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        assert_eq!(r.text, "recovered");
        let last = port.last().unwrap();
        let errors: Vec<_> = last
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            errors[0].starts_with("error: ") && errors[0].contains("'..'"),
            "{errors:?}"
        );
        assert!(errors[1].contains("no tool 'bash'"), "{errors:?}");
    }

    #[test]
    fn the_step_limit_ends_with_a_tool_free_summary_call() {
        let tmp = project();
        let looping = (0..2)
            .map(|i| Scripted::call(&format!("c{i}"), "glob", r#"{"pattern":"*"}"#))
            .chain([Scripted::text("so far: nothing")])
            .collect();
        let port = FakePort::scripted("b", "x", looping);
        let r = go(&port, &assignment(vec![AgentTool::Glob], 2), &tmp.0).unwrap();
        assert!(
            r.text.starts_with("[stopped at the 2-step limit]"),
            "{}",
            r.text
        );
        assert!(r.text.ends_with("so far: nothing"));
        assert_eq!(r.steps, 3);
        let last = port.last().unwrap();
        assert!(last.tools.is_empty(), "no tools on the closing call");
    }

    #[test]
    fn a_missing_working_directory_is_an_error_naming_the_role() {
        let port = FakePort::ok("b", "x");
        let err = go(
            &port,
            &assignment(vec![AgentTool::Read], 2),
            Path::new("/no/such/dir"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("role 'coder'"), "{err}");
    }
}
