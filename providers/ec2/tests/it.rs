//! What the EC2 provider holds that no instance is needed to check: AWS's certificates of the keys
//! that sign its instances' identity documents, each pinned by its SHA-256 fingerprint, as
//! `openssl x509 -noout -fingerprint -sha256` shows it, to compare with AWS's page of them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ec2::IDENTITY_CERTIFICATES;
use expect_test::expect;
use ring::digest::{SHA256, digest};

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
    let pinned: Vec<String> = (IDENTITY_CERTIFICATES.iter())
        .map(|(region, certificate)| {
            let base64: String = (certificate.lines())
                .filter(|line| !line.starts_with("-----"))
                .collect();
            let fingerprint = digest(&SHA256, &STANDARD.decode(base64).unwrap());
            let hex: Vec<String> = (fingerprint.as_ref().iter())
                .map(|byte| format!("{byte:02X}"))
                .collect();
            format!("{region}: {}", hex.join(":"))
        })
        .collect();
    expect![[r#"
        af-south-1: 50:9A:6F:F1:27:D3:54:04:47:F9:96:94:17:5D:59:57:48:B6:34:E3:E7:6F:1D:06:67:1C:5E:40:FD:44:FE:33
        ap-east-1: B1:CF:4F:5C:06:96:CC:FE:3D:CA:E0:22:77:13:0B:17:ED:1A:7F:0D:0D:E8:9B:0B:AA:23:EA:A2:58:93:CB:32
        ap-east-2: 3B:33:7E:11:34:2D:A2:4F:1B:A8:D3:53:45:F0:74:F0:1C:97:16:95:7E:93:83:4C:00:01:96:10:F7:BD:F7:FD
        ap-northeast-1: 67:D2:8C:01:FC:23:C2:F0:DF:9A:08:4A:B3:92:19:DC:E4:C2:79:AB:26:EF:A5:0A:A4:A3:0E:73:36:85:E0:3B
        ap-northeast-2: F4:BC:3E:8B:D6:54:0B:FF:1A:5F:C1:49:39:33:FD:EC:B5:CF:CA:8D:8A:32:22:B6:1B:9A:F8:81:2E:67:C9:9B
        ap-northeast-3: F9:3F:18:9A:54:63:A5:63:90:67:5F:DC:91:88:DB:A7:FA:7B:53:1C:38:95:44:0E:A0:81:D3:25:62:55:A8:B0
        ap-south-1: 63:00:69:62:01:25:C4:6E:15:40:A2:2D:00:10:50:78:AF:48:E5:36:A4:83:21:87:92:9C:EA:B5:EF:F7:C6:CC
        ap-south-2: 79:2E:CF:88:30:1B:EE:08:59:C9:F1:8C:90:82:00:A8:CE:27:DF:F0:C2:27:FE:9E:FC:F4:1F:4A:50:04:DA:0E
        ap-southeast-1: DF:0E:83:6F:8A:6A:68:34:63:EB:5E:E4:7E:64:2F:C0:AA:89:B5:1E:83:82:C5:A6:8E:E9:C8:53:97:43:96:F7
        ap-southeast-2: 8C:9B:35:CC:96:28:9D:18:F3:21:8A:B7:8C:36:93:48:3F:80:03:8E:EE:87:4B:E4:71:7C:DC:9B:8D:5B:38:D4
        ap-southeast-3: 76:18:1C:76:66:51:38:BE:BE:6C:23:DE:C3:08:66:30:A9:3E:2F:0E:D0:6E:7D:0F:06:E7:D2:C2:EF:6D:BB:1E
        ap-southeast-4: EF:58:FE:E8:48:5A:10:93:92:56:EF:90:E9:49:BD:5F:89:74:BB:31:10:2E:CC:87:15:67:F6:F3:17:57:9D:13
        ap-southeast-5: 11:EC:D2:F8:15:13:83:06:6A:5C:57:6D:F2:92:D5:53:04:9C:08:59:F0:D4:DE:FE:C7:CE:C4:A6:26:20:E4:D0
        ap-southeast-6: 31:F3:F9:33:15:72:92:59:F3:92:64:C4:D6:FF:7E:6C:42:F7:09:90:8D:AB:70:0A:B0:58:72:05:36:6C:46:A7
        ap-southeast-7: CB:EE:63:D1:0F:E3:C7:08:88:7C:07:FD:74:6B:6D:9A:DA:FA:E9:45:73:84:6F:48:0F:52:9A:A1:0A:BF:C6:BC
        ca-central-1: F1:9F:4F:C6:FB:16:85:B9:7E:71:E3:E3:56:A9:33:C9:B1:01:CB:22:46:45:E7:2D:0A:40:12:56:23:3F:48:ED
        ca-west-1: BC:DB:D4:69:B0:71:B3:D3:0B:7D:FF:3A:F6:34:C5:C4:17:71:C6:DD:7E:54:0B:A8:10:51:75:42:97:9B:50:7C
        cn-north-1: 68:74:24:D4:5C:66:13:7D:6F:8C:D5:7E:3A:29:C7:5F:B2:88:1B:E6:47:3F:F4:20:FB:AF:19:11:DF:05:FC:7A
        cn-northwest-1: 68:74:24:D4:5C:66:13:7D:6F:8C:D5:7E:3A:29:C7:5F:B2:88:1B:E6:47:3F:F4:20:FB:AF:19:11:DF:05:FC:7A
        eu-central-1: 99:43:5C:EF:CA:ED:AF:26:2D:FF:8C:AE:B6:A7:A3:8E:AF:C7:C1:1C:A7:98:8B:07:99:72:4B:1C:9B:9A:0C:AF
        eu-central-2: E8:83:87:14:8E:C9:B2:99:6D:64:73:08:E3:68:D0:19:47:FF:BF:B6:25:83:D0:30:CE:28:B6:5A:08:E1:0D:E1
        eu-north-1: E5:58:46:43:2E:CC:E6:0F:1D:31:37:7A:D6:05:A6:35:5B:4C:B5:B4:EA:41:4A:54:98:60:71:51:F5:82:C1:E2
        eu-south-1: D3:3D:2D:21:F4:15:4F:44:B9:F6:AE:5B:68:78:9D:51:D7:97:45:3B:48:FF:BD:FC:26:33:33:53:B0:69:71:D5
        eu-south-2: 17:37:7B:03:33:60:17:D1:54:6A:1A:18:1D:53:B1:61:39:01:74:1D:43:8D:F2:71:BC:BA:B7:DB:87:9A:2E:47
        eu-west-1: 70:92:99:3E:FE:38:80:0E:ED:D6:9D:EF:9C:F3:4B:73:4A:C8:28:CC:E3:0E:F9:43:42:38:8B:C7:A5:48:82:7D
        eu-west-2: 7B:57:07:EB:89:16:6F:F0:7A:42:9B:74:DE:90:A8:06:9D:55:FA:3C:33:C9:45:F5:B2:1E:A3:6A:E6:7D:08:AD
        eu-west-3: DD:45:CA:F9:28:C8:28:FA:0A:3A:96:08:2F:C2:DA:C9:9E:0E:22:E3:B4:05:96:31:9F:B1:A9:5F:3E:8D:0D:D2
        eusc-de-east-1: 70:ED:FB:69:A3:7B:5E:FE:DC:CA:6A:68:CE:41:F9:A9:C2:17:38:D9:AF:13:1C:C3:F1:E0:65:ED:4E:62:72:7B
        il-central-1: 7D:97:95:C3:3B:48:A0:01:94:B4:FB:A5:B6:4F:0E:11:76:6C:66:22:00:95:6A:2B:FA:21:84:FC:9A:52:50:E8
        me-central-1: 6B:0A:F8:0A:38:4D:E2:81:A8:B9:28:5D:5F:33:B9:B4:D2:6A:4A:CB:8C:DB:FD:9A:C4:78:E6:FF:B8:30:F7:56
        me-south-1: 28:00:00:67:2B:F2:5F:A7:C3:11:33:EB:7A:4A:0C:89:31:F7:13:A4:D5:F4:F1:FB:66:9B:7D:8A:43:0F:BC:F8
        mx-central-1: 35:42:E0:11:68:C9:21:D2:A5:95:00:E4:00:24:1B:25:F3:B2:64:3B:F5:0A:99:5D:E9:CF:40:29:AD:F8:4D:0B
        sa-east-1: C0:6C:2C:C6:87:E3:FB:E7:CB:38:FA:C8:6C:86:46:5D:91:C8:58:E3:14:63:3A:AA:F4:DD:12:AD:0E:AE:D3:A6
        us-east-1: 65:02:D1:18:33:53:7B:9C:F0:A6:4D:96:D9:83:CA:76:FE:31:7D:3E:C7:5A:82:E5:94:0C:A0:53:90:EA:A5:9F
        us-east-2: 4C:8E:43:C5:7E:10:04:AC:FB:E0:3C:2B:9C:11:4C:71:C2:5D:B0:DC:FB:2E:60:65:2A:1D:C4:8B:8F:4A:C2:A6
        us-gov-east-1: AC:86:33:55:68:56:89:45:FB:07:FB:74:21:E8:BE:6B:B5:7B:D2:C1:54:CE:9B:9C:B8:99:6F:86:E8:A5:4B:27
        us-gov-west-1: FB:6B:DE:DA:4F:10:3E:7B:85:E4:2C:3A:B0:62:BB:9F:7A:09:C4:15:E7:C8:E5:EC:4C:8F:16:01:B3:CE:2E:63
        us-west-1: CB:58:43:D1:CE:7C:F0:A8:2D:40:C4:BB:D8:50:61:B6:E8:4D:74:0E:C5:2A:E9:99:DD:00:D6:00:ED:C8:38:27
        us-west-2: A7:52:02:A6:4F:F3:C4:02:2E:6D:97:EB:91:E5:F4:33:E8:1A:F9:AA:3E:04:40:77:96:D1:92:85:1F:37:3A:20"#]].assert_eq(&pinned.join("\n"));
}
