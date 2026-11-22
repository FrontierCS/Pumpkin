//! What a check finds wrong with a deployment ([`Problem`]).

use std::fmt;

/// A problem that a check found with a deployment. It names the resource, as a person would,
/// such as `role arn:aws:iam::123456789012:role/pumpkin-timer`. It says what is wrong, then what
/// to fix.
// A check reports each finding, so each is a `Problem`. kubeadm's preflight gathers each
// checker's errors the same way, and `brew doctor` gives each check's finding with its remedy:
// https://github.com/kubernetes/kubernetes/blob/8db047b9682a763d9110f321919afdd6e522fe72/cmd/kubeadm/app/preflight/checks.go#L1205-L1229
// https://github.com/Homebrew/brew/blob/3e47189a707c739ee1278a28a3b845f5834574e6/Library/Homebrew/diagnostic/finding.rb#L26-L30
#[derive(Debug)]
pub struct Problem {
    resource: String,
    wrong: String,
}

impl Problem {
    pub fn new(resource: impl fmt::Display, wrong: impl fmt::Display) -> Self {
        Self {
            resource: resource.to_string(),
            wrong: wrong.to_string(),
        }
    }

    /// The problem of a read of `resource` that failed with `error`. `reading` says what the
    /// check read, such as `reading its instance profile`.
    pub fn unread(resource: impl fmt::Display, reading: &str, error: impl fmt::Display) -> Self {
        let wrong = format!(
            "{reading} failed: {error}: check with credentials that `pumpkin-admin policy check` \
             allows"
        );
        Self::new(resource, wrong)
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.resource, self.wrong)
    }
}
