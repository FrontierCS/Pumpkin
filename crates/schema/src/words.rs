//! How Pumpkin's programs word a size and a count, so that every message says them alike.

use std::fmt;

/// `bytes` in the largest binary unit that holds at least one of it, to a tenth, as "16.0 MiB".
/// Below a KiB, it counts the bytes. Every size that Pumpkin says is said so.
pub fn size(bytes: u64) -> String {
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    let Some((unit, name)) = units.into_iter().find(|(unit, _)| bytes >= *unit) else {
        return counted(bytes, "byte");
    };
    let tenths = bytes * 10 / unit;
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}

/// `count` and `noun`, plural unless `count` is 1, as "1 problem", "2 problems", "2 entries" or
/// "2 sandboxes". Every count of a thing that Pumpkin says is said so.
pub fn counted<C: Copy + fmt::Display + PartialEq + From<u8>>(count: C, noun: &str) -> String {
    if count == C::from(1) {
        return format!("{count} {noun}");
    }
    let consonant_y = noun
        .strip_suffix('y')
        .filter(|stem| !stem.ends_with(['a', 'e', 'o', 'u']));
    match consonant_y {
        Some(stem) => format!("{count} {stem}ies"),
        None if noun.ends_with(['s', 'x']) || noun.ends_with("sh") || noun.ends_with("ch") => {
            format!("{count} {noun}es")
        }
        None => format!("{count} {noun}s"),
    }
}
