//! The contributor tools that xtask runs, each pinned to one version.

use crate::{Cargo, Result};

/// A tool that cargo installs and runs as a subcommand.
#[derive(Clone, Copy)]
pub(crate) struct Tool {
    /// The crate, `cargo-<subcommand>`, which also begins the line `--version` prints.
    binary: &'static str,
    version: &'static str,
}

pub(crate) const NEXTEST: Tool = Tool {
    binary: "cargo-nextest",
    version: "0.9.151",
};

pub(crate) const MUTANTS: Tool = Tool {
    binary: "cargo-mutants",
    version: "27.1.0",
};

const TOOLS: [Tool; 2] = [NEXTEST, MUTANTS];

/// Installs each of [`TOOLS`] that cargo doesn't run at its version, into cargo's install root.
pub(crate) fn install(cargo: &Cargo) -> Result<()> {
    for tool in TOOLS {
        let Tool { binary, version } = tool;
        if installed(cargo, tool).as_deref() == Some(version) {
            println!("xtask: {binary} {version} is installed");
            continue;
        }
        cargo
            .cmd(&["install", "--locked", &format!("{binary}@{version}")])
            .run()?;
    }
    Ok(())
}

/// Fails unless cargo runs `tool` at its version, saying how to install it.
pub(crate) fn require(cargo: &Cargo, tool: Tool) -> Result<()> {
    let Tool { binary, version } = tool;
    let wrong = match installed(cargo, tool) {
        Some(found) if found == version => return Ok(()),
        Some(found) => format!("{binary} is at {found}, not at {version}"),
        None => format!("{binary} {version} isn't installed"),
    };
    Err(format!("{wrong}: install it with `cargo xtask install-tools`").into())
}

/// The version of `tool` that cargo runs, as its `--version` prints it: `None` if cargo runs none,
/// or it prints no version.
fn installed(cargo: &Cargo, tool: Tool) -> Option<String> {
    let subcommand = tool.binary.strip_prefix("cargo-")?;
    let printed = cargo
        .cmd(&[subcommand, "--version"])
        .ignore_stderr()
        .read()
        .ok()?;
    match printed.split_whitespace().collect::<Vec<_>>()[..] {
        [binary, version, ..] if binary == tool.binary => Some(version.to_owned()),
        _ => None,
    }
}
