//! `pumpkin lock`, which writes a recipe's fetch stage in place, as `recipe::fetch` says.

use std::process::ExitCode;

use crate::{cli, config_path, configured_engine, star, task_problems};

/// Writes the fetch stage that `flags` name, of the recipe that the task's images give the
/// inputs it is built with, reading the configuration only once the task is checked and the
/// stage read.
pub(crate) fn lock(flags: cli::Lock) -> Result<ExitCode, String> {
    let cli::Lock {
        entry,
        recipe,
        stage,
        args: cli::TaskArgs(args),
        config,
    } = flags;
    let reach = || configured_engine(&config_path(config)?);
    match star::lock(&entry, &args, &recipe, &stage, reach)? {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(problems) => Err(task_problems(&entry, &problems)),
    }
}
