//! How the gateway knows an EC2 sandbox: by its instance's identity document, which AWS signs and
//! the instance's metadata serves to whatever runs on the instance, so `ec2-boot` fetches it before
//! the container starts ([`Identity`]). A hello is the sandbox's if its document is signed by the
//! region's key, names the instance that the create launched, in the envelope's account and
//! region, and names as its private address the one the hello comes from, which no other host
//! has. The gateway opens a session for the first such hello alone.
//!
//! It is the check of SPIRE's `aws_iid` node attestor, which trusts an instance on its first use,
//! and of Vault's `ec2` login, where `disallow_reauthentication` holds an instance to one login
//! and `token_bound_cidrs` binds it to the peer's address:
//! <https://github.com/spiffe/spire/blob/a676d95d6e6992ce73715f01c17525e60b50b5e8/pkg/server/plugin/nodeattestor/awsiid/iid.go#L330-L353>,
//! <https://github.com/hashicorp/vault/blob/558abfa75702b5dab4c98e86b802fb9aef43b0eb/builtin/credential/aws/path_login.go#L477-L513>,
//! <https://github.com/hashicorp/vault/blob/558abfa75702b5dab4c98e86b802fb9aef43b0eb/builtin/credential/aws/path_login.go#L771-L779>.
//!
//! The key is the one in AWS's certificate for the region ([`IDENTITY_CERTIFICATES`], which
//! `cargo xtask aws-certificates` writes from AWS's page), whose dates go unchecked, as SPIRE
//! checks none of its certificate's and AWS's own steps take the key alone out of it:
//! <https://github.com/spiffe/spire/blob/a676d95d6e6992ce73715f01c17525e60b50b5e8/pkg/server/plugin/nodeattestor/awsiid/awsca.go#L10-L42>,
//! <https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/verify-iid.html>.

use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
pub(crate) use certificates::IDENTITY_CERTIFICATES;
use provider::Admits;
use ring::signature::{RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, UnparsedPublicKey};
use schema::ec2_boot::Identity;
use schema::sandbox::Attestation;
use serde::Deserialize;

// By its path, which holds also where a test binary compiles this module.
#[path = "identity/certificates.rs"]
mod certificates;

// The sizes of key that `ring`'s check of this signature takes, in bits of the modulus.
const KEY_BITS: std::ops::RangeInclusive<usize> = 1024..=8192;

/// The identity documents of an envelope's instances: those that its region's key signs, which
/// name its account and region.
#[derive(Clone, Debug)]
pub(crate) struct Documents {
    // The key as PKCS#1's `RSAPublicKey`, in DER, as `ring` takes it.
    key: Vec<u8>,
    account: String,
    region: String,
}

/// What [`Documents::is_of`] reads of a document; AWS's documents hold more.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    account_id: String,
    region: String,
    instance_id: String,
    private_ip: IpAddr,
}

impl Documents {
    /// The documents that AWS's key for `region` signs ([`IDENTITY_CERTIFICATES`]), of `account`.
    pub(crate) fn of_region(region: &str, account: String) -> Result<Self, String> {
        let certificate = (IDENTITY_CERTIFICATES.iter())
            .find(|(named, _)| *named == region)
            .map(|(_, certificate)| *certificate);
        let Some(certificate) = certificate else {
            return Err(format!(
                "the provider holds no AWS certificate for region `{region}`, whose key signs \
                 the identity documents of its instances: `cargo xtask aws-certificates` adds \
                 it to `IDENTITY_CERTIFICATES`, from \
                 https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/regions-certs.html"
            ));
        };
        Self::of(certificate, account, region.to_owned())
    }

    /// The documents that `certificate`'s key signs, of `account` and `region`.
    pub(crate) fn of(certificate: &str, account: String, region: String) -> Result<Self, String> {
        let key = rsa_key(certificate).map_err(|why| {
            format!(
                "AWS's certificate for region `{region}` holds no RSA key that can be read: \
                 {why}: expected one in PEM, as AWS publishes it"
            )
        })?;
        Ok(Self {
            key,
            account,
            region,
        })
    }

    /// The check that admits only the sandbox of instance `instance_id`: a hello whose attestation
    /// is that instance's [`Identity`], its document signed by the key, and that comes from the
    /// document's `privateIp`. Why it refuses one names nothing of `instance_id`'s.
    pub(crate) fn admits(self, instance_id: String) -> Admits {
        Box::new(move |attestation: &Attestation, peer: IpAddr| {
            self.is_of(attestation, &instance_id, peer)
        })
    }

    fn is_of(
        &self,
        attestation: &Attestation,
        instance_id: &str,
        peer: IpAddr,
    ) -> Result<(), String> {
        let region = &self.region;
        let Ok(Identity {
            document,
            signature,
        }) = serde_json::from_slice(attestation.bytes())
        else {
            return Err(
                "the attestation isn't an EC2 instance's identity document and its \
                        signature, as JSON: expected them as `ec2-boot` reads them"
                    .to_owned(),
            );
        };
        // Base64 in lines, as the instance's metadata serves it.
        let signature: String = signature.split_ascii_whitespace().collect();
        let signature = (STANDARD.decode(signature))
            .map_err(|error| format!("the document's signature isn't base64: {error}"))?;
        let key =
            UnparsedPublicKey::new(&RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, &self.key);
        // The signature is checked first, so that nothing else of the document is read unless
        // AWS signed it.
        if key.verify(document.as_bytes(), &signature).is_err() {
            return Err(format!(
                "the identity document's signature doesn't verify with the key of AWS's \
                 certificate for {region}: expected a document that AWS signed, of an instance \
                 there, by the key of the certificate that the provider holds"
            ));
        }
        let document = serde_json::from_str::<Document>(&document).map_err(|error| {
            format!(
                "the signed identity document can't be read: {error}: expected its \
                 `accountId`, `region`, `instanceId` and `privateIp`"
            )
        })?;
        if document.account_id != self.account {
            return Err(
                "the identity document names an account other than the envelope's: \
                        expected an instance of the envelope's account"
                    .to_owned(),
            );
        }
        if document.region != self.region {
            return Err(format!(
                "the identity document names region `{}`: expected {region}, the envelope's",
                document.region
            ));
        }
        if document.instance_id != instance_id {
            return Err(
                "the identity document names an instance that no start awaits: \
                        expected that of an instance launched for a sandbox"
                    .to_owned(),
            );
        }
        if document.private_ip != peer {
            return Err(format!(
                "the hello comes from {peer}, not {}, the private address that its identity \
                 document names: expected it from its instance's own address, with no network \
                 address translation between",
                document.private_ip
            ));
        }
        Ok(())
    }
}

/// The RSA key that a certificate in PEM holds, as PKCS#1's `RSAPublicKey`, in DER.
fn rsa_key(certificate: &str) -> Result<Vec<u8>, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    // The OID of `rsaEncryption` and its NULL parameters, the algorithm of an RSA key's
    // `SubjectPublicKeyInfo`: RFC 3279, section 2.3.1.
    const RSA_ENCRYPTION: &[u8] = b"\x06\x09\x2a\x86\x48\x86\xf7\x0d\x01\x01\x01\x05\x00";
    let base64 = (certificate.trim())
        .strip_prefix(BEGIN)
        .and_then(|rest| rest.strip_suffix(END))
        .ok_or("it isn't one certificate in PEM")?;
    let base64: String = base64.split_ascii_whitespace().collect();
    let der = STANDARD
        .decode(base64)
        .map_err(|error| format!("its base64 can't be read: {error}"))?;
    // RFC 5280, section 4.1: the certificate's `tbsCertificate`, then in it a version, for all
    // but version 1, the serial number, the signature's algorithm, the issuer, the validity and the
    // subject, then the key's `SubjectPublicKeyInfo`.
    let (certificate, _) = Der(&der).take(SEQUENCE)?;
    let (tbs, _) = certificate.take(SEQUENCE)?;
    let tbs = tbs.skip(VERSION).unwrap_or(tbs);
    let fields = [INTEGER, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE];
    let tbs = fields.into_iter().try_fold(tbs, Der::skip)?;
    let (key_info, _) = tbs.take(SEQUENCE)?;
    let (algorithm, rest) = key_info.take(SEQUENCE)?;
    if algorithm.0 != RSA_ENCRYPTION {
        return Err("its key isn't RSA's".to_owned());
    }
    let (bits, _) = rest.take(BIT_STRING)?;
    // A key's bit string has no unused bits.
    let [0, key @ ..] = bits.0 else {
        return Err("its key's bits don't fill whole bytes".to_owned());
    };
    // RFC 8017, appendix A.1.1: `RSAPublicKey`, the modulus, then the public exponent.
    let (fields, _) = Der(key).take(SEQUENCE)?;
    let (modulus, _) = fields.take(INTEGER)?;
    let significant = (modulus.0.iter()).position(|&byte| byte != 0);
    let bits = significant.map_or(0, |at| {
        let leading = modulus.0[at].leading_zeros() as usize;
        (modulus.0.len() - at) * 8 - leading
    });
    if !KEY_BITS.contains(&bits) {
        return Err(format!(
            "its key's modulus is {bits} bits: expected {} to {}",
            KEY_BITS.start(),
            KEY_BITS.end()
        ));
    }
    Ok(key.to_vec())
}

const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const SEQUENCE: u8 = 0x30;
// `[0] EXPLICIT`, a version's tag.
const VERSION: u8 = 0xa0;

/// Bytes of DER, of which [`take`](Self::take) reads the first value.
#[derive(Clone, Copy)]
struct Der<'d>(&'d [u8]);

impl Der<'_> {
    /// The contents of the first value, which must have `tag`, and what follows it.
    fn take(self, tag: u8) -> Result<(Self, Self), String> {
        let malformed = || format!("its DER doesn't hold a value of tag {tag:#04x} where expected");
        let [found, first, rest @ ..] = self.0 else {
            return Err(malformed());
        };
        if *found != tag {
            return Err(malformed());
        }
        // A length of one byte below 128, or of one or two bytes that the first one counts, which
        // is all that a certificate of under 64 KiB takes.
        let (len, rest) = match (*first, rest) {
            (len @ 0..=0x7f, rest) => (usize::from(len), rest),
            (0x81, [len, rest @ ..]) => (usize::from(*len), rest),
            (0x82, [high, low, rest @ ..]) => {
                (usize::from(u16::from_be_bytes([*high, *low])), rest)
            }
            _ => return Err(malformed()),
        };
        if rest.len() < len {
            return Err(malformed());
        }
        let (contents, rest) = rest.split_at(len);
        Ok((Self(contents), Self(rest)))
    }

    /// What follows the first value, which must have `tag`.
    fn skip(self, tag: u8) -> Result<Self, String> {
        self.take(tag).map(|(_, rest)| rest)
    }
}
