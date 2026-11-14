//! Placing a parsed workflow's roles on providers, in classes of sandbox: [`Workflow::place`].

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use super::errors::{Errors, Reported};
use super::{ByProvider, ByRole, Class, Offers, ParseErrors, Placed, Placement, Sandbox, Workflow};
use crate::ir::{Gpu, Name, Needs};

impl Workflow {
    /// Places each role in the smallest class that a provider of `offered`, by name, offers, as
    /// `classes` gives them: the least memory, then CPU, then the first by provider's and class's
    /// name; and gives back each provider it places a role on. A class fits a role whose CPU and
    /// memory it offers at least, every CPU flag it needs, and exactly the GPUs and CPU model it
    /// needs, so a class with GPUs, or a stated CPU model, goes only to roles that need them.
    ///
    /// # Errors
    ///
    /// Returns each role that fits no class, [at](super::ParseError::at) its needs, saying why.
    pub fn place<'o, P>(
        self,
        offered: &'o BTreeMap<Name, P>,
        classes: impl Fn(&'o P) -> &'o BTreeMap<Name, Class>,
    ) -> Result<(Placed, ByProvider<&'o P>), ParseErrors> {
        let offered: Vec<(Placement, &P)> = (offered.iter())
            .flat_map(|(provider, offering)| {
                (classes(offering).iter()).map(move |(class, declared)| {
                    let placement = Placement {
                        provider: provider.clone(),
                        class: class.clone(),
                        declared: declared.clone(),
                    };
                    (placement, offering)
                })
            })
            .collect();
        let mut errors = Errors::default();
        // Every role is placed before any error is returned, so that each role that fits no
        // class is reported.
        let fitted: Vec<Result<&(Placement, &P), Reported>> = (self.roles.iter())
            .map(|sandbox| smallest(&mut errors, sandbox, &offered))
            .collect();
        let fitted: Vec<&(Placement, &P)> = errors.finish(fitted.into_iter().collect())?;
        let providers = (fitted.iter())
            .map(|(placement, offering)| (placement.provider.clone(), *offering))
            .collect();
        let placements = fitted.into_iter().map(|(placement, _)| placement.clone());
        let placed = Placed {
            workflow: self,
            placements: ByRole(placements.collect()),
        };
        Ok((placed, ByProvider(providers)))
    }
}

/// The smallest of `offered` that `sandbox`'s role fits, as [`Workflow::place`] says; otherwise
/// reports why each doesn't fit it, at the role's needs in the IR.
fn smallest<'p, P>(
    errors: &mut Errors,
    sandbox: &Sandbox,
    offered: &'p [(Placement, P)],
) -> Result<&'p (Placement, P), Reported> {
    let Sandbox {
        name, needs, at, ..
    } = sandbox;
    let (mut fits, mut misfits) = (Vec::new(), Vec::new());
    for offer in offered {
        let (placement, _) = offer;
        match misfit(needs, &placement.declared.offers) {
            None => fits.push(offer),
            Some(why) => misfits.push(format!(
                "`{}`'s `{}` {why}",
                placement.provider, placement.class
            )),
        }
    }
    let smallest = fits
        .into_iter()
        .min_by_key(|(placement, _)| size(placement));
    if let Some(smallest) = smallest {
        return Ok(smallest);
    }
    let why = if misfits.is_empty() {
        "no class is offered".to_owned()
    } else {
        misfits.join("; ")
    };
    Err(errors.report(
        at.token("needs"),
        format!(
            "`{name}` fits no class of sandbox, as {why}: offer a class that meets its needs, or \
             change its needs"
        ),
    ))
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

/// Why a role that has `needs` doesn't fit a class that `offers` them: the first of its needs, in
/// the IR's order, that the class doesn't meet; or `None` if it fits.
fn misfit(needs: &Needs, offers: &Offers) -> Option<String> {
    let Needs {
        cpu_millis,
        memory_bytes,
        gpu,
        cpu_features,
        cpu_model,
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
    match (gpu, &offers.gpu) {
        (None, None) => {}
        (Some(needed), Some(offered)) if needed == offered => {}
        (None, Some(offered)) => {
            return Some(format!("has GPUs, {}, and it needs none", gpus(offered)));
        }
        (Some(needed), None) => return Some(format!("has no GPU, and it needs {}", gpus(needed))),
        (Some(needed), Some(offered)) => {
            return Some(format!(
                "has GPUs, {}, and it needs {}",
                gpus(offered),
                gpus(needed)
            ));
        }
    }
    let lacking: Vec<String> = (cpu_features.difference(&offers.cpu_features))
        .map(|flag| format!("`{flag}`"))
        .collect();
    if !lacking.is_empty() {
        return Some(format!(
            "lacks the CPU flags {}, which it needs",
            lacking.join(", ")
        ));
    }
    match (cpu_model, &offers.cpu_model) {
        (None, None) => None,
        (Some(needed), Some(offered)) if needed == offered => None,
        (None, Some(offered)) => Some(format!(
            "states its CPU's model, `{offered}`, so it is offered only to roles that need that \
             model"
        )),
        (Some(needed), None) => Some(format!("states no CPU model, and it needs `{needed}`")),
        (Some(needed), Some(offered)) => Some(format!(
            "states the CPU model `{offered}`, and it needs `{needed}`"
        )),
    }
}

/// `gpu`, as a count and a model, such as ``4 × `NVIDIA H100 80GB HBM3` ``.
fn gpus(gpu: &Gpu) -> String {
    let Gpu { count, model } = gpu;
    format!("{count} × `{model}`")
}
