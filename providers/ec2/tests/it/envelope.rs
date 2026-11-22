//! What the read of an envelope takes of its launch templates, as EC2 describes them: the
//! instance types and the instance profiles that the policies' placeholders are filled from
//! (`templates`, which this binary compiles itself, as the crate keeps it private).

use expect_test::expect;
use serde_json::{Value, json};

#[path = "../../src/envelope/templates.rs"]
mod templates;

use templates::{Data, named};

/// A template, as deploy/aws makes one, of `instance_type`.
fn template(instance_type: &str) -> Value {
    json!({
        "ImageId": "ami-0123",
        "InstanceType": instance_type,
        "IamInstanceProfile": {
            "Arn": "arn:aws:iam::123456789012:instance-profile/pumpkin-sandbox",
        },
        "NetworkInterfaces": [{ "DeviceIndex": 0, "SubnetId": "subnet-a" }],
    })
}

#[test]
fn templates() {
    let mut by_name = template("c6a.4xlarge");
    by_name["IamInstanceProfile"] = json!({ "Name": "pumpkin-sandbox" });
    let mut other = template("c6a.4xlarge");
    other["IamInstanceProfile"] = json!({ "Name": "pumpkin-other" });
    let mut bare = template("c6a.4xlarge");
    bare.as_object_mut().unwrap().remove("InstanceType");
    bare.as_object_mut().unwrap().remove("IamInstanceProfile");
    let cases = [
        ("as deploy/aws makes them", template("c6a.4xlarge")),
        ("a profile by its name", by_name),
        ("another profile", other),
        ("no instance type or profile", bare),
    ];
    let lines = cases.map(|(case, large)| {
        let data: Vec<Data> = [template("c6a.2xlarge"), large]
            .into_iter()
            .map(|template| serde_json::from_value(template).unwrap())
            .collect();
        format!("{case}: {:?}", named(&data))
    });
    expect![[r#"
        as deploy/aws makes them: Named { instance_types: {"c6a.2xlarge", "c6a.4xlarge"}, profiles: {"pumpkin-sandbox"} }
        a profile by its name: Named { instance_types: {"c6a.2xlarge", "c6a.4xlarge"}, profiles: {"pumpkin-sandbox"} }
        another profile: Named { instance_types: {"c6a.2xlarge", "c6a.4xlarge"}, profiles: {"pumpkin-other", "pumpkin-sandbox"} }
        no instance type or profile: Named { instance_types: {"c6a.2xlarge"}, profiles: {"pumpkin-sandbox"} }"#]].assert_eq(&lines.join("\n"));
}
