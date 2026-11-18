//! `ensemble knowledge …`: your node's hub's knowledge, by hand, as an agent's
//! `share` and `recall` tools use it, named for the folder you work in.

use crate::cli::KnowledgeCommand;
use crate::{Ctx, knowledge_for};
use ensemble::knowledge::KnowledgeAccess;
use std::path::Path;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: KnowledgeCommand) -> Result<ExitCode, String> {
    match command {
        KnowledgeCommand::Share {
            title,
            text,
            visibility,
        } => share(&ctx.dir, &title, text, &visibility),
        KnowledgeCommand::Recall { topic } => {
            let knowledge = knowledge_for(&ctx.dir, "knowledge recall")?;
            let notes = ensemble::runtime::block_on(knowledge.call(
                ensemble::knowledge::RECALL,
                &serde_json::json!({ "query": topic.join(" ") }),
            ))
            .expect("recall is a knowledge tool")?;
            println!("{notes}");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `ensemble knowledge share`: keep knowledge in the vault of this node's hub
/// by hand, as the `share` tool does for an agent.
fn share(
    dir: &Path,
    title: &str,
    text: Option<String>,
    visibility: &str,
) -> Result<ExitCode, String> {
    let knowledge = knowledge_for(dir, "knowledge share")?;
    let text = match text {
        Some(text) => text,
        None => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| format!("knowledge share: cannot read the text from stdin: {e}"))?;
            text
        }
    };
    if text.trim().is_empty() {
        return Err("knowledge share: the text is empty; give it with --text or on stdin".into());
    }
    knowledge.set_client("ensemble-cli");
    let said = ensemble::runtime::block_on(knowledge.call(
        ensemble::knowledge::SHARE,
        &serde_json::json!({ "title": title, "text": text, "visibility": visibility }),
    ))
    .expect("share is a knowledge tool")?;
    println!("{said}");
    Ok(ExitCode::SUCCESS)
}
