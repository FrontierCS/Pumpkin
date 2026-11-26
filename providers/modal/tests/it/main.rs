//! What the Modal provider decides without Modal. That is the references of a batch of images
//! (`images`), and the memory and companions that a sandbox holds (`create`). This binary compiles
//! both modules itself, as the crate keeps them private.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::num::{NonZeroU32, NonZeroU64};

use expect_test::{Expect, expect};
use provider::{ImageRef, Role};
use schema::ir::{Image, Name, Resources, User};

#[path = "../../src/create.rs"]
#[expect(
    dead_code,
    reason = "the tests call only the module's checks of a role"
)]
mod create;
#[path = "../../src/images.rs"]
mod images;

use images::Batch;

fn check(lines: impl IntoIterator<Item = String>, expect: &Expect) {
    let lines: Vec<String> = lines.into_iter().collect();
    expect.assert_eq(&cut(&lines.join("\n")));
}

/// The image whose digest is `digit`, 64 times.
fn image(digit: char) -> Image {
    Image::try_from(format!("sha256:{}", String::from(digit).repeat(64))).unwrap()
}

/// `text`, with each digest of [`image`] cut to its first digit.
fn cut(text: &str) -> String {
    ('a'..='f').fold(text.to_owned(), |text, digit| {
        text.replace(&String::from(digit).repeat(64), &String::from(digit))
    })
}

/// A runner's role, of 256 thousandths of a CPU and so many bytes of memory.
fn runner(memory_bytes: u64) -> Role {
    let id = NonZeroU32::new(1000).unwrap();
    Role {
        name: Name::try_from("runner".to_owned()).unwrap(),
        user: User { uid: id, gid: id },
        resources: Resources {
            cpu_millis: NonZeroU64::new(256).unwrap(),
            memory_bytes: NonZeroU64::new(memory_bytes).unwrap(),
        },
        instances: NonZeroU32::MIN,
        companions: 0,
        assets: Vec::new(),
    }
}

#[test]
fn memory() {
    let refused = create::memory_mib(&runner((256 << 20) + 1)).unwrap_err();
    check(
        [refused],
        &expect![
            "role `runner` asks for 268435457 bytes of memory, which Modal gives in whole MiB: ask for a multiple of 1048576"
        ],
    );
}

/// Whether a sandbox of a runner that needs `cpu_millis` thousandths of a CPU and `memory_mib` MiB
/// holds `companions`, or why not.
fn held(cpu_millis: u64, memory_mib: u64, companions: usize) -> String {
    let role = runner(memory_mib << 20);
    let resources = Resources {
        cpu_millis: NonZeroU64::new(cpu_millis).unwrap(),
        ..role.resources
    };
    let role = Role {
        resources,
        companions,
        ..role
    };
    let held = create::companions_held(&role).map_or_else(|refused| refused, |()| "held".into());
    format!("{cpu_millis}, {memory_mib} MiB, {companions}: {held}")
}

// Each count's edge, by the CPU, the memory and the cap. A rule off by one in any of them, or in
// its comparison, fails a case.
#[test]
fn companions() {
    check(
        [
            held(500, 1024, 15),
            held(500, 1024, 16),
            held(512, 1024, 16),
            held(8000, 1000, 31),
            held(8000, 1000, 32),
            held(16_000, 128 << 10, 250),
            held(16_000, 128 << 10, 251),
            held(500, 512, 17),
            held(500, 1024, 251),
            held(16, 1024, 1),
        ],
        &expect![[r#"
            500, 1024 MiB, 15: held
            500, 1024 MiB, 16: role `runner` declares 16 companions, and a Modal sandbox holds 15 for needs of 500 thousandths of a CPU and 1 GiB: need at least 512 thousandths of a CPU, or declare at most 15
            512, 1024 MiB, 16: held
            8000, 1000 MiB, 31: held
            8000, 1000 MiB, 32: role `runner` declares 32 companions, and a Modal sandbox holds 31 for needs of 8000 thousandths of a CPU and 1000 MiB: need at least 1 GiB, or declare at most 31
            16000, 131072 MiB, 250: held
            16000, 131072 MiB, 251: role `runner` declares 251 companions, and a Modal sandbox holds at most 250: declare at most 250
            500, 512 MiB, 17: role `runner` declares 17 companions, and a Modal sandbox holds 15 for needs of 500 thousandths of a CPU and 512 MiB: need at least 544 thousandths of a CPU and 544 MiB, or declare at most 15
            500, 1024 MiB, 251: role `runner` declares 251 companions, and a Modal sandbox holds 15 for needs of 500 thousandths of a CPU and 1 GiB: declare at most 15
            16, 1024 MiB, 1: role `runner` declares 1 companion, and a Modal sandbox holds 0 for needs of 16 thousandths of a CPU and 1 GiB: need at least 32 thousandths of a CPU, or declare none"#]],
    );
}

#[test]
fn batches() {
    let images = [image('b'), image('a'), image('b'), image('c')];
    let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    let distinct: Vec<String> = Batch::of(&images)
        .distinct()
        .map(Image::to_string)
        .collect();
    let references = Batch::of(&images).references(ids(&["im-1", "im-2", "im-3"]));
    let references = references.unwrap();
    let references: Vec<&str> = references.iter().map(ImageRef::as_str).collect();
    let cases = [
        format!("imported once each: {}", distinct.join(" ")),
        format!("referenced: {}", references.join(" ")),
    ];
    check(
        cases,
        &expect![[r#"
        imported once each: sha256:a sha256:b sha256:c
        referenced: im-2@sha256:b im-1@sha256:a im-2@sha256:b im-3@sha256:c"#]],
    );
}
