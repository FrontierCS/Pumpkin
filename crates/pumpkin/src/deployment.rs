//! `pumpkin-admin check`: a check, with read-only calls on AWS, that a controller's deployment is
//! as this release relies on, from the ARN of its settings alone, which name the rest.
//!
//! It checks what an apply of deploy/aws doesn't make by construction:
//!
//! - The settings parse as a job takes them. A module and a release of different tags may
//!   disagree on them.
//! - The members' list parses. The admin writes it.
//! - One live instance carries the controller's tag, and it runs with a public address. An
//!   apply leaves the instance's public address as it is.
//! - IAM allows each role its policy, as `pumpkin-admin policy` prints it, filled with the
//!   deployment's values. The policies of a module of another tag, or a service control policy,
//!   may deny it. The controller's role is the one of its instance profile, so an instance with
//!   no profile, or a profile with no role, is a problem too. The controller's statements for the
//!   Modal provider are checked only if the settings name one.
//!
//! Every check runs, and each problem it finds is a [`Problem`], said on a line of its own once
//! all have run; with none, the command says nothing. What only a running controller shows, a
//! stopped one leaves unchecked, as a line that fails nothing, since the check starts nothing.
//! Reachability is left to the client's connection to the controller's `sshd`, which says what to
//! fix, and to AWS's VPC Reachability Analyzer.
// As kubeadm's preflight runs every checker and gathers their errors before it fails, and `brew
// doctor` gathers each check's finding with its remedy and exits nonzero on any; both say a line
// on success too, which this leaves out, as quiet success is the rule here:
// https://github.com/kubernetes/kubernetes/blob/8db047b9682a763d9110f321919afdd6e522fe72/cmd/kubeadm/app/preflight/checks.go#L71-L74
// https://github.com/Homebrew/brew/blob/3e47189a707c739ee1278a28a3b845f5834574e6/Library/Homebrew/cmd/doctor.rb#L58-L102

use std::net::Ipv4Addr;
use std::path::Path;

use aws::iam::Filling;
use aws::{Cli, ParameterArn, Problem, Region};
use instance::{Described, Instance, ProfileNamed};
use members::Members;

use crate::cli::Exit;
use crate::config::{self, Config, Kind};
use crate::controller::{self, Name};
use crate::iam::{self, CHECK, Principal};
use crate::job::{self, Deployment, Source, Taken};
use crate::say;

/// How a job's messages name the settings, in a problem that names them already.
const NAMED: &str = "they";

/// The address that the check's configuration gives the gateway and Modal's `dials_from`, in
/// place of the controller's, which only a job on it reads: the first of TEST-NET-1, which RFC
/// 5737 keeps for documentation.
const STAND_IN: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

mod instance;
mod simulation;

/// Checks the deployment whose settings are SSM parameter `settings`, says each problem, and
/// returns [`Exit::Failure`] if there is any, [`Exit::Success`] otherwise.
pub(crate) fn check(settings: &ParameterArn) -> Result<Exit, String> {
    let cli = Cli::connect(settings.region().clone())?;
    let mut found = Found::default();
    let partition = settings.partition();
    let mut filling = Filling::new(partition, settings.region().as_str(), settings.account());
    filling.set(&iam::SETTINGS, settings.as_str().to_owned());
    let Read { config, deployment } = read(&cli, settings, &mut found.problems);
    let mut controller = None;
    if let Some(Deployment { name, members }) = deployment {
        match members {
            Source::Parameter(members) => {
                self::members(&members, &name, &mut found.problems);
                filling.set(&iam::MEMBERS, members.as_str().to_owned());
            }
            Source::File(path) => found.problems.push(Problem::new(
                format!("settings {settings}"),
                format!(
                    "their `[deployment] members` names the file {}, which a controller on EC2 \
                     doesn't read: name the members' SSM parameter, as deploy/aws writes it",
                    path.display()
                ),
            )),
        }
        controller = self::controller(&cli, settings.region(), &name, &mut found);
    }
    // The controller's jobs make the Modal provider's calls only if the settings name one.
    let runs_modal = (config.iter().flat_map(|config| config.providers.by_name()))
        .any(|(_, kind)| matches!(kind, Kind::Modal(_)));
    let mut principals = Vec::new();
    if let Some(Controller { id, role }) = controller {
        let arn = filling.arn("ec2", &format!("instance/{id}"));
        filling.set(&iam::CONTROLLER, arn);
        principals
            .extend(role.map(|role| (Principal::Controller, role, iam::controller(runs_modal))));
    }
    if let Some(config) = &config {
        providers(config, &mut filling, &mut found.problems);
    }
    // The roles that the providers' settings and templates name, as their reads fill them.
    let roles = [
        (Principal::Timer, &ec2::iam::TIMER_ROLE),
        (Principal::Sandbox, &ec2::iam::SANDBOX_ROLE),
        (Principal::ModalPull, &modal::iam::PULL_ROLE),
    ];
    for (principal, role) in roles {
        let arn = filling.get(role).map(str::to_owned);
        principals.extend(arn.map(|arn| (principal, arn, iam::allows(principal))));
    }
    simulation::allowed(&cli, &filling, &principals, &mut found);
    let Found {
        problems,
        unchecked,
    } = found;
    for problem in problems.iter().chain(&unchecked) {
        say(problem);
    }
    Ok(if problems.is_empty() {
        Exit::Success
    } else {
        Exit::Failure
    })
}

/// Fills in `filling` the placeholders of the policies of the providers of `config`, each problem
/// of a read in `problems`.
fn providers(config: &Config, filling: &mut Filling, problems: &mut Vec<Problem>) {
    for (name, kind) in config.providers.by_name() {
        let found = match kind {
            Kind::Ec2(settings) => ec2::envelope::fill(settings, filling),
            Kind::Modal(settings) => {
                modal::envelope::fill(settings, filling);
                Vec::new()
            }
            Kind::Docker(_) => Vec::new(),
        };
        let named = found.into_iter();
        problems
            .extend(named.map(|problem| Problem::new(format_args!("provider `{name}`"), problem)));
    }
}

/// What the checks found: the problems, and what they left unchecked, which fails nothing.
#[derive(Default)]
struct Found {
    problems: Vec<Problem>,
    unchecked: Vec<Problem>,
}

/// The settings, as a job reads them, but with a stand-in for what the controller and the client
/// give a job, and their `[deployment]`, each if it parses.
#[derive(Default)]
struct Read {
    config: Option<Config>,
    deployment: Option<Deployment>,
}

/// The settings that are SSM parameter `settings`, as [`Read`] holds them, with each problem in
/// `problems`.
fn read(cli: &Cli, settings: &ParameterArn, problems: &mut Vec<Problem>) -> Read {
    let resource = format!("settings {settings}");
    let mut problem = |wrong: String| problems.push(Problem::new(&resource, wrong));
    let value = job::parameter_value(cli, &CHECK.settings, settings.as_str()).map_err(|error| {
        problem(format!(
            "reading them failed: {error}: name the parameter that deploy/aws outputs as \
             `settings_parameter_arn`, and check with credentials that `pumpkin-admin policy \
             check` allows"
        ));
    });
    let Ok(mut tables) =
        value.and_then(|value| job::settings_tables(&value, NAMED).map_err(&mut problem))
    else {
        return Read::default();
    };
    let deployment = job::deployment(&mut tables.clone(), NAMED).map(|(_, deployment)| deployment);
    // A drain is the admin's own, and each job that it refuses says so.
    let taken = deployment.and_then(|deployment| Ok((deployment, job::take(&mut tables, NAMED)?)));
    let (
        deployment,
        Taken {
            version,
            draining: _,
        },
    ) = match taken {
        Ok(taken) => taken,
        Err(error) => {
            problem(error);
            return Read::default();
        }
    };
    // A Modal provider's job the client completes.
    let modal = job::names_kind(&tables, "modal").then(|| {
        toml::Table::from_iter([
            ("profile".to_owned(), "check".into()),
            ("classes".to_owned(), toml::Table::new().into()),
            ("dials_from".to_owned(), STAND_IN.to_string().into()),
        ])
    });
    let upstreams = toml::Value::Table(toml::Table::new());
    let configured = job::configuration(
        tables,
        NAMED,
        version,
        Some(STAND_IN),
        modal.as_ref(),
        upstreams,
    );
    let config = match configured {
        // Its errors name the path, which is the settings' ARN here.
        Ok(text) => config::parse(Path::new(settings.as_str()), &text)
            .map_err(|error| problems.push(Problem::new("settings", error)))
            .ok(),
        Err(error) => {
            problems.push(Problem::new(&resource, error));
            None
        }
    };
    Read {
        config,
        deployment: Some(deployment),
    }
}

/// Checks that the members' list of controller `name`, SSM parameter `members`, parses.
fn members(members: &ParameterArn, name: &Name, problems: &mut Vec<Problem>) {
    let resource = format!("members {members}");
    let list = Cli::connect(members.region().clone())
        .and_then(|cli| job::parameter_value(&cli, &CHECK.members, members.as_str()));
    let list = match list {
        Ok(list) => list,
        Err(error) => {
            problems.push(Problem::new(
                resource,
                format!(
                    "reading them failed: {error}: name the members' parameter of controller \
                     `{name}` in the settings' `[deployment] members`, as deploy/aws writes it, \
                     and check with credentials that `pumpkin-admin policy check` allows"
                ),
            ));
            return;
        }
    };
    if let Err(why) = Members::parse(&list) {
        problems.push(Problem::new(
            resource,
            format!("{why}, so the controller copies none of the keys until it is corrected"),
        ));
    }
}

/// What the check of the controller found of its instance that the other checks read.
struct Controller {
    /// Its instance's ID.
    id: String,
    /// The ARN of its role, if IAM describes it.
    role: Option<String>,
}

/// Checks controller `name`'s instance in `region`, as its tag finds it, as
/// [`Described::problems`] decides. Returns what the other checks read of it, if there is one.
fn controller(cli: &Cli, region: &Region, name: &Name, found: &mut Found) -> Option<Controller> {
    let instance = match tagged(cli, region, name) {
        Ok(instance) => instance,
        Err(problem) => {
            found.problems.push(problem);
            return None;
        }
    };
    let profile = (instance.iam_instance_profile.as_ref()).map(|ProfileNamed { arn }| {
        // Of an ARN, the name follows the path.
        let name = arn.rsplit_once('/').map_or(arn.as_str(), |(_, name)| name);
        cli.instance_profile(&CHECK.profile, name)
    });
    let described = Described { instance, profile };
    let resource = format!("controller `{name}`, {}", described.instance.id);
    (found.problems).extend(described.problems(&resource));
    (found.unchecked).extend(described.unchecked(&resource));
    let role = described.role().map(str::to_owned);
    let id = described.instance.id;
    Some(Controller { id, role })
}

/// The one live instance in `region` that carries controller `name`'s tag.
fn tagged(cli: &Cli, region: &Region, name: &Name) -> Result<Instance, Problem> {
    let resource = format!("controller `{name}`");
    let filter = controller::tagged(name);
    let instances: Vec<Instance> = controller::instances(cli, &CHECK.instances, &filter)
        .map_err(|error| Problem::unread(&resource, "looking for its instance", error))?;
    let mut live: Vec<Instance> = (instances.into_iter())
        .filter(|instance| controller::live(&instance.state.name))
        .collect();
    let tag = controller::tag(name);
    let region = region.as_str();
    if live.len() > 1 {
        let ids: Vec<&str> = live.iter().map(|instance| instance.id.as_str()).collect();
        return Err(Problem::new(
            resource,
            format!(
                "the live instances {} in {region} each carry the tag {tag}, where `pumpkin` \
                 finds the controller as the one: terminate each but the one that deploy/aws \
                 made",
                ids.join(", ")
            ),
        ));
    }
    live.pop().ok_or_else(|| {
        Problem::new(
            resource,
            format!(
                "no live instance in {region} carries the tag {tag}: apply deploy/aws, whose \
                 controller carries it, with the `name` that the settings' `[deployment]` names"
            ),
        )
    })
}
