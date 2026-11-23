//! What the EC2 provider decides without an instance: whether a hello's attestation is the identity
//! document of the instance that a create launched, from that instance's address (`identity`,
//! which this binary compiles itself, as the crate keeps it private), the certificates of AWS's
//! keys that it holds, and what the read of an envelope takes of its launch templates
//! (`envelope`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use expect_test::{Expect, expect};
use identity::{Documents, IDENTITY_CERTIFICATES};
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use schema::ec2_boot::Identity;
use schema::sandbox::Attestation;

mod envelope;
#[path = "../../src/identity.rs"]
mod identity;

// A key pair made for these tests alone, by `openssl req -x509 -newkey rsa:2048`: its private key,
// in PKCS #8, and its certificate. It is of 2048 bits, the fewest that `ring` signs with, where
// AWS's keys have 1024.
const TEST_KEY: &str = "
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDdxdpXe2s9nTLf
3cLrhtV7TlEN7+wQqiTKiLvFbkCUmf5//agkjif9rqDvfQmy7xfUxtucbMfof/ks
EokKgTz+lf7xnQ6ZI+ksT26YYdjcDpexP5rKh2Mg+LZK7JUzYXgdgRLeL8jdEC0L
tP/+EReocW9jq5AY3dVmSDScfsYwM6om6jNGifHr6rFaG8FbgdyB++fWcogUTz1P
pFux47wmBhzZnSau1iHxzUF4mXvp+/KP2bete7vSjIwpnNT4jCv4kdPw7GMaTCjS
1jpiFelByAN4QgwpTBkvSXkDZivwNshAxZr0KeJ9xpbovQe3TL2z1U/4343S+IbL
yWLKNHgbAgMBAAECggEAKsa7+73JjKPiqT2cGFP0n4kWp0qMzRYUbKB9H3LJsCAX
79YcdZiZ7Buf3THQChyGCzukJoLw+E6e4DpfBpCxFCYYe8AgjbdYhXx4x5mv+5ow
9jZfhAsjOrveZYtGqBd31LEN05+B7ent1mE+M2JWnt3qb14rODCptcXdqAXn8mnY
qTxIiA4hxMhT/pfeQr3boWC2HUkxNWDskjcPXTjuqPnWVZoeWMsjMY2IESEvO11k
Wa96tn45z7OuBMhuLeRk04J0TaPtNQJ0fY8FdvZ1v18K9GFGbXCpYxpvSZEG0zMH
5XCbLdSWiRmMeWK600MTKC5HN9ylgK9RQ0PdshF1+QKBgQD8Tt2O1EgDAclcGPpc
kZ9XzkJhSqDq/GVXoGwjCS3S/HhxOe8e8k0/+vCHtPQ/dJp8FKFoOP5hJ2miLDOI
h/G7c49yk4s4yA1GTum65HuoGD0VFmHODLOIo/5xF25fkR8jRm4JaOlPbkcUoQln
4NRvzfUJmfYuVTZTOvDouRjjRQKBgQDhBJqcTF+qhtrnETfQrTTK/Y4TdONIguZl
CzB+ZlHpNGXuOOTaLBc7Lb/f9y47DM8r2k3zg3TxkHmyl626uSxblNcgdOMP2pDV
MIR8SozF2ps6IoY/oucK/WTdMOCu8PzPJwHACF8BUP1r50ejhhLBJPib83Wl8JF/
WwjbzVzz3wKBgCifqhytIUsapoklnGn7KGTC2P1BPteW+9o4AjFB8Rbff6T7Zvzc
loOOq9utzwXIiHch+zsEsjCCYAe0hjeda9Z6rMYN64Sg8UI4wnlNAHv4Kx2SFP1Q
HjgJ75XV5/eLLjBHzr/FHIulfdxccV9AW/QEfUemenTLU4vjGZbe9prhAoGBAMH8
iRnj0bmeZss7kcXJ/LBfJQPrCiIyD1Pe8KCXr1f/vzw/OjRIPILdLwRzAR0RLu1c
V0Vu9/tlFTV9A93s06vkRg4KFtJNF/1ixWStlGzhyF628subsdlMmI8EGz9WLGyd
8SlPTjBBVSiYC6+S30o5iEZR/gPfwF9t6NYtVkw7AoGAa7w5LILfAFt5rM4IuaPM
lxPyKlbKIoF1bc/wyjYv5CrwtAtcstn4Us5Het0VA0MVOm49Xfi26+4ZJkYAjm6l
/RUvk2JLvOjg4VEoxVx1v6XDMMSXvw6fv6mY8BDskYnSWSwTv0kAPOOJTVfEfSta
aMzz6q3cGEiPe7i2OwpdVNc=";

const TEST_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIDDzCCAfegAwIBAgIUSrwjo0hVjYHUkq/0XyOCmUeuKPgwDQYJKoZIhvcNAQEL
BQAwFzEVMBMGA1UEAwwMcHVtcGtpbi10ZXN0MB4XDTI2MTEyMTIwMTY1NVoXDTI2
MTEyMjIwMTY1NVowFzEVMBMGA1UEAwwMcHVtcGtpbi10ZXN0MIIBIjANBgkqhkiG
9w0BAQEFAAOCAQ8AMIIBCgKCAQEA3cXaV3trPZ0y393C64bVe05RDe/sEKokyoi7
xW5AlJn+f/2oJI4n/a6g730Jsu8X1MbbnGzH6H/5LBKJCoE8/pX+8Z0OmSPpLE9u
mGHY3A6XsT+ayodjIPi2SuyVM2F4HYES3i/I3RAtC7T//hEXqHFvY6uQGN3VZkg0
nH7GMDOqJuozRonx6+qxWhvBW4Hcgfvn1nKIFE89T6RbseO8JgYc2Z0mrtYh8c1B
eJl76fvyj9m3rXu70oyMKZzU+Iwr+JHT8OxjGkwo0tY6YhXpQcgDeEIMKUwZL0l5
A2Yr8DbIQMWa9CnifcaW6L0Ht0y9s9VP+N+N0viGy8liyjR4GwIDAQABo1MwUTAd
BgNVHQ4EFgQUPChKp9t9vwFUe4DqrdhmHgLg7O0wHwYDVR0jBBgwFoAUPChKp9t9
vwFUe4DqrdhmHgLg7O0wDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOC
AQEANeWGycDxz9jNaTVqHE2BdHhw713tXKEn7wm1XlAWUXVXlOkMqcqOPqQvs2ij
eVaQgRmVk8WIIfmUehDxNJ0DW0aGbi+QBJnQGhOPFwa4a2EShzGD9s5jhlNyASWb
dshx1CJpiWBFBcz9xXigt8tObom6SW5mtX0mbeh5b1mJr3d8+DwF30XcL6PGiVcL
XfFSwnNOibtet0gj0GceQdZw565MQ9Aj66M8bp+kyPfGAvdPUwABq8+RtblbJTwN
67zeAfHT7a4XF77Uy49voT2Co+GOx3n8gkL3+AKAjEah92SAizcDLS3t4U0Cm4Gf
NezZMkR3LgbUGE2sV9OPmPCUww==
-----END CERTIFICATE-----";

const ACCOUNT: &str = "123456789012";
const REGION: &str = "us-west-1";
const INSTANCE: &str = "i-0123456789abcdef0";
const PRIVATE_IP: &str = "10.0.1.5";

fn check(lines: impl IntoIterator<Item = String>, expect: &Expect) {
    let lines: Vec<String> = lines.into_iter().collect();
    expect.assert_eq(&lines.join("\n"));
}

/// An identity document as AWS writes one, of `account`, `region` and `instance`, at `PRIVATE_IP`.
fn document(account: &str, region: &str, instance: &str) -> String {
    let fields = [
        ("accountId", account),
        ("architecture", "x86_64"),
        ("instanceId", instance),
        ("privateIp", PRIVATE_IP),
        ("region", region),
        ("version", "2017-09-30"),
    ];
    let fields: Vec<String> = (fields.iter())
        .map(|(name, value)| format!("  \"{name}\" : \"{value}\""))
        .collect();
    format!("{{\n{}\n}}", fields.join(",\n"))
}

/// The base64 of `document`'s signature by the test key, in lines of 64, as an instance's metadata
/// serves a signature.
fn signed(document: &str) -> String {
    let der: String = TEST_KEY.split_ascii_whitespace().collect();
    let key = RsaKeyPair::from_pkcs8(&STANDARD.decode(der).unwrap()).unwrap();
    let mut signature = vec![0; key.public().modulus_len()];
    let random = SystemRandom::new();
    (key.sign(
        &RSA_PKCS1_SHA256,
        &random,
        document.as_bytes(),
        &mut signature,
    ))
    .unwrap();
    let base64 = STANDARD.encode(signature);
    let lines: Vec<&str> = (base64.as_bytes().chunks(64))
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect();
    lines.join("\n")
}

fn attestation(document: &str, signature: &str) -> Attestation {
    let identity = Identity {
        document: document.to_owned(),
        signature: signature.to_owned(),
    };
    Attestation::try_from(serde_json::to_vec(&identity).unwrap()).unwrap()
}

fn hellos() -> Vec<(&'static str, Attestation, IpAddr)> {
    let launched = document(ACCOUNT, REGION, INSTANCE);
    let signature = signed(&launched);
    let other = |account, region, instance| {
        let document = document(account, region, instance);
        attestation(&document, &signed(&document))
    };
    let ip: IpAddr = PRIVATE_IP.parse().unwrap();
    let elsewhere: IpAddr = "10.0.1.6".parse().unwrap();
    let mapped: IpAddr = format!("::ffff:{PRIVATE_IP}").parse().unwrap();
    let flipped = launched.replace("x86_64", "x86_65");
    let cases = [
        (
            "the launched instance's",
            attestation(&launched, &signature),
            ip,
        ),
        ("a byte flipped", attestation(&flipped, &signature), ip),
        (
            "another instance's",
            other(ACCOUNT, REGION, "i-0fedcba9876543210"),
            ip,
        ),
        (
            "another account's",
            other("210987654321", REGION, INSTANCE),
            ip,
        ),
        (
            "another region's",
            other(ACCOUNT, "us-east-1", INSTANCE),
            ip,
        ),
        (
            "from another address",
            attestation(&launched, &signature),
            elsewhere,
        ),
        (
            "from its address, IPv4-mapped",
            attestation(&launched, &signature),
            mapped,
        ),
        ("a signature not in base64", attestation(&launched, "%"), ip),
        (
            "no identity",
            Attestation::try_from(b"{}".to_vec()).unwrap(),
            ip,
        ),
    ];
    cases.into()
}

#[test]
fn identity_documents() {
    let cases = hellos();
    let ip: IpAddr = PRIVATE_IP.parse().unwrap();
    let documents = Documents::of(TEST_CERTIFICATE, ACCOUNT.to_owned(), REGION.to_owned());
    let admits = documents.unwrap().admits(INSTANCE.to_owned());
    let aws = Documents::of_region(REGION, ACCOUNT.to_owned()).unwrap();
    let aws = aws.admits(INSTANCE.to_owned());
    let lines = (cases.iter())
        .map(|(case, attestation, peer)| format!("{case}: {:?}", admits(attestation, *peer)))
        .chain([format!(
            "signed by a key other than AWS's: {:?}",
            aws(&cases[0].1, ip)
        )]);
    check(
        lines,
        &expect![[r#"
        the launched instance's: Ok(())
        a byte flipped: Err("the identity document's signature doesn't verify with the key of AWS's certificate for us-west-1: expected a document that AWS signed, of an instance there, by the key of the certificate that the provider holds")
        another instance's: Err("the identity document names an instance that no start awaits: expected that of an instance launched for a sandbox")
        another account's: Err("the identity document names an account other than the envelope's: expected an instance of the envelope's account")
        another region's: Err("the identity document names region `us-east-1`: expected us-west-1, the envelope's")
        from another address: Err("the hello comes from 10.0.1.6, not 10.0.1.5, the private address that its identity document names: expected it from its instance's own address, with no network address translation between")
        from its address, IPv4-mapped: Err("the hello comes from ::ffff:10.0.1.5, not 10.0.1.5, the private address that its identity document names: expected it from its instance's own address, with no network address translation between")
        a signature not in base64: Err("the document's signature isn't base64: Invalid symbol 37, offset 0.")
        no identity: Err("the attestation isn't an EC2 instance's identity document and its signature, as JSON: expected them as `ec2-boot` reads them")
        signed by a key other than AWS's: Err("the identity document's signature doesn't verify with the key of AWS's certificate for us-west-1: expected a document that AWS signed, of an instance there, by the key of the certificate that the provider holds")"#]],
    );
}

// Each pinned by the SHA-256 of its DER, as `openssl x509 -fingerprint -sha256` shows it: AWS
// publishes none, so this pins the provider's own copies.
#[test]
fn identity_certificates() {
    let regions: Vec<&str> = IDENTITY_CERTIFICATES
        .iter()
        .map(|(region, _)| *region)
        .collect();
    assert!(
        regions.is_sorted_by(|a, b| a < b),
        "the regions, each once, in order"
    );
    let pinned = (IDENTITY_CERTIFICATES.iter()).map(|(region, certificate)| {
        let base64: String = (certificate.lines())
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let fingerprint = digest(&SHA256, &STANDARD.decode(base64).unwrap());
        let hex: Vec<String> = (fingerprint.as_ref().iter())
            .map(|byte| format!("{byte:02X}"))
            .collect();
        let read = Documents::of_region(region, ACCOUNT.to_owned()).map(drop);
        format!("{region}: {}: {read:?}", hex.join(":"))
    });
    let unheld = Documents::of_region("mars-1", ACCOUNT.to_owned()).map(drop);
    check(
        pinned.chain([format!("mars-1: {unheld:?}")]),
        &expect![[r#"
            af-south-1: 50:9A:6F:F1:27:D3:54:04:47:F9:96:94:17:5D:59:57:48:B6:34:E3:E7:6F:1D:06:67:1C:5E:40:FD:44:FE:33: Ok(())
            ap-east-1: B1:CF:4F:5C:06:96:CC:FE:3D:CA:E0:22:77:13:0B:17:ED:1A:7F:0D:0D:E8:9B:0B:AA:23:EA:A2:58:93:CB:32: Ok(())
            ap-east-2: 3B:33:7E:11:34:2D:A2:4F:1B:A8:D3:53:45:F0:74:F0:1C:97:16:95:7E:93:83:4C:00:01:96:10:F7:BD:F7:FD: Ok(())
            ap-northeast-1: 67:D2:8C:01:FC:23:C2:F0:DF:9A:08:4A:B3:92:19:DC:E4:C2:79:AB:26:EF:A5:0A:A4:A3:0E:73:36:85:E0:3B: Ok(())
            ap-northeast-2: F4:BC:3E:8B:D6:54:0B:FF:1A:5F:C1:49:39:33:FD:EC:B5:CF:CA:8D:8A:32:22:B6:1B:9A:F8:81:2E:67:C9:9B: Ok(())
            ap-northeast-3: F9:3F:18:9A:54:63:A5:63:90:67:5F:DC:91:88:DB:A7:FA:7B:53:1C:38:95:44:0E:A0:81:D3:25:62:55:A8:B0: Ok(())
            ap-south-1: 63:00:69:62:01:25:C4:6E:15:40:A2:2D:00:10:50:78:AF:48:E5:36:A4:83:21:87:92:9C:EA:B5:EF:F7:C6:CC: Ok(())
            ap-south-2: 79:2E:CF:88:30:1B:EE:08:59:C9:F1:8C:90:82:00:A8:CE:27:DF:F0:C2:27:FE:9E:FC:F4:1F:4A:50:04:DA:0E: Ok(())
            ap-southeast-1: DF:0E:83:6F:8A:6A:68:34:63:EB:5E:E4:7E:64:2F:C0:AA:89:B5:1E:83:82:C5:A6:8E:E9:C8:53:97:43:96:F7: Ok(())
            ap-southeast-2: 8C:9B:35:CC:96:28:9D:18:F3:21:8A:B7:8C:36:93:48:3F:80:03:8E:EE:87:4B:E4:71:7C:DC:9B:8D:5B:38:D4: Ok(())
            ap-southeast-3: 76:18:1C:76:66:51:38:BE:BE:6C:23:DE:C3:08:66:30:A9:3E:2F:0E:D0:6E:7D:0F:06:E7:D2:C2:EF:6D:BB:1E: Ok(())
            ap-southeast-4: EF:58:FE:E8:48:5A:10:93:92:56:EF:90:E9:49:BD:5F:89:74:BB:31:10:2E:CC:87:15:67:F6:F3:17:57:9D:13: Ok(())
            ap-southeast-5: 11:EC:D2:F8:15:13:83:06:6A:5C:57:6D:F2:92:D5:53:04:9C:08:59:F0:D4:DE:FE:C7:CE:C4:A6:26:20:E4:D0: Ok(())
            ap-southeast-6: 31:F3:F9:33:15:72:92:59:F3:92:64:C4:D6:FF:7E:6C:42:F7:09:90:8D:AB:70:0A:B0:58:72:05:36:6C:46:A7: Ok(())
            ap-southeast-7: CB:EE:63:D1:0F:E3:C7:08:88:7C:07:FD:74:6B:6D:9A:DA:FA:E9:45:73:84:6F:48:0F:52:9A:A1:0A:BF:C6:BC: Ok(())
            ca-central-1: F1:9F:4F:C6:FB:16:85:B9:7E:71:E3:E3:56:A9:33:C9:B1:01:CB:22:46:45:E7:2D:0A:40:12:56:23:3F:48:ED: Ok(())
            ca-west-1: BC:DB:D4:69:B0:71:B3:D3:0B:7D:FF:3A:F6:34:C5:C4:17:71:C6:DD:7E:54:0B:A8:10:51:75:42:97:9B:50:7C: Ok(())
            cn-north-1: 68:74:24:D4:5C:66:13:7D:6F:8C:D5:7E:3A:29:C7:5F:B2:88:1B:E6:47:3F:F4:20:FB:AF:19:11:DF:05:FC:7A: Ok(())
            cn-northwest-1: 68:74:24:D4:5C:66:13:7D:6F:8C:D5:7E:3A:29:C7:5F:B2:88:1B:E6:47:3F:F4:20:FB:AF:19:11:DF:05:FC:7A: Ok(())
            eu-central-1: 99:43:5C:EF:CA:ED:AF:26:2D:FF:8C:AE:B6:A7:A3:8E:AF:C7:C1:1C:A7:98:8B:07:99:72:4B:1C:9B:9A:0C:AF: Ok(())
            eu-central-2: E8:83:87:14:8E:C9:B2:99:6D:64:73:08:E3:68:D0:19:47:FF:BF:B6:25:83:D0:30:CE:28:B6:5A:08:E1:0D:E1: Ok(())
            eu-north-1: E5:58:46:43:2E:CC:E6:0F:1D:31:37:7A:D6:05:A6:35:5B:4C:B5:B4:EA:41:4A:54:98:60:71:51:F5:82:C1:E2: Ok(())
            eu-south-1: D3:3D:2D:21:F4:15:4F:44:B9:F6:AE:5B:68:78:9D:51:D7:97:45:3B:48:FF:BD:FC:26:33:33:53:B0:69:71:D5: Ok(())
            eu-south-2: 17:37:7B:03:33:60:17:D1:54:6A:1A:18:1D:53:B1:61:39:01:74:1D:43:8D:F2:71:BC:BA:B7:DB:87:9A:2E:47: Ok(())
            eu-west-1: 70:92:99:3E:FE:38:80:0E:ED:D6:9D:EF:9C:F3:4B:73:4A:C8:28:CC:E3:0E:F9:43:42:38:8B:C7:A5:48:82:7D: Ok(())
            eu-west-2: 7B:57:07:EB:89:16:6F:F0:7A:42:9B:74:DE:90:A8:06:9D:55:FA:3C:33:C9:45:F5:B2:1E:A3:6A:E6:7D:08:AD: Ok(())
            eu-west-3: DD:45:CA:F9:28:C8:28:FA:0A:3A:96:08:2F:C2:DA:C9:9E:0E:22:E3:B4:05:96:31:9F:B1:A9:5F:3E:8D:0D:D2: Ok(())
            eusc-de-east-1: 70:ED:FB:69:A3:7B:5E:FE:DC:CA:6A:68:CE:41:F9:A9:C2:17:38:D9:AF:13:1C:C3:F1:E0:65:ED:4E:62:72:7B: Ok(())
            il-central-1: 7D:97:95:C3:3B:48:A0:01:94:B4:FB:A5:B6:4F:0E:11:76:6C:66:22:00:95:6A:2B:FA:21:84:FC:9A:52:50:E8: Ok(())
            me-central-1: 6B:0A:F8:0A:38:4D:E2:81:A8:B9:28:5D:5F:33:B9:B4:D2:6A:4A:CB:8C:DB:FD:9A:C4:78:E6:FF:B8:30:F7:56: Ok(())
            me-south-1: 28:00:00:67:2B:F2:5F:A7:C3:11:33:EB:7A:4A:0C:89:31:F7:13:A4:D5:F4:F1:FB:66:9B:7D:8A:43:0F:BC:F8: Ok(())
            mx-central-1: 35:42:E0:11:68:C9:21:D2:A5:95:00:E4:00:24:1B:25:F3:B2:64:3B:F5:0A:99:5D:E9:CF:40:29:AD:F8:4D:0B: Ok(())
            sa-east-1: C0:6C:2C:C6:87:E3:FB:E7:CB:38:FA:C8:6C:86:46:5D:91:C8:58:E3:14:63:3A:AA:F4:DD:12:AD:0E:AE:D3:A6: Ok(())
            us-east-1: 65:02:D1:18:33:53:7B:9C:F0:A6:4D:96:D9:83:CA:76:FE:31:7D:3E:C7:5A:82:E5:94:0C:A0:53:90:EA:A5:9F: Ok(())
            us-east-2: 4C:8E:43:C5:7E:10:04:AC:FB:E0:3C:2B:9C:11:4C:71:C2:5D:B0:DC:FB:2E:60:65:2A:1D:C4:8B:8F:4A:C2:A6: Ok(())
            us-gov-east-1: AC:86:33:55:68:56:89:45:FB:07:FB:74:21:E8:BE:6B:B5:7B:D2:C1:54:CE:9B:9C:B8:99:6F:86:E8:A5:4B:27: Ok(())
            us-gov-west-1: FB:6B:DE:DA:4F:10:3E:7B:85:E4:2C:3A:B0:62:BB:9F:7A:09:C4:15:E7:C8:E5:EC:4C:8F:16:01:B3:CE:2E:63: Ok(())
            us-west-1: CB:58:43:D1:CE:7C:F0:A8:2D:40:C4:BB:D8:50:61:B6:E8:4D:74:0E:C5:2A:E9:99:DD:00:D6:00:ED:C8:38:27: Ok(())
            us-west-2: A7:52:02:A6:4F:F3:C4:02:2E:6D:97:EB:91:E5:F4:33:E8:1A:F9:AA:3E:04:40:77:96:D1:92:85:1F:37:3A:20: Ok(())
            mars-1: Err("the provider holds no AWS certificate for region `mars-1`, whose key signs the identity documents of its instances: `cargo xtask aws-certificates` adds it to `IDENTITY_CERTIFICATES`, from https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/regions-certs.html")"#]],
    );
}
