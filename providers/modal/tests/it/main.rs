//! What the Modal provider decides without Modal. That is the references of a batch of images
//! (`images`), what its helper's `create` is asked, and how its answer is read (`create`). So is
//! how many companions a sandbox holds (`create`). This binary compiles both modules itself, as
//! the crate keeps them private.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::net::Ipv4Addr;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use expect_test::{Expect, expect};
use provider::{CompanionDial, Create, ImageRef, ListenDials, Role, RunTag, SandboxdRef, UnixTime};
use schema::ir::{CompanionName, Image, Name, Resources, User};
use schema::sandbox::{Listening, RegistrationGrant, Relayed, TlsIdentity};
use serde_json::{Value, json};

#[path = "../../src/create.rs"]
mod create;
#[path = "../../src/images.rs"]
mod images;

use create::Made;
use images::Batch;

fn check(lines: impl IntoIterator<Item = String>, expect: &Expect) {
    let lines: Vec<String> = lines.into_iter().collect();
    expect.assert_eq(&lines.join("\n"));
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

fn user(id: u32) -> User {
    let id = NonZeroU32::new(id).unwrap();
    User { uid: id, gid: id }
}

fn name(name: &str) -> Name {
    Name::try_from(name.to_owned()).unwrap()
}

/// A create of a runner's sandbox, of 256 thousandths of a CPU and so many bytes of memory.
fn runner(memory_bytes: u64, ops: bool, model: bool) -> Create {
    let resources = Resources {
        cpu_millis: NonZeroU64::new(256).unwrap(),
        memory_bytes: NonZeroU64::new(memory_bytes).unwrap(),
    };
    Create {
        run: RunTag::try_from("run-1".to_owned()).unwrap(),
        name: name("lease-0"),
        role: Role {
            name: name("runner"),
            user: user(1000),
            resources,
            instances: NonZeroU32::MIN,
            companions: 0,
        },
        instance: 0,
        class: name("cpu"),
        image: ImageRef::from(format!("im-main@{}", image('a'))),
        sandboxd: SandboxdRef::from("/opt/pumpkin/sandboxd@sha256:5d".to_owned()),
        ops,
        model,
        hard_end: UnixTime::after_epoch(Duration::from_secs(2_000_003_600)),
        by: UnixTime::after_epoch(Duration::from_secs(2_000_000_600)),
    }
}

/// A companion's dial: its user's id is `id`, and its grant and identity are made of `id` too.
fn companion(name: &str, image: &str, id: u8) -> CompanionDial {
    CompanionDial {
        name: CompanionName::try_from(name.to_owned()).unwrap(),
        image: ImageRef::from(image.to_owned()),
        user: user(u32::from(id)),
        relayed: Relayed {
            grant: RegistrationGrant::from([id; 16]),
            identity: TlsIdentity::new(vec![0xce, id], vec![0x5e, id]),
        },
    }
}

fn dials(companions: Vec<CompanionDial>) -> ListenDials {
    let grant = RegistrationGrant::from([1; 16]);
    ListenDials {
        main: Listening { grant },
        companions,
    }
}

/// The helper's request for `create` with `dials`, or why there is none.
fn request(create: &Create, dials: &ListenDials, gpu: Option<&str>) -> Result<Value, String> {
    let source = Ipv4Addr::new(203, 0, 113, 7);
    let request = create::request(create, dials, gpu, source).map_err(|refused| cut(&refused))?;
    Ok(serde_json::to_value(request).unwrap())
}

/// `request`, a line for each key whose value isn't `first`'s, and one for each sidecar. Or why
/// there is none.
fn asked(case: &str, request: Result<Value, String>, first: &Value) -> String {
    let mut request = match request {
        Ok(request) => request,
        Err(refused) => return format!("{case}: {refused}"),
    };
    let sidecars = request["sidecars"].take();
    let keys = (request.as_object().unwrap().iter())
        .filter(|(key, value)| *key != "sidecars" && first.get(key) != Some(value))
        .map(|(key, value)| format!("  {key}: {value}"));
    let sidecars = sidecars.as_array().unwrap().iter();
    let sidecars = sidecars.map(|sidecar| format!("  sidecar: {sidecar}"));
    let lines: Vec<String> = keys.chain(sidecars).collect();
    format!("{case}:\n{}", lines.join("\n"))
}

#[test]
fn requests() {
    let mib = 1 << 20;
    let beside = || {
        vec![
            companion("db", "im-db@sha256:b", 2),
            companion("cache-1", "im-cache@sha256:c", 3),
        ]
    };
    let alone = request(&runner(256 * mib, false, false), &dials(Vec::new()), None);
    let first = alone.clone().unwrap();
    // Each case after the first shows what departs from it.
    let cases = [
        asked("main alone, passive", alone, &json!({})),
        asked(
            "main with ops, a model and a GPU",
            request(
                &runner(256 * mib, true, true),
                &dials(Vec::new()),
                Some("A100-80GB"),
            ),
            &first,
        ),
        asked(
            "two companions",
            request(&runner(256 * mib, true, false), &dials(beside()), None),
            &first,
        ),
        asked(
            "memory that isn't whole MiB",
            request(
                &runner(256 * mib + 1, false, false),
                &dials(Vec::new()),
                None,
            ),
            &first,
        ),
    ];
    check(
        cases,
        &expect![[r#"
            main alone, passive:
              args: ["/.pumpkin/sandboxd","0.0.0.0:7434","--listen","--by","2000000600","--user","1000:1000"]
              cpu_millis: 256
              env: {"SANDBOXD_GRANT":"01010101010101010101010101010101"}
              gpu: null
              hard_end: 2000003600
              image: "im-main"
              memory_mib: 256
              ports: [7434]
              role: "runner"
              run: "run-1"
              sandboxd: {"at":["/.pumpkin/sandboxd"],"path":"/opt/pumpkin/sandboxd","sha256":"5d"}
              source: "203.0.113.7/32"
            main with ops, a model and a GPU:
              args: ["/.pumpkin/sandboxd","0.0.0.0:7434","--listen","--by","2000000600","--user","1000:1000","--ops","--model"]
              gpu: "A100-80GB"
              sandboxd: {"at":["/.pumpkin/sandboxd","/.pumpkin/bin/sandboxctl"],"path":"/opt/pumpkin/sandboxd","sha256":"5d"}
            two companions:
              args: ["/.pumpkin/sandboxd","0.0.0.0:7434","--listen","--by","2000000600","--user","1000:1000","--ops","--relay","0.0.0.0:7435","--relayed","db","--relayed","cache-1"]
              ports: [7434,7435]
              sandboxd: {"at":["/.pumpkin/sandboxd","/.pumpkin/bin/sandboxctl"],"path":"/opt/pumpkin/sandboxd","sha256":"5d"}
              sidecar: {"args":["/.pumpkin/sandboxd","0.0.0.0:7434","--listen","--by","2000000600","--user","2:2","--tls"],"env":{"SANDBOXD_GRANT":"02020202020202020202020202020202","SANDBOXD_TLS_CERTIFICATE":"ce02","SANDBOXD_TLS_KEY":"5e02"},"image":"im-db","name":"db"}
              sidecar: {"args":["/.pumpkin/sandboxd","0.0.0.0:7434","--listen","--by","2000000600","--user","3:3","--tls"],"env":{"SANDBOXD_GRANT":"03030303030303030303030303030303","SANDBOXD_TLS_CERTIFICATE":"ce03","SANDBOXD_TLS_KEY":"5e03"},"image":"im-cache","name":"cache-1"}
            memory that isn't whole MiB: role `runner` asks for 268435457 bytes of memory, which Modal gives in whole MiB: ask for a multiple of 1048576"#]],
    );
}

/// How the helper's `answer` reads, for a create that began 1 s into the machine's clock.
fn answered(case: &str, relayed: bool, answer: &Value) -> String {
    let made: Made = serde_json::from_value(answer.clone()).unwrap();
    match made.listened(Duration::from_secs(1), relayed) {
        Ok(listened) => format!("{case}: {listened:?}"),
        Err(refused) => format!("{case}: {refused}"),
    }
}

#[test]
fn answers() {
    let tunnel = |host: &str| json!({ "host": host, "port": 443 });
    let made = |tunnels: Vec<Value>| {
        json!({
            "id": "sb-1",
            "tunnels": tunnels,
            "phases": [
                { "name": "lookup", "began_ns": 1_250_000_000_u64, "took_ns": 500_000_000 },
                { "name": "early", "began_ns": 900_000_000, "took_ns": 1 },
            ],
        })
    };
    let (main, relay) = (tunnel("main.modal.host"), tunnel("relay.modal.host"));
    let both = || vec![main.clone(), relay.clone()];
    let cases = [
        answered("main alone", false, &made(vec![main.clone()])),
        answered("main and its relay", true, &made(both())),
        answered("no relay, though asked", true, &made(vec![main.clone()])),
        answered("a relay, though not asked", false, &made(both())),
        answered(
            "a relay at an address",
            true,
            &made(vec![main.clone(), tunnel("192.0.2.1")]),
        ),
    ];
    check(
        cases,
        &expect![[r#"
        main alone: Listened { created: Created { id: SandboxId("sb-1"), phases: [Phase { name: "lookup", began: 250ms, took: 500ms }, Phase { name: "early", began: 0ns, took: 1ns }] }, main: Endpoint { host: "main.modal.host", port: 443 }, relay: None }
        main and its relay: Listened { created: Created { id: SandboxId("sb-1"), phases: [Phase { name: "lookup", began: 250ms, took: 500ms }, Phase { name: "early", began: 0ns, took: 1ns }] }, main: Endpoint { host: "main.modal.host", port: 443 }, relay: Some(Endpoint { host: "relay.modal.host", port: 443 }) }
        no relay, though asked: the Modal helper's `create` gave sandbox sb-1's tunnels as a list of 1: expected one for each port that it was asked for
        a relay, though not asked: the Modal helper's `create` gave sandbox sb-1's tunnels as a list of 2: expected one for each port that it was asked for
        a relay at an address: Modal's tunnel to sandbox sb-1's relay isn't an endpoint: `192.0.2.1` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`"#]],
    );
}

/// Whether a sandbox of a runner that needs `cpu_millis` thousandths of a CPU and `memory_mib` MiB
/// holds `companions`, or why not.
fn held(cpu_millis: u64, memory_mib: u64, companions: usize) -> String {
    let Create { role, .. } = runner(memory_mib << 20, false, false);
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
    let shown = |references: Result<Vec<ImageRef>, String>| match references {
        Ok(references) => {
            let references: Vec<&str> = references.iter().map(ImageRef::as_str).collect();
            cut(&references.join(" "))
        }
        Err(refused) => refused,
    };
    let references = |imported: &[&str]| shown(Batch::of(&images).references(ids(imported)));
    let cases = [
        format!("imported once each: {}", cut(&distinct.join(" "))),
        format!("referenced: {}", references(&["im-1", "im-2", "im-3"])),
        format!("an id too few: {}", references(&["im-1", "im-2"])),
    ];
    check(
        cases,
        &expect![[r#"
        imported once each: sha256:a sha256:b sha256:c
        referenced: im-2@sha256:b im-1@sha256:a im-2@sha256:b im-3@sha256:c
        an id too few: the Modal helper's `import` answered 2 ids for 3 images: expected one for each"#]],
    );
}
