//! The files of a job's directory that name the build that runs it. A controller's admin reads them
//! to see which releases its jobs run, before a change that an older release would meet. `release`
//! names the build, as `pumpkin --version` shows it. `envelope` names the envelopes whose settings
//! it takes, as Cargo's caret requirement `^M.m` writes them: the minor `M.m` or a later one of
//! major `M`
//! (<https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/doc/book/src/reference/specifying-dependencies.md#L51-L57>).
//! The job's `pumpkin` link names its install. A job of a build from before these has neither
//! file.

use std::path::Path;

const RELEASE: &str = "release";

const ENVELOPE: &str = "envelope";

/// Names this build, and the envelopes that it takes, in the job's directory `job`.
pub(super) fn write(job: &Path) -> Result<(), String> {
    // GitHub's runner likewise names its version to the service on each poll. That tells when an
    // older one may be retired:
    // https://github.com/actions/runner/blob/d1eb12d4b115c301201de27d667284e1a80f2ab5/src/Runner.Listener/BrokerMessageListener.cs#L299-L305
    super::line::write(job, RELEASE, &runtime::BUILD.to_string())?;
    super::line::write(job, ENVELOPE, &format!("^{}", super::settings::OLDEST))
}
