//! AWS's command line, `aws`, as Pumpkin's providers run it ([`Cli`]), with ECR's repositories
//! on it, the IAM roles that providers name in their settings ([`RoleArn`]), and SSM's parameters
//! ([`ParameterArn`]).
//!
//! `Cli` runs only a declared [`Call`], which states what IAM must allow of it. So a principal's
//! policy is what its code's calls state ([`iam::policy`]).
//!
//! Every call `aws` makes is in the region its [`Cli`] was made for, with the credentials that
//! `aws` finds on its own, which on an EC2 instance are the instance's role. No variable of this
//! process's environment, such as `AWS_REGION` or `AWS_PROFILE`, steers `aws` to another region
//! or account.

use std::fmt;
use std::process::Command;
use std::time::Duration;

use engine::{Login, MANIFEST_TYPES, Pusher};
pub use problem::Problem;
use schema::ir::Image;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::process::{self, Echo, Environment, Failure, Run};
use stdx::{Context as _, Deadline};

use crate::iam::{Call, Ecr};

pub mod iam;
mod problem;

// Every command a provider runs is one AWS answers at once, in one page. Throttled, `aws` tries
// again ([`RETRIES`]), waiting about 7 min in all at most; it gives up on a connection, and on a
// read that gets nothing, after 60 s each. So past 10 min a command is hung, as on a credentials
// process that never ends, or AWS is out, and only a bound turns that into a failure, rather
// than a wait without end.
const CALL_TIMEOUT: Duration = Duration::from_mins(10);

/// The variables of `aws`'s own that set how it retries a request, which it gets beside
/// [`CALL_ENVIRONMENT`]: 25 attempts in all, in its standard mode.
// AWS throttles a burst of calls, as when many runs are launched at once, and `aws`, which makes
// 3 attempts by default, then fails runs that only had to wait. Its standard mode waits a random
// time before each retry, up to twice the last bound, from 1 s to 20 s at most, so 25 attempts
// wait 7 min at most, half that on average; legacy mode, which a configuration may name, has no
// such cap.
const RETRIES: [(&str, &str); 2] = [("AWS_MAX_ATTEMPTS", "25"), ("AWS_RETRY_MODE", "standard")];

/// What `aws` gets of this process's environment: what it needs to find its own configuration.
// `HOME`, for `~/.aws`; `PATH`, for the programs its configuration may run for credentials; and
// its own variables for where that configuration lives. Any other variable of its, such as
// `AWS_REGION`, `AWS_PROFILE` or `AWS_ACCESS_KEY_ID`, would steer it past the region it is given.
const CALL_ENVIRONMENT: Environment = Environment::Only(&[
    "HOME",
    "PATH",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
]);

/// The user name of every login to ECR, whose password `aws ecr get-login-password` gives.
const ECR_USER: &str = "AWS";

/// The failure code by which ECR says it holds no image by the identifier it was asked.
const NOT_FOUND: &str = "ImageNotFound";

/// The most images that one `aws ecr batch-get-image` asks for.
// ECR's BatchGetImage takes 1 to 100 image ids:
// https://docs.aws.amazon.com/AmazonECR/latest/APIReference/API_BatchGetImage.html
const MAX_LOOKUP_IMAGES: usize = 100;

/// An AWS region, such as `us-west-1`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct Region(String);

impl Region {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Region {
    type Error = String;

    fn try_from(region: String) -> Result<Self, Self::Error> {
        let allowed = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        let holds = (region.split('-')).all(|word| !word.is_empty() && word.bytes().all(allowed));
        if !holds {
            return Err(format!(
                "`{}` is not an AWS region: use one such as `us-west-1`",
                region.escape_debug()
            ));
        }
        Ok(Self(region))
    }
}

/// The name of an ECR repository, such as `pumpkin/roles`, in ECR's grammar of names.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String")]
pub struct RepositoryName(String);

impl RepositoryName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RepositoryName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        // ECR's grammar: 2 to 256 bytes of components split by `/`. A component is words of
        // lowercase letters and digits, split by `.`, `_`, `__` or a run of `-`:
        // https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/data/ecr/2015-09-21/service-2.json#L4709-L4714
        let word = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        let component = |component: &str| {
            component.starts_with(word)
                && component.ends_with(word)
                && component.split(word).all(|separator| {
                    matches!(separator, "." | "_" | "__") || separator.bytes().all(|b| b == b'-')
                })
        };
        if !(2..=256).contains(&name.len()) || !name.split('/').all(component) {
            return Err(format!(
                "`{}` is not an ECR repository's name: use one such as `pumpkin/roles`, of 2 to \
                 256 bytes: lowercase letters and digits, split by `/`, `.`, `_`, `__` or a run \
                 of `-`",
                name.escape_debug()
            ));
        }
        Ok(Self(name))
    }
}

/// The ARN of an IAM role, such as `arn:aws:iam::123456789012:role/pumpkin-timer`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoleArn {
    arn: String,
    /// The AWS partition it is in, such as `aws`.
    partition: String,
}

impl RoleArn {
    pub fn as_str(&self) -> &str {
        &self.arn
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }
}

impl From<RoleArn> for String {
    fn from(role: RoleArn) -> Self {
        role.arn
    }
}

impl TryFrom<String> for RoleArn {
    type Error = String;

    fn try_from(arn: String) -> Result<Self, Self::Error> {
        // `arn:<partition>:iam::<account>:role/<name>`; AWS refuses a role it doesn't hold.
        let parts = (arn.strip_prefix("arn:")).and_then(|rest| rest.split_once(":iam::"));
        let partition = parts.filter(|(partition, rest)| {
            !partition.is_empty() && !partition.contains(':') && rest.contains(":role/")
        });
        let Some((partition, _)) = partition else {
            return Err(format!(
                "`{}` is not an IAM role's ARN: use one such as \
                 `arn:aws:iam::123456789012:role/pumpkin-timer`",
                arn.escape_debug()
            ));
        };
        let partition = partition.to_owned();
        Ok(Self { arn, partition })
    }
}

/// The ARN of an SSM parameter, such as
/// `arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings`. SSM's calls take it in
/// place of the parameter's name, in the parameter's region.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ParameterArn {
    arn: String,
    partition: String,
    region: Region,
    account: String,
}

impl ParameterArn {
    pub fn as_str(&self) -> &str {
        &self.arn
    }

    /// The AWS partition it is in, such as `aws`.
    pub fn partition(&self) -> &str {
        &self.partition
    }

    /// The region that holds it. A call on it is made there.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The ID of the account that holds it.
    pub fn account(&self) -> &str {
        &self.account
    }
}

impl fmt::Display for ParameterArn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.arn.fmt(f)
    }
}

impl From<ParameterArn> for String {
    fn from(parameter: ParameterArn) -> Self {
        parameter.arn
    }
}

impl TryFrom<String> for ParameterArn {
    type Error = String;

    fn try_from(arn: String) -> Result<Self, Self::Error> {
        // `arn:<partition>:ssm:<region>:<account>:parameter/<name>`. The name leaves out its
        // leading `/`, and holds a-z, A-Z, 0-9, `_`, `.`, `-` and `/`, as SSM's API says of a
        // name. GetParameter takes an ARN in place of the name:
        // https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/data/ssm/2014-11-06/service-2.json#L15336
        // https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/data/ssm/2014-11-06/service-2.json#L9259-L9261
        let parts: Vec<&str> = arn.splitn(6, ':').collect();
        let name = |name: &str| {
            let allowed = |byte: u8| byte.is_ascii_alphanumeric() || b"_.-/".contains(&byte);
            !name.is_empty() && !name.starts_with('/') && name.bytes().all(allowed)
        };
        let parsed = match parts[..] {
            ["arn", partition, "ssm", region, account, resource]
                if !partition.is_empty()
                    && account.len() == 12
                    && account.bytes().all(|byte| byte.is_ascii_digit())
                    && resource.strip_prefix("parameter/").is_some_and(name) =>
            {
                let region = Region::try_from(region.to_owned()).ok();
                region.map(|region| (partition.to_owned(), region, account.to_owned()))
            }
            _ => None,
        };
        let Some((partition, region, account)) = parsed else {
            return Err(format!(
                "`{}` is not an SSM parameter's ARN: use one such as \
                 `arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings`",
                arn.escape_debug()
            ));
        };
        Ok(Self {
            arn,
            partition,
            region,
            account,
        })
    }
}

/// An ECR repository, as AWS describes it ([`Cli::repositories`]).
#[derive(Debug, Serialize)]
pub struct Repository {
    name: RepositoryName,
    /// Its URI, `<registry>/<name>`, which every image in it is named under.
    uri: String,
    #[serde(skip)]
    ecr: &'static Ecr,
}

impl Repository {
    pub fn name(&self) -> &RepositoryName {
        &self.name
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Its registry, which its URI names first.
    pub fn registry(&self) -> &str {
        let uri = &self.uri;
        uri.split_once('/').map_or(uri, |(registry, _)| registry)
    }

    /// The reference of `image` in it, by its digest: `<URI>@<image>`. A pull by the reference
    /// finds the image once [`Cli::publish`] has published it here.
    pub fn reference(&self, image: &Image) -> String {
        format!("{}@{image}", self.uri)
    }
}

/// An IAM instance profile, as IAM describes it ([`Cli::instance_profile`]).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InstanceProfile {
    pub arn: String,
    pub roles: Vec<ProfileRole>,
}

/// A role that an instance profile holds, by its ARN.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProfileRole {
    pub arn: String,
}

/// When an `aws` command is killed if it hasn't ended.
#[derive(Clone, Copy)]
pub enum Bound {
    /// 10 min after it starts (`CALL_TIMEOUT`).
    Call,
    /// At a create's deadline, `provider::Create::by`, as this deadline.
    Create(Deadline),
}

/// The `aws` command line, AWS CLI v2, found on `PATH`, run in one region, given only the
/// environment that finds its own configuration.
#[derive(Debug)]
pub struct Cli {
    region: Region,
    version: String,
}

/// What `aws ecr describe-repositories` prints.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EcrRepositories {
    repositories: Vec<EcrRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EcrRepository {
    repository_name: String,
    repository_uri: String,
}

/// What `aws ecr batch-get-image` prints of the images it was asked for: those it found, and a
/// failure for each other. Either list is left out when empty.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchImages {
    #[serde(default)]
    images: Vec<FoundImage>,
    #[serde(default)]
    failures: Vec<ImageFailure>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundImage {
    image_id: ImageId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageFailure {
    image_id: ImageId,
    failure_code: String,
    failure_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageId {
    image_digest: Option<String>,
}

impl Cli {
    /// The command line, run in `region`, with the version `aws --version` printed.
    pub fn connect(region: Region) -> Result<Self, String> {
        let how = bounded(Bound::Call).named("aws --version");
        let output = process::run(aws().arg("--version"), how).map_err(|failure| {
            match failure {
                // Version 1, which some package managers install, takes none of the options that
                // every other command gives, such as `--no-cli-pager`.
                Failure::Unstarted { name, error, .. } => Failure::Unstarted {
                    name,
                    error,
                    expected: "install AWS CLI v2 on `PATH`".to_owned(),
                },
                failure @ (Failure::Unwatched { .. }
                | Failure::Unended { .. }
                | Failure::Exited { .. }) => failure,
            }
            .to_string()
        })?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok(Self { region, version })
    }

    /// What `aws --version` printed, such as `aws-cli/2.38.1 Python/3.14.6 Linux/…`.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Runs `call` with `args`, its options, killed at `bound`, and parses the JSON it prints.
    pub fn json<T: DeserializeOwned>(
        &self,
        call: &'static Call,
        args: &[&str],
        bound: Bound,
    ) -> Result<T, String> {
        serde_json::from_slice(&self.run(call, args, bound)?).map_err(|error| {
            format!(
                "the JSON `{}` printed isn't of the form expected: {error}",
                shown(call)
            )
        })
    }

    /// Reads the instance profile named `name` with `call`, an `iam get-instance-profile`.
    pub fn instance_profile(
        &self,
        call: &'static Call,
        name: &str,
    ) -> Result<InstanceProfile, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Got {
            instance_profile: InstanceProfile,
        }
        let args = ["--instance-profile-name", name];
        let Got { instance_profile } = self.json(call, &args, Bound::Call)?;
        Ok(instance_profile)
    }

    /// Runs `call` with `args`, its options, killed at `bound`, and returns what it prints on
    /// standard output.
    pub fn run(&self, call: &'static Call, args: &[&str], bound: Bound) -> Result<Vec<u8>, String> {
        let output = process::run(&mut self.command(call, args), bounded(bound));
        Ok(output.map_err(|failure| failure.to_string())?.stdout)
    }

    /// As [`run`](Self::run) does, but returns `None` if AWS reports that what it names doesn't
    /// exist (`ResourceNotFoundException`).
    pub fn run_found(
        &self,
        call: &'static Call,
        args: &[&str],
        bound: Bound,
    ) -> Result<Option<Vec<u8>>, String> {
        // How AWS CLI v2 writes the code of an error that AWS reports.
        let absent = "An error occurred (ResourceNotFoundException)";
        match process::run(&mut self.command(call, args), bounded(bound)) {
            Ok(output) => Ok(Some(output.stdout)),
            Err(failure) if failure.reported().contains(absent) => Ok(None),
            Err(failure) => Err(failure.to_string()),
        }
    }

    /// The repositories that `names` name, in their order. One call of `ecr`'s describes each.
    /// Each keeps `ecr`'s calls, with which an image is published to it. Fails if AWS describes
    /// none of a name, and says why.
    pub fn repositories<const N: usize>(
        &self,
        ecr: &'static Ecr,
        names: [&RepositoryName; N],
    ) -> Result<[Result<Repository, String>; N], String> {
        let mut args = vec!["--repository-names"];
        args.extend(names.iter().map(|name| name.as_str()));
        let EcrRepositories { repositories } = self.json(&ecr.describe, &args, Bound::Call)?;
        Ok(names.map(|name| {
            let found =
                (repositories.iter()).find(|repository| repository.repository_name == name.0);
            let Some(found) = found else {
                return Err(format!(
                    "AWS described no repository `{}`: expected each repository named to exist",
                    name.0
                ));
            };
            Ok(Repository {
                name: name.clone(),
                uri: found.repository_uri.clone(),
                ecr,
            })
        }))
    }

    /// Makes `repository` hold each of `images`, which are distinct images of `engine`'s. It pushes
    /// each from the engine unless ECR answers that the repository holds it already, by its
    /// digest. The push tags the image with its digest's hexadecimal digits. It fails if the
    /// repository holds no image by an image's digest once its push ends.
    pub fn publish(
        &self,
        engine: &Pusher<'_>,
        repository: &Repository,
        images: &[&Image],
    ) -> Result<(), String> {
        let answer = self.look_up(repository, images);
        for image in images {
            if holds(repository, image, &answer)? {
                continue;
            }
            let pushed = self.push(engine, repository, image);
            // Another publisher, such as a run that starts at once with the same image, may have
            // pushed it since: then this push fails, as the repository's tags can't be moved, and
            // the repository holds the image all the same.
            let held = holds(repository, image, &self.look_up(repository, &[image]));
            match (pushed, held) {
                (_, Ok(true)) => Ok(()),
                (Ok(()), Ok(false)) => Err(format!(
                    "repository `{}` holds no image {image} once it was pushed there: expected \
                     the push to keep the image's manifest, as the containerd image store does",
                    repository.name.0
                )),
                (Err(pushing), Ok(false)) => Err(pushing),
                (Ok(()), Err(looking)) => Err(looking),
                (Err(pushing), Err(looking)) => Err(format!("{pushing}; and then {looking}")),
            }?;
        }
        Ok(())
    }

    /// What `aws ecr batch-get-image` answers for `images` in `repository`, by their digests. It
    /// asks in calls of at most [`MAX_LOOKUP_IMAGES`] images each. It fails with the error of the
    /// first call that fails. It makes no call when `images` is empty.
    fn look_up(&self, repository: &Repository, images: &[&Image]) -> Result<BatchImages, String> {
        let mut answer = BatchImages {
            images: Vec::new(),
            failures: Vec::new(),
        };
        for chunk in images.chunks(MAX_LOOKUP_IMAGES) {
            let ids: Vec<String> = (chunk.iter())
                .map(|image| format!("imageDigest={image}"))
                .collect();
            let mut args = vec!["--repository-name", &repository.name.0, "--image-ids"];
            args.extend(ids.iter().map(String::as_str));
            args.push("--accepted-media-types");
            args.extend(MANIFEST_TYPES);
            let BatchImages { images, failures } =
                self.json(&repository.ecr.holds, &args, Bound::Call)?;
            answer.images.extend(images);
            answer.failures.extend(failures);
        }
        Ok(answer)
    }

    fn push(
        &self,
        pusher: &Pusher<'_>,
        repository: &Repository,
        image: &Image,
    ) -> Result<(), String> {
        // The password is good for the registry of the account and region `aws` calls, which
        // holds every repository there.
        let password = self.run(&repository.ecr.login, &[], Bound::Call)?;
        let password = String::from_utf8_lossy(&password).trim().to_owned();
        let login = Login::new(
            repository.registry().to_owned(),
            ECR_USER.to_owned(),
            password,
        );
        // A tag that its image's digest names names one image, as the repository's tags can't
        // be moved. The digits alone: `sha256-` and the digits is the tag of the index of the
        // image's referrers, such as its signatures, where a registry has no referrers API.
        pusher.push(image, &login, &repository.name.0, image.hex())
    }

    fn command(&self, call: &Call, args: &[&str]) -> Command {
        let mut command = aws();
        // After `args`, so that a failure is named by the command they give, as `aws` takes its
        // global options anywhere: its parser of them reads every argument, and leaves the rest
        // to the command.
        // https://github.com/aws/aws-cli/blob/7372e144ac4a20def37cc46ba60f1bcce4aec215/awscli/clidriver.py#L454-L460
        command.args(call.command()).args(args).args([
            "--no-cli-pager",
            "--output",
            "json",
            "--region",
            &self.region.0,
        ]);
        command
    }
}

/// Whether `repository` holds `image`, by its digest, as `answer` says: with the image, or with a
/// failure whose code is [`NOT_FOUND`] for it. `answer` is what a lookup of images that include it
/// answered ([`Cli::look_up`]).
fn holds(
    repository: &Repository,
    image: &Image,
    answer: &Result<BatchImages, String>,
) -> Result<bool, String> {
    let looking = format_args!(
        "looking up {image} in repository `{}` failed",
        repository.name.0
    );
    let BatchImages { images, failures } = answer.as_ref().context(looking)?;
    let ours = |id: &ImageId| id.image_digest.as_deref() == Some(image.as_str());
    if images.iter().any(|found| ours(&found.image_id)) {
        return Ok(true);
    }
    let absent =
        |failure: &ImageFailure| ours(&failure.image_id) && failure.failure_code == NOT_FOUND;
    if failures.iter().any(absent) {
        return Ok(false);
    }
    // A failure that names no digest may be of any image the lookup asked for.
    let mine = |failure: &&ImageFailure| {
        let id = &failure.image_id;
        id.image_digest.is_none() || ours(id)
    };
    let reported = failures.iter().filter(mine).map(|failure| {
        let reason = failure.failure_reason.as_deref().unwrap_or_default();
        format!(
            "`{}` ({})",
            failure.failure_code,
            reason.trim_end_matches('.')
        )
    });
    Err(format!(
        "{looking}: ECR reports {}: expected the image, or `{NOT_FOUND}` for it",
        reported.collect::<Vec<_>>().join("; ")
    ))
}

fn aws() -> Command {
    let mut command = stdx::process::command("aws", CALL_ENVIRONMENT);
    command.envs(RETRIES);
    command
}

/// A silent run of an `aws` command, killed at `bound`.
fn bounded(bound: Bound) -> Run<'static> {
    let bound = match bound {
        Bound::Call => process::Bound::Within(CALL_TIMEOUT),
        Bound::Create(by) => process::Bound::By(by),
    };
    Run::new(bound, Echo::Silent)
}

/// How an error shows `call`'s command, such as `aws ec2 describe-images`, without its options,
/// which can be long.
fn shown(call: &Call) -> String {
    let [service, operation] = call.command();
    format!("aws {service} {operation}")
}
