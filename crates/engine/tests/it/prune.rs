//! Which of an engine's images a prune removes, without Docker: none until the disk is full;
//! then, of those that live runs and builds don't hold, by their IDs or tags, nor, while a build
//! is under way, untagged, the least recently used, until their sizes bring the disk's use down
//! to the low mark; and whether a build is refused, past the refusal mark.

use std::collections::BTreeSet;

use engine::prune::{self, Cached, Disk, Held};
use expect_test::{Expect, expect};
use schema::ir::Image;

fn image(digit: char) -> Image {
    Image::try_from(format!("sha256:{}", digit.to_string().repeat(64))).unwrap()
}

fn tag(digit: char) -> String {
    format!("pumpkin/task:{}", digit.to_string().repeat(64))
}

/// A case: its name; the disk's size and free bytes; each image, by the digit of its ID, with
/// its size and its last use, or none, tagged as [`tag`] says unless untagged; the digits of
/// the images held by their IDs, as live runs name them; those of the images whose tags live
/// builds hold; and whether a build is under way.
type Case<'a> = (
    &'a str,
    (u64, u64),
    &'a [(char, u64, Option<u32>)],
    &'a str,
    &'a str,
    bool,
);

fn check(cases: &[Case<'_>], untagged: char, expect: &Expect) {
    let mut lines = Vec::new();
    for &(name, (size_bytes, free_bytes), images, by_id, by_tag, building) in cases {
        let disk = Disk {
            size_bytes,
            free_bytes,
        };
        let cached = (images.iter())
            .map(|&(digit, size_bytes, _)| Cached {
                image: image(digit),
                tags: (digit != untagged)
                    .then(|| tag(digit))
                    .into_iter()
                    .collect(),
                size_bytes,
            })
            .collect();
        let last_used = |used: &Image| {
            let found = images.iter().find(|&&(digit, ..)| image(digit) == *used);
            found.and_then(|&(_, _, last_used)| last_used)
        };
        let held = Held {
            images: by_id.chars().map(image).collect::<BTreeSet<Image>>(),
            tags: by_tag.chars().map(tag).collect(),
            building,
        };
        let evicted = prune::evicted(disk, cached, last_used, &held);
        let digits: Vec<String> = (evicted.iter())
            .map(|image| image.as_str()["sha256:".len()..][..1].to_owned())
            .collect();
        let removed = if digits.is_empty() {
            "nothing".to_owned()
        } else {
            digits.join(" ")
        };
        let refused = if disk.refused() {
            "; a build is refused"
        } else {
            ""
        };
        let shown = if disk.full() {
            let free = disk.low_free_bytes();
            format!("{name}: removes {removed}; the build cache down to {free} bytes free{refused}")
        } else {
            format!("{name}: not full, removes {removed}{refused}")
        };
        lines.push(shown);
    }
    expect.assert_eq(&lines.join("\n"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn evicted() {
    let untagged: &[(char, u64, Option<u32>)] = &[('e', 30_000, None)];
    let four: &[(char, u64, Option<u32>)] = &[
        ('d', 10_000, Some(3)),
        ('c', 10_000, Some(2)),
        ('b', 10_000, Some(1)),
        ('a', 10_000, None),
    ];
    check(
        &[
            ("at the high mark", (100_000, 20_000), four, "", "", false),
            ("past it", (100_000, 19_000), four, "", "", false),
            ("past it, b live", (100_000, 19_000), four, "b", "", false),
            (
                "past it, a and c held",
                (100_000, 19_000),
                four,
                "",
                "ac",
                false,
            ),
            (
                "past it, a live, b held",
                (100_000, 19_000),
                four,
                "a",
                "b",
                false,
            ),
            (
                "at the refusal mark",
                (100_000, 15_000),
                four,
                "",
                "",
                false,
            ),
            ("past it", (100_000, 14_000), four, "", "", false),
            ("all used, but a live", (100_000, 0), four, "bcd", "", false),
            ("all used, all held", (100_000, 0), four, "", "abcd", false),
            (
                "held, but untagged",
                (100_000, 19_000),
                untagged,
                "",
                "e",
                false,
            ),
            (
                "untagged, held by its ID",
                (100_000, 19_000),
                untagged,
                "e",
                "",
                false,
            ),
            (
                "untagged, while a build is under way",
                (100_000, 19_000),
                untagged,
                "",
                "",
                true,
            ),
            (
                "tagged, while a build is under way",
                (100_000, 19_000),
                four,
                "",
                "",
                true,
            ),
            (
                "ties",
                (100_000, 19_000),
                &[
                    ('f', 30_000, Some(1)),
                    ('e', 5_000, Some(1)),
                    ('c', 5_000, None),
                    ('a', 5_000, None),
                ],
                "",
                "",
                false,
            ),
            (
                "one large",
                (100_000, 19_000),
                &[('b', 1_000, None), ('a', 50_000, None)],
                "",
                "",
                false,
            ),
            ("no images", (100_000, 0), &[], "", "", false),
        ],
        'e',
        &expect![[r#"
            at the high mark: not full, removes nothing
            past it: removes a b c; the build cache down to 40000 bytes free
            past it, b live: removes a c d; the build cache down to 40000 bytes free
            past it, a and c held: removes b d; the build cache down to 40000 bytes free
            past it, a live, b held: removes c d; the build cache down to 40000 bytes free
            at the refusal mark: removes a b c; the build cache down to 40000 bytes free
            past it: removes a b c; the build cache down to 40000 bytes free; a build is refused
            all used, but a live: removes a; the build cache down to 40000 bytes free; a build is refused
            all used, all held: removes nothing; the build cache down to 40000 bytes free; a build is refused
            held, but untagged: removes e; the build cache down to 40000 bytes free
            untagged, held by its ID: removes nothing; the build cache down to 40000 bytes free
            untagged, while a build is under way: removes nothing; the build cache down to 40000 bytes free
            tagged, while a build is under way: removes a b c; the build cache down to 40000 bytes free
            ties: removes a c e f; the build cache down to 40000 bytes free
            one large: removes a; the build cache down to 40000 bytes free
            no images: removes nothing; the build cache down to 40000 bytes free; a build is refused"#]],
    );
}
