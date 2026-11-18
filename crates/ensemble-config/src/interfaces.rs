//! How a hub or a node may be managed: the interfaces its `interfaces` block
//! turns on. Left out, the command line is on; a block that turns on none is
//! refused, as nothing could manage it then.

use serde::Deserialize;

/// The interfaces a hub or a node turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interfaces {
    /// `ensemble hub …` or `ensemble node …` on its computer: run by the
    /// running process, over its socket, when it runs.
    pub cli: bool,
}

impl Default for Interfaces {
    fn default() -> Self {
        Interfaces { cli: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawInterfaces {
    pub(crate) cli: Option<RawCli>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCli {}

/// The interfaces `raw` turns on: the command line when it is left out.
pub(crate) fn interfaces(raw: Option<&RawInterfaces>, problems: &mut Vec<String>) -> Interfaces {
    let Some(raw) = raw else {
        return Interfaces::default();
    };
    let interfaces = Interfaces {
        cli: raw.cli.is_some(),
    };
    if !interfaces.cli {
        problems.push(
            "interfaces: turns on no interface, so nothing could manage it; add `cli: {}`".into(),
        );
    }
    interfaces
}
