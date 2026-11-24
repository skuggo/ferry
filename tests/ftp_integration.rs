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

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a local Docker daemon with a reachable bridge network"]
fn active_mode_upload_and_download_from_the_control_peer() {
    let fixture = support::start_ftp();
    // Use the container address so the control and active data connections
    // have the same remote peer, without Docker's published-port proxy.
    let server = fixture.container.get_bridge_ip_address().unwrap();
    let mut ftp = Ftp::connect(&server.to_string(), 21, "test", "testpw", false).unwrap();
    let path = support::remote_path("active.txt");
    let payload = b"active FTP transfer\n";
    ftp.upload_bytes(&path, payload).unwrap();
    assert_eq!(ftp.download(&path).unwrap(), payload);
}
