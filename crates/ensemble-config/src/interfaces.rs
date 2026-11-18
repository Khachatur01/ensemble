//! How a hub or a runner is reached: the interfaces its `interfaces` block
//! turns on. Left out, the command line is on; a block that turns on nothing
//! that manages it is refused, as nothing could manage it then.

use serde::Deserialize;

/// The interfaces a hub or a runner turns on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interfaces {
    /// `ensemble-hub …` or `ensemble-runner …` on its computer: run by the
    /// running process, over its socket, when it runs.
    pub cli: bool,
    /// The hub's MCP over HTTP, for client agents. A hub's alone.
    pub mcp: Option<Mcp>,
}

/// The hub's MCP over HTTP: `/mcp` on the hub's port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mcp {
    /// The address clients reach the hub at, such as `https://hub.example.com`,
    /// with no path and no trailing slash: what a new token's answer tells
    /// them to add to their agent.
    pub url: String,
}

impl Mcp {
    /// The MCP endpoint clients add.
    pub fn endpoint(&self) -> String {
        format!("{}/mcp", self.url)
    }
}

impl Default for Interfaces {
    fn default() -> Self {
        Interfaces {
            cli: true,
            mcp: None,
        }
    }
}

/// A hub's `interfaces` block.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawInterfaces {
    pub(crate) cli: Option<RawCli>,
    pub(crate) mcp: Option<RawMcp>,
}

/// A runner's `interfaces` block: it serves no one, so the command line alone.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRunnerInterfaces {
    pub(crate) cli: Option<RawCli>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCli {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMcp {
    pub(crate) url: String,
}

/// The interfaces a hub's `raw` block turns on: the command line when it is
/// left out.
pub(crate) fn interfaces(raw: Option<&RawInterfaces>, problems: &mut Vec<String>) -> Interfaces {
    let Some(raw) = raw else {
        return Interfaces::default();
    };
    let mcp = raw.mcp.as_ref().and_then(|m| match mcp_url(&m.url) {
        Ok(url) => Some(Mcp { url }),
        Err(e) => {
            problems.push(format!("interfaces.mcp.url: {e}"));
            None
        }
    });
    managed(
        Interfaces {
            cli: raw.cli.is_some(),
            mcp,
        },
        problems,
    )
}

/// The interfaces a runner's `raw` block turns on.
pub(crate) fn runner_interfaces(
    raw: Option<&RawRunnerInterfaces>,
    problems: &mut Vec<String>,
) -> Interfaces {
    let Some(raw) = raw else {
        return Interfaces::default();
    };
    managed(
        Interfaces {
            cli: raw.cli.is_some(),
            mcp: None,
        },
        problems,
    )
}

/// `interfaces`, once it is known that something manages it.
fn managed(interfaces: Interfaces, problems: &mut Vec<String>) -> Interfaces {
    if !interfaces.cli {
        problems.push(
            "interfaces: turns on nothing that manages it, so nothing could; add `cli: {}`".into(),
        );
    }
    interfaces
}

/// `url` as the address clients reach the hub at: http or https, with a
/// host, and no path, query or trailing slash.
fn mcp_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("'{url}' must start with https:// or http://"))?;
    if rest.is_empty() || rest.contains(['/', '?', '#', ' ']) {
        return Err(format!(
            "'{url}' must be the hub's address alone, such as https://hub.example.com"
        ));
    }
    Ok(url.to_string())
}
