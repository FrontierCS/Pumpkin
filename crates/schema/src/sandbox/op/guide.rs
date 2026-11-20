//! Shell usage for the grants the session owner has made public to this role.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::{Grant, OpsReply, SessionGrant};
use crate::ir::{Argument, Name};

impl OpsReply {
    /// Shell instructions for the role, built only from its public ops reply.
    pub fn guide(&self) -> String {
        let mut text = format!(
            "Sandbox commands for role {}\nWork time upper bound: {} seconds. \
             Run `sandboxctl ops` for current limits.\n",
            self.role, self.remaining_seconds
        );
        for grant in &self.grants {
            text.push('\n');
            match grant {
                Grant::Job(job) => {
                    usage(&mut text, &job.name, &job.arguments, true);
                    description(&mut text, &job.help);
                    let _ = write!(
                        text,
                        "Payload limit: {} bytes. Timeout: {} seconds per call.",
                        job.payload_max_bytes, job.call_timeout_seconds
                    );
                    if let Some(calls) = job.calls {
                        let _ = write!(text, " Call limit: {calls}.");
                    }
                    if let Some(seconds) = job.cumulative_seconds {
                        let _ = write!(text, " Total call time limit: {seconds} seconds.");
                    }
                    text.push('\n');
                }
                Grant::Now(now) => {
                    usage(&mut text, &now.name, &now.arguments, false);
                    description(&mut text, &now.help);
                    let _ = writeln!(
                        text,
                        "Payload limit: {} bytes. Returns immediately.",
                        now.payload_max_bytes
                    );
                }
                Grant::Session(session) => session_usage(&mut text, session),
            }
        }
        if self
            .grants
            .iter()
            .any(|grant| !matches!(grant, Grant::Session(_)))
        {
            text.push_str("\nFILE is a regular file containing the payload. Omit FILE for an empty payload.\n");
        }
        if self
            .grants
            .iter()
            .any(|grant| matches!(grant, Grant::Job(_)))
        {
            text.push_str(
                "Use --wait SECONDS to wait for the job. A completed job has a returned field. Without --wait, \
                follow the job number with sandboxctl status JOB.\n",
            );
        }
        text
    }
}

fn session_usage(text: &mut String, session: &SessionGrant) {
    let name = &session.name;
    let _ = writeln!(text, "sandboxctl call {name}.open [--seconds SECONDS]");
    let _ = writeln!(
        text,
        "sandboxctl call {name}.run LEASE [--timeout SECONDS] -- COMMAND [ARG ...]"
    );
    let _ = writeln!(
        text,
        "sandboxctl call {name}.put LEASE LOCAL REMOTE [--replace]"
    );
    let _ = writeln!(text, "sandboxctl call {name}.get LEASE REMOTE LOCAL");
    let _ = writeln!(
        text,
        "sandboxctl call {name}.status LEASE\nsandboxctl call {name}.close LEASE"
    );
    description(text, &session.help);
    let _ = writeln!(
        text,
        "Limits: {} simultaneous leases, {} seconds per lease, {} bytes per transfer.",
        session.instances, session.instance_seconds, session.transfer_bytes
    );
    if let Some(limit) = session.commands {
        let _ = writeln!(
            text,
            "Command limit: {limit} (each transfer counts as one)."
        );
    }
    if let Some(seconds) = session.cumulative_seconds {
        let _ = writeln!(text, "Total lease time limit: {seconds} seconds.");
    }
}

fn description(text: &mut String, help: &str) {
    if !help.is_empty() {
        let _ = writeln!(text, "{help}");
    }
}

fn usage(text: &mut String, name: &Name, arguments: &BTreeMap<Name, Argument>, job: bool) {
    let _ = write!(text, "sandboxctl call {name} [FILE]");
    let mut defaults = String::new();
    for (name, arg) in arguments {
        let (kind, default) = match arg {
            Argument::Str { default } => ("STR", default.as_ref().map(|s| format!("{s:?}"))),
            Argument::Int { default } => ("INT", default.map(|v| v.to_string())),
            Argument::Bool { default } => ("BOOL", default.map(|v| v.to_string())),
        };
        if let Some(default) = default {
            let _ = write!(text, " [--arg {name}={kind}]");
            let _ = writeln!(defaults, "Default for {name}: {default}.");
        } else {
            let _ = write!(text, " --arg {name}={kind}");
        }
    }
    if job {
        text.push_str(" [--wait SECONDS]");
    }
    text.push('\n');
    text.push_str(&defaults);
}
