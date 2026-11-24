//! The table of AWS's identity certificates, as Rust source, from AWS's page of them.
//!
//! It follows SPIRE's `aws-ca-parser`, which reads the Markdown form of the same page and writes
//! the certificate tables of its `aws_iid` node attestor:
//! <https://github.com/spiffe/spire/blob/5a04fc60d8339c36ce866c3537963256b31e3ad7/pkg/server/plugin/nodeattestor/awsiid/aws-ca-parser/main.go>.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::Result;

/// AWS's page of the certificates, in Markdown, which holds each region's under its heading.
pub(crate) const URL: &str = "https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/regions-certs.md";

/// The kind of certificate the table holds, as the page's tab names it: the RSA key of 1024 bits
/// that signs the `signature` form of the document. The page's other tabs are other keys.
const KIND: &str = "RSA";

/// The width of a line of PEM, which RFC 7468 fixes at 64 columns, where the page may give a longer
/// one.
const PEM_COLUMNS: usize = 64;

const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

/// The source of the table that `page`, AWS's Markdown, gives, and how many regions it holds.
pub(crate) fn generate(page: &str) -> Result<(String, usize)> {
    let certificates = parse(page)?;
    Ok((render(&certificates), certificates.len()))
}

/// The [`KIND`] certificate of each region on `page`, AWS's Markdown, as PEM, by region.
///
/// A region has a heading, `## <name> — <region>`, and a tab heading, `#### [ <kind> ]`, for each
/// kind of certificate, whose fenced block holds it. A page that departs from this fails, with
/// where, rather than lose a region.
fn parse(page: &str) -> Result<BTreeMap<String, String>> {
    let mut found: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut region: Option<&str> = None;
    let mut kind: Option<&str> = None;
    // The lines of the fenced block that is open, and the line it began at.
    let mut block: Option<(usize, Vec<&str>)> = None;
    for (index, line) in page.lines().enumerate() {
        let at = index + 1;
        if let Some((began, lines)) = &mut block {
            if !line.starts_with("```") {
                lines.push(line);
                continue;
            }
            if let (Some(region), Some(KIND)) = (region, kind) {
                let pem = pem(lines).map_err(|why| {
                    format!("the {KIND} certificate of {region}, at line {began}: {why}")
                })?;
                if found
                    .insert(region.to_owned(), Some(pem))
                    .is_some_and(|had| had.is_some())
                {
                    return Err(format!(
                        "{region} has two {KIND} certificates, the second at line {began}: \
                         expected one"
                    )
                    .into());
                }
            }
            block = None;
        } else if let Some(heading) = line.strip_prefix("## ") {
            let named = heading.rsplit_once(" — ").map(|(_, region)| region);
            let Some(named) = named.filter(|region| is_region(region)) else {
                return Err(format!(
                    "line {at}, `{line}`, is no region's heading: expected `## <name> — <region>`"
                )
                .into());
            };
            if found.insert(named.to_owned(), None).is_some() {
                return Err(format!("{named} has two headings, the second at line {at}").into());
            }
            (region, kind) = (Some(named), None);
        } else if let Some(heading) = line.strip_prefix("#### [") {
            kind = heading.strip_suffix(']').map(str::trim);
        } else if line.starts_with("```") {
            block = Some((at, Vec::new()));
        }
    }
    if found.is_empty() {
        return Err("the page has no region: expected a `## <name> — <region>` heading".into());
    }
    let mut certificates = BTreeMap::new();
    for (region, pem) in found {
        let Some(pem) = pem else {
            return Err(format!("{region} has no {KIND} certificate on the page").into());
        };
        certificates.insert(region, pem);
    }
    Ok(certificates)
}

/// Whether `text` is the name of a region, such as `us-east-1`.
fn is_region(text: &str) -> bool {
    text.contains('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The certificate in `lines`, a fenced block's, as PEM wrapped at [`PEM_COLUMNS`], without a
/// final newline.
fn pem(lines: &[&str]) -> std::result::Result<String, String> {
    let lines: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
    let lines: Vec<&str> = lines.into_iter().filter(|line| !line.is_empty()).collect();
    let [begin, body @ .., end] = lines.as_slice() else {
        return Err(format!("expected `{BEGIN}`, base64 and `{END}` lines"));
    };
    if (*begin, *end) != (BEGIN, END) || body.is_empty() {
        return Err(format!("expected `{BEGIN}`, base64 and `{END}` lines"));
    }
    let base64: Vec<char> = body.concat().chars().collect();
    if let Some(bad) = (base64.iter()).find(|c| !(c.is_ascii_alphanumeric() || "+/=".contains(**c)))
    {
        return Err(format!("expected base64, which has no `{bad}`"));
    }
    let mut pem = format!("{BEGIN}\n");
    for line in base64.chunks(PEM_COLUMNS) {
        pem.extend(line);
        pem.push('\n');
    }
    pem.push_str(END);
    Ok(pem)
}

/// The Rust source of the table of `certificates`.
fn render(certificates: &BTreeMap<String, String>) -> String {
    let count = certificates.len();
    let mut source = format!(
        "//! Code generated by `cargo xtask aws-certificates`. DO NOT EDIT: run it again to follow
//! AWS's page, as when AWS rotates a certificate or adds a region. Then read the diff, refill the
//! `identity_certificates` snapshot with `cargo xtask refill --package ec2 identity_certificates`,
//! and commit the result.
//!
//! The certificate of the RSA key that signs the `signature` of the identity documents of each
//! region's instances, as AWS publishes them, wrapped at 64 columns, as RFC 7468 writes PEM:
//! <{URL}>.
//! The generator follows SPIRE's `aws-ca-parser`:
//! <https://github.com/spiffe/spire/blob/5a04fc60d8339c36ce866c3537963256b31e3ad7/pkg/server/plugin/nodeattestor/awsiid/aws-ca-parser/main.go>.

/// AWS's certificate of the key that signs the `signature` of the identity documents of each
/// region's instances, as PEM, by region, sorted by the region's name: one for each region on
/// AWS's page of them.
pub(crate) const IDENTITY_CERTIFICATES: [(&str, &str); {count}] = [\n"
    );
    for (region, pem) in certificates {
        let _ = write!(
            source,
            "    (\n        \"{region}\",\n        \"{pem}\",\n    ),\n"
        );
    }
    source.push_str("];\n");
    source
}
