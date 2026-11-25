//! Placing a parsed workflow's roles on providers, in classes of sandbox: [`Workflow::place`].

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::errors::{Errors, Reported};
use super::{
    ByProvider, ByRole, Class, Companions, Offers, ParseErrors, Placed, Placement, Sandbox,
    Workflow,
};
use crate::ir::{Name, Needs};
use crate::words;

mod misfit;

impl Workflow {
    /// Places each role in the smallest class that a provider of `offered`, by name, offers, as
    /// `classes` gives them: the least memory, then CPU, then the first by provider's and class's
    /// name; and gives back each provider it places a role on, dropping the others. A class fits
    /// a role whose CPU and memory it offers at least, every CPU flag it needs, and exactly the
    /// GPUs and CPU model it needs, so a class with GPUs, or a stated CPU model, goes only to roles
    /// that need them. A runner fits only a class that holds all its companions for its needs.
    ///
    /// Fails with each role that fits no class, [at](super::ParseError::at) its needs. Each error
    /// says what each class offers short of them, and the changes that would place the role. A
    /// runner whose companions no class that fits its needs holds fails at its companions.
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
        name, needs, at, ..
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
    let holding = (fitting.iter().copied())
        .filter(|placement| companions <= placement.declared.offers.companions.holds(needs));
    if let Some(smallest) = holding.min_by_key(|placement| size(placement)) {
        return Ok(smallest);
    }
    let declared = words::counted(companions, "companion");
    let at_companions = at.token("runtime").token("runner").token("companions");
    // First, as no change of its needs could place it.
    let unoffered =
        (offered.iter()).all(|placement| placement.declared.offers.companions == Companions::None);
    if companions > 0 && !offered.is_empty() && unoffered {
        return Err(errors.report(
            at_companions,
            format!(
                "`{name}` declares {declared}, and no class of sandbox offers companions: declare \
                 none, or run it where a class offers them"
            ),
        ));
    }
    if fitting.is_empty() {
        let refused = misfit::refusal(name, needs, &at.token("needs"), &misfits);
        return Err(errors.add(refused));
    }
    // Some class fits its needs, so it declares companions that none of them holds.
    let short: Vec<String> = (fitting.iter())
        .filter_map(|placement| {
            let why = short_of(needs, companions, &placement.declared.offers)?;
            Some(format!(
                "`{}`'s `{}` {why}",
                placement.provider, placement.class
            ))
        })
        .collect();
    let message = if short.is_empty() {
        format!(
            "`{name}` declares {declared}, and no class that fits its needs offers companions: \
             declare none, or change its needs to fit a class that offers them"
        )
    } else {
        format!(
            "`{name}` declares {declared}, more than any class that fits its needs holds: {}",
            short.join("; ")
        )
    };
    Err(errors.report(at_companions, message))
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

impl Companions {
    /// How many companions a sandbox of the class holds for a role that needs `needs`.
    fn holds(self, needs: &Needs) -> usize {
        match self {
            Self::None => 0,
            Self::Reserved {
                cpu_millis_each,
                memory_bytes_each,
                most,
            } => {
                let by_cpu = needs.cpu_millis.get() / cpu_millis_each.get();
                let by_memory = needs.memory_bytes.get() / memory_bytes_each.get();
                // A count past `usize::MAX` is past `most` too.
                let held = usize::try_from(by_cpu.min(by_memory)).unwrap_or(usize::MAX);
                held.min(most.get())
            }
        }
    }
}

/// How few companions a class that `offers` them holds for a role that has `needs` and declares
/// more, `declared`. It says what would make them fit, and is `None` if the class offers none.
fn short_of(needs: &Needs, declared: usize, offers: &Offers) -> Option<String> {
    let Companions::Reserved {
        cpu_millis_each,
        memory_bytes_each,
        most,
    } = offers.companions
    else {
        return None;
    };
    let held = offers.companions.holds(needs);
    let fewer = match held {
        0 => "declare none".to_owned(),
        _ => format!("declare at most {held}"),
    };
    if held == most.get() {
        return Some(format!("holds at most {most}, so {fewer}"));
    }
    let fact = format!(
        "holds {held} for needs of {} thousandths of a CPU and {}",
        needs.cpu_millis,
        binary(needs.memory_bytes.get())
    );
    // In `u128`, where no product of a count and an amount overflows.
    let each = |amount: NonZeroU64| declared as u128 * u128::from(amount.get());
    let (cpu_millis, memory_bytes) = (each(cpu_millis_each), each(memory_bytes_each));
    // Only what falls short, and only within what the class offers, so that the need still fits it.
    let cpu = (cpu_millis > u128::from(needs.cpu_millis.get())).then_some(cpu_millis);
    let memory = (memory_bytes > u128::from(needs.memory_bytes.get())).then_some(memory_bytes);
    let within = cpu.is_none_or(|cpu_millis| cpu_millis <= u128::from(offers.cpu_millis.get()))
        && memory.is_none_or(|memory_bytes| memory_bytes <= u128::from(offers.memory_bytes.get()));
    let more = match (cpu, memory) {
        _ if declared > most.get() || !within => None,
        (Some(cpu_millis), None) => Some(format!("{cpu_millis} thousandths of a CPU")),
        (None, Some(memory_bytes)) => Some(binary(memory_bytes)),
        (Some(cpu_millis), Some(memory_bytes)) => Some(format!(
            "{cpu_millis} thousandths of a CPU and {}",
            binary(memory_bytes)
        )),
        // `held`, below `most`, is the fewer of the counts that the CPU and the memory give, each
        // rounded down. So `declared`, which is more, takes more than the role needs of one.
        (None, None) => {
            unreachable!("a class short of companions below its cap is short of CPU or memory")
        }
    };
    Some(match more {
        Some(more) => format!("{fact}, so need at least {more}, or {fewer}"),
        None => format!("{fact}, so {fewer}"),
    })
}

/// `bytes` exactly, in the largest binary unit that divides it, as "3 GiB" or "1536 MiB".
fn binary(bytes: impl Into<u128>) -> String {
    let bytes = bytes.into();
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    match units
        .into_iter()
        .find(|(unit, _)| bytes.is_multiple_of(*unit))
    {
        Some((unit, name)) => format!("{} {name}", bytes / unit),
        None => words::counted(bytes, "byte"),
    }
}
