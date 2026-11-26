//! Placing a parsed workflow's roles on providers, in classes of sandbox: [`Workflow::place`].

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::errors::{Errors, Reported};
use super::{ByProvider, ByRole, Class, Offers, ParseErrors, Placed, Placement, Sandbox, Workflow};
use crate::ir::Name;

mod misfit;

impl Workflow {
    /// Places each role in the smallest class that a provider of `offered`, by name, offers, as
    /// `classes` gives them: the least memory, then CPU, then the first by provider's and class's
    /// name; and gives back each provider it places a role on, dropping the others. A class fits
    /// a role whose CPU and memory it offers at least, every CPU flag it needs, and exactly the
    /// GPUs and CPU model it needs, so a class with GPUs, or a stated CPU model, goes only to roles
    /// that need them.
    ///
    /// Fails with each role that fits no class, [at](super::ParseError::at) its needs. Each error
    /// says what each class offers short of them, and the changes that would place the role.
    pub fn place<P>(
        self,
        offered: BTreeMap<Name, P>,
        classes: impl Fn(&P) -> &BTreeMap<Name, Class>,
    ) -> Result<(Placed, ByProvider<P>), ParseErrors> {
        let offers: Vec<Placement> = (offered.iter())
            .flat_map(|(provider, offering)| {
                (classes(offering).iter()).map(move |(class, declared)| Placement {
                    provider: provider.clone(),
                    class: class.clone(),
                    declared: declared.clone(),
                })
            })
            .collect();
        let mut errors = Errors::default();
        // Every role is placed before any error is returned, so that each role that fits no
        // class is reported.
        let fitted: Vec<Result<&Placement, Reported>> = (self.roles.iter())
            .map(|sandbox| smallest(&mut errors, sandbox, &offers))
            .collect();
        let fitted: Vec<&Placement> = errors.finish(fitted.into_iter().collect())?;
        let placed_on: BTreeSet<&Name> =
            fitted.iter().map(|placement| &placement.provider).collect();
        let providers = (offered.into_iter())
            .filter(|(provider, _)| placed_on.contains(provider))
            .collect();
        let placements = fitted.into_iter().cloned();
        let placed = Placed {
            workflow: self,
            placements: ByRole(placements.collect()),
        };
        Ok((placed, ByProvider(providers)))
    }
}

/// The smallest of `offered` that `sandbox`'s role fits, as [`Workflow::place`] says; otherwise
/// reports why each doesn't fit it, at the role's needs in the IR.
fn smallest<'p>(
    errors: &mut Errors,
    sandbox: &Sandbox,
    offered: &'p [Placement],
) -> Result<&'p Placement, Reported> {
    let Sandbox {
        name, needs, at, ..
    } = sandbox;
    let (mut fits, mut misfits) = (Vec::new(), Vec::new());
    for placement in offered {
        let unmet = misfit::unmet(needs, &placement.declared.offers);
        if unmet.is_empty() {
            fits.push(placement);
        } else {
            misfits.push((placement, unmet));
        }
    }
    let smallest = fits.into_iter().min_by_key(|placement| size(placement));
    if let Some(smallest) = smallest {
        return Ok(smallest);
    }
    let refused = misfit::refusal(name, needs, &at.token("needs"), &misfits);
    Err(errors.add(refused))
}

/// How [`Workflow::place`] orders the classes that a role fits, smallest first.
// Not by GPUs: every class that a role fits has exactly the GPUs it needs.
fn size(placement: &Placement) -> (NonZeroU64, NonZeroU64, &Name, &Name) {
    let Offers {
        cpu_millis,
        memory_bytes,
        ..
    } = &placement.declared.offers;
    (
        *memory_bytes,
        *cpu_millis,
        &placement.provider,
        &placement.class,
    )
}
