//! Placing a parsed workflow's roles on providers, in classes of sandbox: [`Workflow::place`].

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::errors::{Errors, Reported};
use super::{
    At, ByProvider, ByRole, Class, Offers, ParseError, ParseErrors, Placed, Placement, Sandbox,
    Workflow,
};
use crate::ir::Name;
use crate::words;

mod misfit;

impl Workflow {
    /// Places each role in the smallest class that a provider of `offered`, by name, offers, as
    /// `classes` gives them: the least memory, then CPU, then the first by provider's and class's
    /// name; and gives back each provider it places a role on, dropping the others. A class fits
    /// a role whose CPU and memory it offers at least, every CPU flag it needs, and exactly the
    /// GPUs and CPU model it needs, so a class with GPUs, or a stated CPU model, goes only to roles
    /// that need them. A role that declares companions or assets fits only a class that holds them.
    ///
    /// Fails with each role that fits no class, [at](super::ParseError::at) its needs, or at its
    /// companions or assets if only classes without them fit. Each says why, and how to place it.
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
        let fitted: Vec<Result<&Placement, Reported>> = (self.roles())
            .map(|(role, sandbox)| {
                let runner = self.runners.get(&role);
                let companions = runner.map_or(0, |runner| runner.companions.len());
                smallest(&mut errors, sandbox, companions, &offers)
            })
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

/// The smallest of `offered` that `sandbox`'s role, with its `companions`, fits, as
/// [`Workflow::place`] says; otherwise reports why it fits none.
fn smallest<'p>(
    errors: &mut Errors,
    sandbox: &Sandbox,
    companions: usize,
    offered: &'p [Placement],
) -> Result<&'p Placement, Reported> {
    let Sandbox {
        name,
        needs,
        assets,
        at,
        ..
    } = sandbox;
    let (mut fitting, mut misfits) = (Vec::new(), Vec::new());
    for placement in offered {
        let unmet = misfit::unmet(needs, &placement.declared.offers);
        if unmet.is_empty() {
            fitting.push(placement);
        } else {
            misfits.push((placement, unmet));
        }
    }
    let accompanies =
        |placement: &&Placement| companions == 0 || placement.declared.offers.companions;
    let holds = |placement: &&Placement| assets.is_empty() || placement.declared.offers.assets;
    let holding = (fitting.iter()).filter(|placement| accompanies(placement) && holds(placement));
    if let Some(smallest) = holding.min_by_key(|placement| size(placement)) {
        return Ok(smallest);
    }
    let unheld = |classes: &str, change: &str| ParseError {
        at: At {
            ir: at.token("runtime").token("runner").token("companions"),
            source: None,
        },
        message: format!("role `{name}` declares companions, and no {classes} offers them"),
        label: Some(words::counted(companions, "companion")),
        why: Vec::new(),
        help: Vec::from(["declare none".to_owned(), change.to_owned()]),
    };
    // First, as no change of its needs could place it.
    let unoffered = (offered.iter()).all(|placement| !placement.declared.offers.companions);
    if companions > 0 && !offered.is_empty() && unoffered {
        let refused = unheld("class of sandbox", "run it where a class offers them");
        return Err(errors.add(refused));
    }
    let none_hold = (offered.iter()).all(|placement| !placement.declared.offers.assets);
    if !assets.is_empty() && !offered.is_empty() && none_hold {
        let change = "run it where a class holds them";
        return Err(errors.add(assets_unheld(sandbox, "class of sandbox", change)));
    }
    if fitting.is_empty() {
        let refused = misfit::refusal(name, needs, &at.token("needs"), &misfits);
        return Err(errors.add(refused));
    }
    // Some class fits its needs.
    if !fitting.iter().any(accompanies) {
        let change = "change its needs to fit a class that offers them";
        return Err(errors.add(unheld("class that fits its needs", change)));
    }
    // Some class fits its needs and holds its companions, if it declares any. So it declares
    // assets, which none of those holds.
    let classes = if companions == 0 {
        "class that fits its needs"
    } else {
        "class that fits its needs and holds its companions"
    };
    let change = "change its needs to fit a class that holds them";
    Err(errors.add(assets_unheld(sandbox, classes, change)))
}

/// The error that `sandbox`'s role declares assets that no class holds. `classes` says which
/// classes, such as "class that fits its needs". The help is to declare none, or `change`.
fn assets_unheld(sandbox: &Sandbox, classes: &str, change: &str) -> ParseError {
    let Sandbox {
        name, assets, at, ..
    } = sandbox;
    let paths: Vec<String> = (assets.iter())
        .map(|asset| format!("`{}`", asset.path))
        .collect();
    ParseError {
        at: At {
            ir: at.token("assets"),
            source: None,
        },
        message: format!("role `{name}` declares assets, and no {classes} holds them"),
        label: Some(format!(
            "{} at {}",
            words::counted(assets.len(), "asset"),
            paths.join(", ")
        )),
        why: Vec::new(),
        help: Vec::from(["declare none".to_owned(), change.to_owned()]),
    }
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
