//! What the table's generator reads of AWS's page, and what it refuses there.

use expect_test::{Expect, expect};

#[path = "../../src/aws_certificates/table.rs"]
mod table;

use table::generate;

/// The page's tabs of a region, each a fenced block; `rsa` is the lines of the RSA one.
fn region(heading: &str, rsa: &str) -> String {
    format!(
        "## {heading}\n<a name=\"x\"></a>\n\n------\n#### [ DSA ]\n\n```\n{BLOCK}\n```\n\n\
         ------\n#### [ RSA ]\n\n```\n{rsa}\n```\n\n------\n#### [ RSA-2048 ]\n\n```\n{BLOCK}\n```\n\n"
    )
}

const BLOCK: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";

fn check(page: &str, expect: &Expect) {
    let generated = match generate(page) {
        Ok((source, count)) => {
            let table = source.split_once("= [\n").unwrap().1;
            format!("{count} regions\n{table}")
        }
        Err(error) => error.to_string(),
    };
    expect.assert_eq(&generated);
}

#[test]
fn sorts_regions_and_keeps_only_the_rsa_tab_wrapped_at_64_columns() {
    let long = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
        "B".repeat(70)
    );
    let page = format!(
        "# AWS public certificates\n\n```\nnot a region\n```\n\n{}{}",
        region("US West (Oregon) — us-west-2", &long),
        region("Africa (Cape Town) — af-south-1", BLOCK),
    );
    check(
        &page,
        &expect![[r#"
        2 regions
            (
                "af-south-1",
                "-----BEGIN CERTIFICATE-----
        AAAA
        -----END CERTIFICATE-----",
            ),
            (
                "us-west-2",
                "-----BEGIN CERTIFICATE-----
        BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
        BBBBBB
        -----END CERTIFICATE-----",
            ),
        ];
    "#]],
    );
}

#[test]
fn refuses_a_page_that_departs_from_its_shape() {
    let page = |text: &str| format!("{}{text}", region("Africa (Cape Town) — af-south-1", BLOCK));
    let cases = [
        ("no region", "# Title\n".to_owned()),
        ("heading", page("## Related topics\n")),
        (
            "missing tab",
            page("## Asia (Taipei) — ap-east-2\n#### [ DSA ]\n```\nx\n```\n"),
        ),
        (
            "repeated region",
            page(&region("Africa (Cape Town) — af-south-1", BLOCK)),
        ),
        (
            "repeated tab",
            page(&format!(
                "## Asia (Taipei) — ap-east-2\n#### [ RSA ]\n```\n{BLOCK}\n```\n#### [ RSA ]\n```\n{BLOCK}\n```\n"
            )),
        ),
        (
            "not base64",
            page(&region(
                "Asia (Taipei) — ap-east-2",
                "-----BEGIN CERTIFICATE-----\nAA!A\n-----END CERTIFICATE-----",
            )),
        ),
        (
            "not a certificate",
            page(&region("Asia (Taipei) — ap-east-2", "AAAA")),
        ),
    ];
    let refused: Vec<String> = cases
        .iter()
        .map(|(name, page)| format!("{name}: {}", generate(page).unwrap_err()))
        .collect();
    expect![[r#"
        no region: the page has no region: expected a `## <name> — <region>` heading
        heading: line 31, `## Related topics`, is no region's heading: expected `## <name> — <region>`
        missing tab: ap-east-2 has no RSA certificate on the page
        repeated region: af-south-1 has two headings, the second at line 31
        repeated tab: ap-east-2 has two RSA certificates, the second at line 39: expected one
        not base64: the RSA certificate of ap-east-2, at line 46: expected base64, which has no `!`
        not a certificate: the RSA certificate of ap-east-2, at line 46: expected `-----BEGIN CERTIFICATE-----`, base64 and `-----END CERTIFICATE-----` lines"#]].assert_eq(&refused.join("\n"));
}
