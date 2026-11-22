//! The simulation of each principal's policy, as `pumpkin-admin policy` prints it, with the
//! deployment's values in place of its placeholders, each statement of which IAM must allow
//! ([`allowed`]).
// `iam:SimulatePrincipalPolicy` is a read, of the principal's policies:
// https://github.com/salesforce/policy_sentry/blob/2575f2b873e1c1f0fb604a448dee392f39ad74ad/policy_sentry/shared/data/iam-definition.json#L406017

use aws::iam::{Allow, Filling, Simulation, Unallowed, simulate};
use aws::{Cli, Problem};

use super::Found;
use crate::iam::{CHECK, Principal};

/// Checks that IAM allows each of `principals`, each with its ARN, each of its statements, filled
/// by `filling`. A statement whose placeholder has no value goes unchecked.
pub(super) fn allowed(
    cli: &Cli,
    filling: &Filling,
    principals: &[(Principal, String, Vec<&'static Allow>)],
    found: &mut Found,
) {
    for (principal, arn, allows) in principals {
        let name = principal.name();
        let resource = format!("{name} {arn}");
        let (simulations, unfilled) = Simulation::of(allows, filling);
        for placeholder in unfilled {
            found.unchecked.push(Problem::new(
                &resource,
                format!(
                    "the statements of its policy that name `{placeholder}` go unsimulated, as \
                     nothing that the check read gives it a value"
                ),
            ));
        }
        for simulation in &simulations {
            let unallowed = match simulate(cli, &CHECK.simulate, arn, simulation) {
                Ok(unallowed) => unallowed,
                Err(error) => {
                    let problem = Problem::unread(&resource, "simulating its policy", error);
                    found.problems.push(problem);
                    continue;
                }
            };
            for Unallowed {
                action,
                resource: on,
                decision,
                missing,
            } in unallowed
            {
                let missing = if missing.is_empty() {
                    String::new()
                } else {
                    format!(", missing {}", missing.join(", "))
                };
                let under = simulation.to_string();
                let under = if under.is_empty() {
                    String::new()
                } else {
                    format!(" where {under}")
                };
                found.problems.push(Problem::new(
                    &resource,
                    format!(
                        "IAM doesn't allow it `{action}` on `{on}`{under}: {decision}{missing}: \
                         attach the policy that `pumpkin-admin policy {name}` prints, as \
                         deploy/aws does"
                    ),
                ));
            }
        }
    }
}
