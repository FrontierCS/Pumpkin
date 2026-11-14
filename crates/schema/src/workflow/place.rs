//! Placing a parsed workflow's roles in classes of sandbox: [`Workflow::place`].

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{ByRole, ParseErrors, Placed, Sandbox, Workflow};
use crate::ir::{Name, Resources};
use crate::provider::Class;

impl Workflow {
    /// Places each role in the smallest of `classes` that fits it: the least memory, then the
    /// least CPU, then the first by name. A class fits a role that needs no more of each
    /// resource than the class gives a role ([`Class::resources`]).
    ///
    /// # Errors
    ///
    /// Returns each role that fits no class, [at](super::ParseError::at) its needs, saying of
    /// each class the first of the role's needs, in the IR's order, that it doesn't meet.
    pub fn place(self, classes: &BTreeMap<Name, Class>) -> Result<Placed, ParseErrors> {
        let mut errors = Errors::default();
        // Every role is placed before any error is returned, so that each role that fits no
        // class is reported.
        let fitted: Vec<Result<Duration, Reported>> = (self.roles.iter())
            .map(|sandbox| smallest(&mut errors, sandbox, classes))
            .collect();
        let startups = fitted.into_iter().collect::<Result<_, _>>().map(ByRole);
        Ok(Placed {
            workflow: self,
            startups: errors.finish(startups)?,
        })
    }
}

/// The startup of the smallest of `classes` that `sandbox`'s role fits, as [`Workflow::place`]
/// says; otherwise reports why each doesn't fit it, at the role's needs in the IR.
fn smallest(
    errors: &mut Errors,
    sandbox: &Sandbox,
    classes: &BTreeMap<Name, Class>,
) -> Result<Duration, Reported> {
    let Sandbox {
        name, needs, at, ..
    } = sandbox;
    let (mut fits, mut misfits) = (Vec::new(), Vec::new());
    for (class_name, class) in classes {
        match misfit(needs, &class.resources) {
            None => fits.push((class_name, class)),
            Some(why) => misfits.push(format!("`{class_name}` {why}")),
        }
    }
    let smallest = fits
        .into_iter()
        .min_by_key(|&(name, class)| size(name, class));
    if let Some((_, class)) = smallest {
        return Ok(Duration::from_secs(class.startup_seconds.get()));
    }
    let why = if misfits.is_empty() {
        "no class is offered".to_owned()
    } else {
        misfits.join("; ")
    };
    Err(errors.report(
        at.token("needs"),
        format!("`{name}` fits no class of sandbox, as {why}: ask for no more than a class offers"),
    ))
}

/// How [`Workflow::place`] orders the classes that a role fits, smallest first.
fn size<'c>(name: &'c Name, class: &Class) -> (NonZeroU64, NonZeroU64, &'c Name) {
    let Resources {
        cpu_millis,
        memory_bytes,
    } = class.resources;
    (memory_bytes, cpu_millis, name)
}

/// Why a role that has `needs` doesn't fit a class that gives a role `offers`: the first of its
/// needs, in the IR's order, that the class doesn't meet; or `None` if it fits.
fn misfit(needs: &Resources, offers: &Resources) -> Option<String> {
    let Resources {
        cpu_millis,
        memory_bytes,
    } = needs;
    if *cpu_millis > offers.cpu_millis {
        return Some(format!(
            "offers {} thousandths of a CPU, fewer than its {cpu_millis}",
            offers.cpu_millis
        ));
    }
    if *memory_bytes > offers.memory_bytes {
        return Some(format!(
            "offers {} bytes of memory, fewer than its {memory_bytes}",
            offers.memory_bytes
        ));
    }
    None
}
