//! Why a role fits no class of sandbox, and the changes that would place it, as
//! [`Workflow::place`](crate::workflow::Workflow::place) refuses it.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::num::NonZeroU64;

use crate::ir::{CpuFlag, Gpu, JsonPointer, MILLIS_PER_CPU, Name, Needs};
use crate::words;
use crate::workflow::{At, Offers, ParseError, Placement};

/// A need of a role, which a class may not meet, in the IR's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Need {
    Cpu,
    Memory,
    Gpu,
    CpuFeatures,
    CpuModel,
}

/// Each of `needs` that a class that `offers` them doesn't meet, in the IR's order: none if the
/// class fits a role that has them.
pub(super) fn unmet(needs: &Needs, offers: &Offers) -> Vec<Need> {
    let Needs {
        cpu_millis,
        memory_bytes,
        gpu,
        cpu_features,
        cpu_model,
    } = needs;
    let met = [
        (Need::Cpu, *cpu_millis <= offers.cpu_millis),
        (Need::Memory, *memory_bytes <= offers.memory_bytes),
        (Need::Gpu, *gpu == offers.gpu),
        (
            Need::CpuFeatures,
            cpu_features.is_subset(&offers.cpu_features),
        ),
        (Need::CpuModel, *cpu_model == offers.cpu_model),
    ];
    (met.into_iter())
        .filter_map(|(need, met)| (!met).then_some(need))
        .collect()
}

/// That the role `name`, which has `needs` at `at`, fits none of `misfits`. Each of those is a
/// class with what it doesn't meet of them. The error says what each class offers there, and the
/// changes that would place the role.
pub(super) fn refusal(
    name: &Name,
    needs: &Needs,
    at: &JsonPointer,
    misfits: &[(&Placement, Vec<Need>)],
) -> ParseError {
    let mut why: Vec<String> = (misfits.iter())
        .map(|(placement, unmet)| {
            let offers = &placement.declared.offers;
            let offered: Vec<String> = (unmet.iter())
                .map(|need| offered(*need, needs, offers))
                .collect();
            format!("{}: {}", class(placement), offered.join(", "))
        })
        .collect();
    if why.is_empty() {
        why.push("no class is offered".to_owned());
    }
    let mut help = Vec::from([format!("add a class with {}", wanted(needs))]);
    help.extend(changes(needs, misfits));
    ParseError {
        at: At {
            ir: at.clone(),
            source: None,
        },
        message: format!("role `{name}` fits no class of sandbox"),
        label: Some(stated(needs)),
        why,
        help,
    }
}

/// `needs` as a role states them, such as ``16 CPUs, 64.0 GiB, 1 × `NVIDIA H100 80GB HBM3` ``.
fn stated(needs: &Needs) -> String {
    let Needs {
        cpu_millis,
        memory_bytes,
        gpu,
        cpu_features,
        cpu_model,
    } = needs;
    let mut stated = Vec::from([cpus(*cpu_millis), words::size(memory_bytes.get())]);
    stated.extend(gpu.as_ref().map(gpus));
    if !cpu_features.is_empty() {
        stated.push(format!("the CPU flags {}", flags(cpu_features)));
    }
    stated.extend(cpu_model.as_ref().map(|model| format!("CPU `{model}`")));
    stated.join(", ")
}

/// What a class that a role with `needs` fits has: exactly its GPUs, CPU model and CPU flags,
/// and at least its CPU and memory.
fn wanted(needs: &Needs) -> String {
    let Needs {
        cpu_millis,
        memory_bytes,
        gpu,
        cpu_features,
        cpu_model,
    } = needs;
    let mut exact: Vec<String> = gpu.iter().map(gpus).collect();
    if !cpu_features.is_empty() {
        exact.push(format!("the CPU flags {}", flags(cpu_features)));
    }
    exact.extend(cpu_model.iter().map(|model| format!("CPU `{model}`")));
    let least = format!(
        "at least {} and {}",
        cpus(*cpu_millis),
        words::size(memory_bytes.get())
    );
    if exact.is_empty() {
        least
    } else {
        format!("{}, and {least}", exact.join(", "))
    }
}

/// What a class that `offers` them has of `need`, which it doesn't meet for a role that has
/// `needs`.
fn offered(need: Need, needs: &Needs, offers: &Offers) -> String {
    match need {
        Need::Cpu => cpus(offers.cpu_millis),
        Need::Memory => words::size(offers.memory_bytes.get()),
        Need::Gpu => match (&needs.gpu, &offers.gpu) {
            (_, None) => "no GPU".to_owned(),
            (None, Some(offered)) => format!("{} (only for roles that need it)", gpus(offered)),
            (Some(_), Some(offered)) => gpus(offered),
        },
        Need::CpuFeatures => {
            let lacking = needs.cpu_features.difference(&offers.cpu_features);
            format!("without {}", flags(lacking))
        }
        Need::CpuModel => match (&needs.cpu_model, &offers.cpu_model) {
            (_, None) => "no stated CPU model".to_owned(),
            (None, Some(offered)) => format!("CPU `{offered}` (only for roles that need it)"),
            (Some(_), Some(offered)) => format!("CPU `{offered}`"),
        },
    }
}

/// The changes of one of `needs` that would each place the role in a class of `misfits`. Each of
/// those is a class with what it doesn't meet of them.
// Only to less CPU or memory, or to another GPU or CPU model. Dropping or adding a GPU, a CPU
// flag or a CPU model changes what the role's code runs on. Only its author knows that it can.
fn changes(needs: &Needs, misfits: &[(&Placement, Vec<Need>)]) -> Vec<String> {
    let mut by_need: BTreeMap<Need, Vec<&Placement>> = BTreeMap::new();
    for (placement, unmet) in misfits {
        if let [need] = unmet[..] {
            by_need.entry(need).or_default().push(*placement);
        }
    }
    let mut changes = Vec::new();
    for (need, placements) in by_need {
        match need {
            Need::Cpu => {
                let most = (placements.iter().copied())
                    .min_by_key(|placement| Reverse(placement.declared.offers.cpu_millis));
                changes.extend(most.map(|most| {
                    format!(
                        "lower the role's `cpu_millis` to {}, which {} offers",
                        most.declared.offers.cpu_millis,
                        class(most)
                    )
                }));
            }
            Need::Memory => {
                let most = (placements.iter().copied())
                    .min_by_key(|placement| Reverse(placement.declared.offers.memory_bytes));
                changes.extend(most.map(|most| {
                    let bytes = most.declared.offers.memory_bytes;
                    format!(
                        "lower the role's `memory_bytes` to {bytes} ({}), which {} offers",
                        words::size(bytes.get()),
                        class(most)
                    )
                }));
            }
            Need::Gpu if needs.gpu.is_some() => {
                let mut seen = Vec::new();
                for placement in placements {
                    let Some(gpu) = &placement.declared.offers.gpu else {
                        continue;
                    };
                    if !seen.contains(&gpu) {
                        seen.push(gpu);
                        changes.push(format!(
                            "change the role's `gpu` to {}, which {} has",
                            gpus(gpu),
                            class(placement)
                        ));
                    }
                }
            }
            Need::CpuModel if needs.cpu_model.is_some() => {
                let mut seen = Vec::new();
                for placement in placements {
                    let Some(model) = &placement.declared.offers.cpu_model else {
                        continue;
                    };
                    if !seen.contains(&model) {
                        seen.push(model);
                        changes.push(format!(
                            "change the role's `cpu_model` to `{model}`, which {} states",
                            class(placement)
                        ));
                    }
                }
            }
            Need::Gpu | Need::CpuFeatures | Need::CpuModel => {}
        }
    }
    changes
}

/// `placement`'s class, by its provider's name and its own, such as `` `aws/small` ``.
fn class(placement: &Placement) -> String {
    format!("`{}/{}`", placement.provider, placement.class)
}

/// `millis` thousandths of a CPU, as "1 CPU", "16 CPUs" or "0.5 CPUs".
fn cpus(millis: NonZeroU64) -> String {
    let (whole, thousandths) = (millis.get() / MILLIS_PER_CPU, millis.get() % MILLIS_PER_CPU);
    if thousandths == 0 {
        return words::counted(whole, "CPU");
    }
    let fraction = format!("{thousandths:03}");
    format!("{whole}.{} CPUs", fraction.trim_end_matches('0'))
}

/// `gpu`, as a count and a model, such as ``4 × `NVIDIA H100 80GB HBM3` ``.
fn gpus(gpu: &Gpu) -> String {
    let Gpu { count, model } = gpu;
    format!("{count} × `{model}`")
}

/// `flags`, each in backticks, such as ``` `avx2`, `avx512f` ```.
fn flags<'f>(flags: impl IntoIterator<Item = &'f CpuFlag>) -> String {
    let flags: Vec<String> = flags.into_iter().map(|flag| format!("`{flag}`")).collect();
    flags.join(", ")
}
