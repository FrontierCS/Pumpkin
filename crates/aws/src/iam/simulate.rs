//! IAM's policy simulator, run on statements filled with a deployment's values.
//!
//! A [`Filling`] holds the values. A [`Simulation`] holds the inputs of one evaluation.
//! [`simulate`] asks the simulator what it allows of a principal.
//!
//! The simulator evaluates the policies attached to the principal and the organization's
//! service control policies, with the context entries it is given. It fetches no resource
//! policy and evaluates no resource control policy. A call that one of those denies is
//! therefore allowed here.
// As IAM's API says of `SimulatePrincipalPolicy`:
// https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/data/iam/2010-05-08/service-2.json#L2443
// https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/data/iam/2010-05-08/service-2.json#L9016-L9075

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;
use serde_json::json;

use super::{
    ACCOUNT, Allow, Call, Condition, Key, List, PARTITION, Part, Placeholder, REGION, Resource,
    Test, Values,
};
use crate::{Bound, Cli};

/// The value that replaces a `*` in a resource's ARN. The simulator takes it as any other value
/// of that part, such as an instance's ID.
const ANY: &str = "stand-in";

/// The values of a deployment's placeholders: its partition, region and account, and the others
/// by name. A placeholder has one value, and a list has several.
#[derive(Debug)]
pub struct Filling {
    partition: String,
    region: String,
    account: String,
    values: BTreeMap<&'static str, Vec<String>>,
}

impl Filling {
    /// A filling of a deployment in `partition`, `region` and `account`, which fill the
    /// placeholders of those names, and no other.
    pub fn new(partition: &str, region: &str, account: &str) -> Self {
        Self {
            partition: partition.to_owned(),
            region: region.to_owned(),
            account: account.to_owned(),
            values: BTreeMap::new(),
        }
    }

    /// Fills `placeholder` with `value`.
    pub fn set(&mut self, placeholder: &'static Placeholder, value: String) {
        self.values.insert(placeholder.name, vec![value]);
    }

    /// Fills `list` with `values`.
    pub fn list(&mut self, list: &'static List, values: Vec<String>) {
        self.values.insert(list.0.name, values);
    }

    /// The value of `placeholder`, if it has one. The value of a list is its first.
    pub fn get(&self, placeholder: &Placeholder) -> Option<&str> {
        let fixed = [
            (&PARTITION, &self.partition),
            (&REGION, &self.region),
            (&ACCOUNT, &self.account),
        ];
        if let Some((_, value)) = fixed
            .iter()
            .find(|(fixed, _)| fixed.name == placeholder.name)
        {
            return Some(value);
        }
        let values = self.values.get(placeholder.name)?;
        values.first().map(String::as_str)
    }

    /// Builds the ARN of `resource` in `service`, in the deployment's partition, region and
    /// account. A `resource` looks like `instance/i-0123456789abcdef0`, and a `service` like
    /// `ec2`.
    pub fn arn(&self, service: &str, resource: &str) -> String {
        self.arn_in(service, &self.region, resource)
    }

    /// Builds the ARN of `resource` in `service` and `region`, as [`arn`](Self::arn) does in the
    /// deployment's region.
    pub fn arn_in(&self, service: &str, region: &str, resource: &str) -> String {
        let Self {
            partition, account, ..
        } = self;
        format!("arn:{partition}:{service}:{region}:{account}:{resource}")
    }

    fn text(&self, parts: &[Part]) -> Result<String, &'static str> {
        let mut text = String::new();
        for part in parts {
            match part {
                Part::Text(literal) => text.push_str(literal),
                Part::Fill(placeholder) => {
                    text.push_str(self.get(placeholder).ok_or(placeholder.name)?);
                }
            }
        }
        Ok(text)
    }
}

/// The inputs of one evaluation of IAM's policy simulator: its actions, its resources and its
/// context entries.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Simulation {
    actions: BTreeSet<String>,
    resources: BTreeSet<String>,
    context: Context,
}

type Context = BTreeMap<String, Entry>;

/// A context entry: its values and their type, as the simulator names the type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    values: Vec<String>,
    kind: &'static str,
}

impl Simulation {
    /// Builds the simulations that must allow every statement of `allows`, filled with
    /// `filling`. A statement gives one simulation for each value of each list that its
    /// conditions name. A `*` in a resource becomes a stand-in, and each condition's key gets a
    /// value that the condition holds for, or none where a `Null` test holds only without the
    /// key. A statement on every resource but some is simulated on `*`. Statements with the same
    /// resources and context share a simulation, with each of their actions. A statement with a
    /// placeholder that has no value is left out, and the second set returned names the
    /// placeholder.
    pub fn of(allows: &[&Allow], filling: &Filling) -> (BTreeSet<Self>, BTreeSet<&'static str>) {
        let mut merged: BTreeMap<(BTreeSet<String>, Context), BTreeSet<String>> = BTreeMap::new();
        let mut unfilled = BTreeSet::new();
        for allow in allows {
            let resources = if allow.but {
                Ok(BTreeSet::from(["*".to_owned()]))
            } else {
                (allow.resources.iter())
                    .map(|resource| resource_arn(resource, filling))
                    .collect()
            };
            let contexts = contexts(allow.conditions, filling);
            let (resources, contexts) = match (resources, contexts) {
                (Ok(resources), Ok(contexts)) => (resources, contexts),
                (Err(name), _) | (_, Err(name)) => {
                    unfilled.insert(name);
                    continue;
                }
            };
            for context in contexts {
                let actions = merged.entry((resources.clone(), context)).or_default();
                actions.extend(allow.actions.iter().map(|action| (*action).to_owned()));
            }
        }
        let simulations = merged
            .into_iter()
            .map(|((resources, context), actions)| Self {
                actions,
                resources,
                context,
            });
        (simulations.collect(), unfilled)
    }
}

impl fmt::Display for Simulation {
    /// Writes its context as `k = v` for each key. The context tells it from another simulation
    /// of the same statement.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = (self.context.iter())
            .map(|(key, entry)| format!("{key} = {}", entry.values.join(", ")));
        f.write_str(&entries.collect::<Vec<_>>().join("; "))
    }
}

/// An action on a resource that a simulation named and the simulator didn't allow.
#[derive(Debug)]
pub struct Unallowed {
    pub action: String,
    pub resource: String,
    /// The simulator's decision, such as `implicitDeny`.
    pub decision: String,
    /// The context keys that the policies test and the simulation gave no value.
    pub missing: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Simulated {
    evaluation_results: Vec<Evaluated>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Evaluated {
    eval_action_name: String,
    eval_resource_name: String,
    eval_decision: String,
    #[serde(default)]
    missing_context_values: Vec<String>,
    /// The decision on each resource of a simulation that names several. The fields above sum
    /// them up, with a pattern in place of a resource's name.
    #[serde(default)]
    resource_specific_results: Vec<OnResource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct OnResource {
    eval_resource_name: String,
    eval_resource_decision: String,
    #[serde(default)]
    missing_context_values: Vec<String>,
}

/// Returns each action of `simulation` on a resource that IAM doesn't allow `principal`, the ARN
/// of a role or a user. Asks the simulator with `call`, an `iam simulate-principal-policy`.
pub fn simulate(
    cli: &Cli,
    call: &'static Call,
    principal: &str,
    simulation: &Simulation,
) -> Result<Vec<Unallowed>, String> {
    let Simulation {
        actions,
        resources,
        context,
    } = simulation;
    let entries = context.iter().map(|(key, Entry { values, kind })| {
        json!({ "ContextKeyName": key, "ContextKeyValues": values, "ContextKeyType": kind })
    });
    let entries = serde_json::Value::from(entries.collect::<Vec<_>>()).to_string();
    let mut args = vec!["--policy-source-arn", principal, "--action-names"];
    args.extend(actions.iter().map(String::as_str));
    args.push("--resource-arns");
    args.extend(resources.iter().map(String::as_str));
    if !context.is_empty() {
        args.extend(["--context-entries", &entries]);
    }
    let Simulated { evaluation_results } = cli.json(call, &args, Bound::Call)?;
    let mut unallowed = Vec::new();
    for evaluated in evaluation_results {
        let Evaluated {
            eval_action_name: action,
            eval_resource_name,
            eval_decision,
            missing_context_values,
            resource_specific_results,
        } = evaluated;
        if eval_decision == "allowed" {
            continue;
        }
        let on: Vec<Unallowed> = (resource_specific_results.into_iter())
            .filter(|on| on.eval_resource_decision != "allowed")
            .map(|on| Unallowed {
                action: action.clone(),
                resource: on.eval_resource_name,
                decision: on.eval_resource_decision,
                missing: on.missing_context_values,
            })
            .collect();
        // Without a decision on each resource, `eval_decision` is the decision on the resource.
        if on.is_empty() {
            unallowed.push(Unallowed {
                action,
                resource: eval_resource_name,
                decision: eval_decision,
                missing: missing_context_values,
            });
        }
        unallowed.extend(on);
    }
    Ok(unallowed)
}

fn resource_arn(resource: &Resource, filling: &Filling) -> Result<String, &'static str> {
    let fill = |placeholder: &'static Placeholder| {
        filling
            .get(placeholder)
            .map(str::to_owned)
            .ok_or(placeholder.name)
    };
    Ok(match resource {
        Resource::Any => "*".to_owned(),
        Resource::Arn(placeholder) => fill(placeholder)?,
        Resource::In(service, path) => {
            let path = filling.text(path)?.replace('*', ANY);
            filling.arn(service, &path)
        }
    })
}

/// The contexts in which every one of `conditions` holds, one for each value of each list they
/// name.
fn contexts(conditions: &[Condition], filling: &Filling) -> Result<Vec<Context>, &'static str> {
    let mut contexts = vec![BTreeMap::new()];
    for Condition { test, key, values } in conditions {
        let key = match key {
            Key::Named(name) => (*name).to_owned(),
            Key::Tag(service, tag) => format!("{service}:ResourceTag/{tag}"),
            Key::RequestTag(tag) => format!("aws:RequestTag/{tag}"),
        };
        let choices: Vec<Vec<String>> = match values {
            Values::Are(parts) => {
                let values = parts
                    .iter()
                    .map(|part| filling.text(std::slice::from_ref(part)));
                vec![values.collect::<Result<_, _>>()?]
            }
            Values::Listed(list) => {
                let values = filling.values.get(list.0.name).ok_or(list.0.name)?;
                values.iter().map(|value| vec![value.clone()]).collect()
            }
        };
        let (kind, choices) = match test {
            // The context always has the key, so `StringEqualsIfExists` acts as `StringEquals`.
            Test::ArnEquals | Test::StringEquals | Test::StringEqualsIfExists => {
                ("string", choices)
            }
            Test::Bool => ("boolean", choices),
            // `Null` with `false` holds when the key has any value, and with `true` when it has
            // none.
            Test::Null if choices == [vec!["false".to_owned()]] => {
                ("string", vec![vec![ANY.to_owned()]])
            }
            Test::Null => continue,
        };
        contexts = (contexts.iter())
            .flat_map(|context| {
                choices.iter().map(|values| {
                    let mut context = context.clone();
                    let entry = Entry {
                        values: values.clone(),
                        kind,
                    };
                    context.insert(key.clone(), entry);
                    context
                })
            })
            .collect();
    }
    Ok(contexts)
}
