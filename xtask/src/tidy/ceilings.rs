//! The caps on what each file of a kind holds, and the ceilings of the files over one.

use std::collections::BTreeMap;

use crate::Result;
use crate::repo::Repo;

// The files over a cap, each with its ceiling, the most it may hold, which only goes down: a
// ratchet, as TigerBeetle's tidy holds the functions that are already long and stops the others
// growing past its cap, and fails on an exception that names no file:
// https://github.com/tigerbeetle/tigerbeetle/blob/24d92bff033453dcc757adfcd9aa14655d1b1734/src/tidy.zig#L731-L753
// https://github.com/tigerbeetle/tigerbeetle/blob/24d92bff033453dcc757adfcd9aa14655d1b1734/src/tidy.zig#L1496-L1508
const CEILINGS: &str = ".config/tidy.toml";

/// A cap on what each file of a kind holds, whose exceptions are its table in [`CEILINGS`].
pub(super) struct Cap {
    pub(super) table: &'static str,
    /// The files it applies to, as a problem names a file that isn't one.
    pub(super) kind: &'static str,
    pub(super) unit: [&'static str; 2],
    pub(super) limit: usize,
    /// What to do with a file over the cap.
    pub(super) fix: &'static str,
}

pub(super) fn ceilings(repo: &Repo<'_>) -> Result<toml::Table> {
    if !repo.has(CEILINGS) {
        return Ok(toml::Table::new());
    }
    Ok(toml::from_str(&repo.read(CEILINGS)?)
        .map_err(|error| format!("{CEILINGS} isn't TOML: {error}"))?)
}

fn ceilings_of(ceilings: &toml::Table, table: &str, unit: &str) -> Result<BTreeMap<String, usize>> {
    let listed = ceilings.get(table).and_then(toml::Value::as_table);
    let mut read = BTreeMap::new();
    for (path, ceiling) in listed.into_iter().flatten() {
        let ceiling = ceiling
            .as_integer()
            .and_then(|ceiling| usize::try_from(ceiling).ok());
        let Some(ceiling) = ceiling else {
            return Err(format!(
                "{CEILINGS}: `{path}`'s ceiling isn't a count of {unit}: give it the file's"
            )
            .into());
        };
        read.insert(path.clone(), ceiling);
    }
    Ok(read)
}

/// Each file with its count of a cap's unit, and the problem of each where the unit is a problem.
pub(super) type Counts = Vec<(String, usize, Vec<String>)>;

/// The problems of `counts`, of `cap`'s kind. A file over the cap that `cap`'s table of `ceilings`
/// doesn't list, or one past its ceiling there, is a problem, and the problems of its unit follow
/// it. So is an entry of the table that is stale: its file gone, within the cap or under its
/// ceiling.
pub(super) fn capped(ceilings: &toml::Table, cap: &Cap, counts: Counts) -> Result<Vec<String>> {
    let Cap {
        table,
        kind,
        unit,
        limit: cap,
        fix,
    } = *cap;
    let mut ceilings = ceilings_of(ceilings, table, unit[1])?;
    let mut problems = Vec::new();
    for (path, count, each) in counts {
        let unit = unit[usize::from(count != 1)];
        let file = format!("{path}: the file has {count} {unit}");
        let ceiling = ceilings.remove(&path);
        let over = count > ceiling.unwrap_or(cap);
        let problem = match ceiling {
            None if over => format!("{file}, over the cap of {cap}: {fix}"),
            Some(ceiling) if over => format!(
                "{file}, over the cap of {cap} and its ceiling of {ceiling} in {CEILINGS}, which \
                 only goes down: {fix}"
            ),
            Some(_) if count <= cap => {
                format!("{file}, within the cap of {cap}: take it off {CEILINGS}")
            }
            Some(ceiling) if count < ceiling => format!(
                "{file}, under its ceiling of {ceiling} in {CEILINGS}: lower the ceiling to {count}"
            ),
            None | Some(_) => continue,
        };
        problems.push(problem);
        problems.extend(each.into_iter().filter(|_| over));
    }
    for path in ceilings.keys() {
        problems.push(format!(
            "{CEILINGS}: `{path}` isn't a {kind} of the repository: take it off the list"
        ));
    }
    Ok(problems)
}
