//! Requires Docker. Run with: cargo test --test ftp_integration -- --ignored
use ferry::ftp::Ftp;
#[allow(dead_code)]
mod support;

#[test]
#[ignore]
fn connect_list_upload_download() {
    let fixture = support::start_ftp();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    let path = support::remote_path("hello.txt");
    ftp.upload_bytes(&path, b"hello\n").unwrap();
    let listed = ftp.list(support::REMOTE_ROOT).unwrap();
    assert!(listed.iter().any(|e| e.name == "hello.txt"));
    let bytes = ftp.download(&path).unwrap();
    assert_eq!(bytes, b"hello\n");
}
