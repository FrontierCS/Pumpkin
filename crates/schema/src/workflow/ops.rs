//! The role's public grants and limits, at startup and as its caller sees them during work.

use std::num::NonZeroU64;

use super::errors::Errors;
use super::{ParseErrors, RoleId, Session, SessionId, Workflow};
use crate::frame;
use crate::sandbox::ToSandbox;
use crate::sandbox::op::{self, Grant, LeaseRemaining, OpReply, OpsReply};

impl Workflow {
    pub(super) fn check_ops(&self) -> Result<(), ParseErrors> {
        let mut errors = Errors::default();
        for (role, sandbox) in self.roles() {
            let mut ops = self.initial_ops(role);
            if ops.grants.is_empty() {
                continue;
            }
            // Remaining lease quotas only decrease. The op number and time take their widest
            // forms so that every later reply, with its JSON escapes and envelope, fits a frame.
            ops.remaining_seconds = u64::MAX;
            let message = ToSandbox::OpReply {
                op: u32::MAX,
                reply: OpReply::Ops(ops),
            };
            if let Err(error) = frame::write(&mut std::io::sink(), &message, &[]) {
                errors.report(sandbox.at.clone(), format!("the role's ops reply cannot be sent: {error}: shorten its public grant interface"));
            }
        }
        errors.finish(Ok(()))
    }

    pub(super) fn initial_ops(&self, role: RoleId) -> OpsReply {
        self.ops(role, self.work_deadline().as_secs(), |_, grant| {
            LeaseRemaining {
                instances: grant.instances.get(),
                instance_seconds: grant.instance_seconds.get(),
                cumulative_seconds: grant.cumulative_seconds.map(NonZeroU64::get),
                commands: grant.commands.map(NonZeroU64::get),
            }
        })
    }

    /// The calling role's public interface, with its current time and session quotas.
    pub fn ops(
        &self,
        role: RoleId,
        remaining_seconds: u64,
        remaining: impl Fn(SessionId, &Session) -> LeaseRemaining,
    ) -> OpsReply {
        let jobs = self.job_functions_of(role).map(|(_, function)| {
            Grant::Job(op::JobFunction {
                help: function.help.clone(),
                name: function.name.clone(),
                payload_max_bytes: function.payload_max_bytes(),
                calls: function.calls,
                cumulative_seconds: function.cumulative_seconds,
                call_timeout_seconds: function.call_timeout_seconds,
                arguments: function.body.arguments.clone(),
            })
        });
        let leases = (self.sessions()).filter(|(_, grant)| grant.holder.role() == role);
        let leases = leases.map(|(id, grant)| {
            let Session {
                name,
                help,
                holder: _,
                runner,
                instances,
                instance_seconds,
                cumulative_seconds,
                commands,
                transfer_bytes,
            } = grant;
            Grant::Session(op::SessionGrant {
                name: name.clone(),
                help: help.clone(),
                instances: *instances,
                instance_seconds: *instance_seconds,
                cumulative_seconds: *cumulative_seconds,
                commands: *commands,
                transfer_bytes: *transfer_bytes,
                companions: (self.runner(*runner).companions.iter())
                    .map(|companion| companion.name.clone())
                    .collect(),
                remaining: remaining(id, grant),
            })
        });
        let now = self.now_functions_of(role).map(|(_, now)| {
            Grant::Now(op::NowFunction {
                help: now.help.clone(),
                name: now.name.clone(),
                payload_max_bytes: now.payload_max_bytes,
                arguments: now.body.arguments.clone(),
            })
        });
        OpsReply {
            role: self.sandbox(role).name.clone(),
            remaining_seconds,
            resources: (self.sandbox(role).needs).resources(),
            grants: (jobs.chain(leases)).chain(now).collect(),
        }
    }
}
