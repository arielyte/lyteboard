#![cfg(target_os = "windows")]

use lyteboard_core::ipc::{
    Client,
    windows::{Endpoint, receive},
};
use lyteboard_protocol::ipc::{ErrorCode, Response};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::windows::named_pipe::ClientOptions,
    time::{sleep, timeout},
};

struct Daemon {
    child: Child,
    root: tempfile::TempDir,
}
impl Daemon {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let child = Self::command(&root).spawn().unwrap();
        Self { child, root }
    }
    fn command(root: &tempfile::TempDir) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lyteboardd"));
        cmd.env("LYTEBOARD_RUNTIME_DIR", root.path().join("run"))
            .env("LYTEBOARD_PROFILE", root.path().join("profile"))
            .env("LYTEBOARD_DISCOVERY", "off")
            .env("LYTEBOARD_LISTEN_ADDR", "127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
    fn client(&self) -> Client {
        Client::in_runtime_dir(self.root.path().join("run")).unwrap()
    }
    async fn ready(&mut self) {
        timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "daemon exited during startup"
                );
                if self.client().ping().await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn lifecycle_single_instance_and_malformed_frames() {
    let mut daemon = Daemon::start();
    daemon.ready().await;
    let client = daemon.client();
    let status = client.status().await.unwrap();
    assert_eq!(status.pid, daemon.child.id());
    assert_eq!(client.version().await.unwrap(), lyteboard_core::VERSION);
    let duplicate = Daemon::command(&daemon.root).status().unwrap();
    assert!(!duplicate.success());
    assert_eq!(client.status().await.unwrap().pid, status.pid);
    let endpoint = Endpoint::new(daemon.root.path().join("run")).unwrap();
    let name = endpoint.pipe_name().unwrap();
    for (bytes, expected) in [
        (vec![0, 0, 0, 0], ErrorCode::InvalidRequest),
        (vec![0, 0, 16, 1], ErrorCode::FrameTooLarge),
        (
            [vec![0, 0, 0, 1], vec![b'?']].concat(),
            ErrorCode::InvalidRequest,
        ),
    ] {
        let mut pipe = ClientOptions::new()
            .security_qos_flags(0x00110000)
            .open(&name)
            .unwrap();
        pipe.write_all(&bytes).await.unwrap();
        let reply: Response = timeout(Duration::from_secs(4), receive(&mut pipe))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(reply, Response::Error(code) if code == expected));
    }
    // A stalled client cannot block status or shutdown.
    let mut stalled = ClientOptions::new().open(&name).unwrap();
    stalled.write_all(&[0]).await.unwrap();
    client.ping().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(daemon.child.wait().unwrap().success());
    assert!(matches!(
        client.ping().await,
        Err(lyteboard_core::ipc::ClientError::Offline)
    ));
    daemon.child = Daemon::command(&daemon.root).spawn().unwrap();
    daemon.ready().await;
    client.ping().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(daemon.child.wait().unwrap().success());
}
