#![cfg(target_os = "linux")]

use std::{
    fs::{self, Permissions},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

use lyteboard_core::ipc::{Client, ClientError, linux};
use lyteboard_protocol::ipc::{self, ErrorCode, Request, Response};
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    task::JoinSet,
    time::{sleep, timeout},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(6);

struct Runtime(TempDir);

impl Runtime {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        Self(dir)
    }

    fn client(&self) -> Client {
        Client::in_runtime_dir(self.0.path()).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lyteboardd"));
        command.env("XDG_RUNTIME_DIR", self.0.path());
        command
    }

    fn app_dir(&self) -> PathBuf {
        self.0.path().join("lyteboard")
    }

    fn socket(&self) -> PathBuf {
        self.app_dir().join("daemon.sock")
    }

    async fn start(&self) -> Daemon {
        let mut daemon = Daemon(
            self.command()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let client = self.client();
        timeout(TEST_TIMEOUT, async {
            loop {
                assert!(
                    daemon.0.try_wait().unwrap().is_none(),
                    "daemon exited before it became ready"
                );
                match client.ping().await {
                    Ok(()) => break,
                    Err(ClientError::Offline) => sleep(Duration::from_millis(20)).await,
                    Err(error) => panic!("unexpected startup error: {error}"),
                }
            }
        })
        .await
        .expect("daemon startup timed out");
        daemon
    }
}

struct Daemon(Child);

impl Daemon {
    async fn wait(&mut self) -> ExitStatus {
        timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon did not exit")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o777
}

#[tokio::test]
async fn answers_ping_status_and_version_with_live_daemon_state() {
    let runtime = Runtime::new();
    let daemon = runtime.start().await;
    let client = runtime.client();
    client.ping().await.unwrap();
    let first = client.status().await.unwrap();
    assert!(first.running);
    assert_eq!(first.version, lyteboard_core::VERSION);
    assert_eq!(first.version, client.version().await.unwrap());
    assert_eq!(first.pid, daemon.0.id());
    assert_eq!(first.protocol_version, ipc::VERSION);
    sleep(Duration::from_secs(1)).await;
    let later = client.status().await.unwrap();
    assert!(later.uptime_seconds > first.uptime_seconds);
    assert_eq!(later.connections_accepted, first.connections_accepted + 2);
    assert_eq!(mode(&runtime.app_dir()), 0o700);
    assert_eq!(mode(&runtime.socket()), 0o600);
}

#[tokio::test]
async fn missing_daemon_is_offline_and_does_not_create_files() {
    let runtime = Runtime::new();
    assert!(matches!(
        runtime.client().status().await,
        Err(ClientError::Offline)
    ));
    assert!(!runtime.app_dir().exists());
}

#[tokio::test]
async fn authorized_shutdown_cleans_its_socket_and_the_same_client_can_reconnect() {
    let runtime = Runtime::new();
    let client = runtime.client();
    let mut first = runtime.start().await;
    let lock_path = runtime.app_dir().join("daemon.lock");
    let lock_inode = fs::metadata(&lock_path).unwrap().ino();

    client.shutdown().await.unwrap();
    assert!(matches!(client.status().await, Err(ClientError::Offline)));
    assert!(first.wait().await.success());
    assert!(!runtime.socket().exists());

    let mut second = runtime.start().await;
    assert_eq!(client.status().await.unwrap().pid, second.0.id());
    assert_eq!(fs::metadata(lock_path).unwrap().ino(), lock_inode);
    client.shutdown().await.unwrap();
    assert!(second.wait().await.success());
}

#[tokio::test]
async fn sigint_and_sigterm_shut_down_cleanly() {
    let runtime = Runtime::new();
    for signal in [Signal::INT, Signal::TERM] {
        let mut daemon = runtime.start().await;
        let pid = Pid::from_raw(daemon.0.id() as i32).unwrap();
        kill_process(pid, signal).unwrap();
        assert!(daemon.wait().await.success());
        assert!(!runtime.socket().exists());
    }
}

#[tokio::test]
async fn a_second_daemon_cannot_replace_the_live_socket() {
    let runtime = Runtime::new();
    let original = runtime.start().await;
    let inode = fs::metadata(runtime.socket()).unwrap().ino();
    let duplicate = runtime.command().output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owns"));
    assert_eq!(
        runtime.client().status().await.unwrap().pid,
        original.0.id()
    );
    assert_eq!(fs::metadata(runtime.socket()).unwrap().ino(), inode);
}

#[tokio::test]
async fn a_listener_without_the_lock_is_preserved() {
    let runtime = Runtime::new();
    fs::create_dir(runtime.app_dir()).unwrap();
    fs::set_permissions(runtime.app_dir(), Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(runtime.socket()).unwrap();
    let inode = fs::metadata(runtime.socket()).unwrap().ino();
    let result = runtime.command().output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("already owns"));
    assert_eq!(fs::metadata(runtime.socket()).unwrap().ino(), inode);
    listener.accept().unwrap();
}

#[tokio::test]
async fn recovers_the_stale_socket_left_by_a_crash() {
    let runtime = Runtime::new();
    let mut crashed = runtime.start().await;
    crashed.0.kill().unwrap();
    assert!(!crashed.wait().await.success());
    assert!(runtime.socket().exists());
    assert!(matches!(
        runtime.client().ping().await,
        Err(ClientError::Offline)
    ));

    let recovered = runtime.start().await;
    assert_eq!(
        runtime.client().status().await.unwrap().pid,
        recovered.0.id()
    );
}

#[tokio::test]
async fn refuses_unsafe_runtime_paths_and_preserves_non_socket_entries() {
    let runtime = Runtime::new();
    fs::set_permissions(runtime.0.path(), Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        runtime.client().ping().await,
        Err(ClientError::Endpoint(
            linux::EndpointError::UnsafePath { .. }
        ))
    ));
    assert!(!runtime.command().output().unwrap().status.success());
    assert!(!runtime.app_dir().exists());
    fs::set_permissions(runtime.0.path(), Permissions::from_mode(0o700)).unwrap();

    let other = Runtime::new();
    symlink(other.0.path(), runtime.app_dir()).unwrap();
    assert!(!runtime.command().output().unwrap().status.success());
    assert!(fs::read_link(runtime.app_dir()).is_ok());
    fs::remove_file(runtime.app_dir()).unwrap();

    fs::create_dir(runtime.app_dir()).unwrap();
    fs::set_permissions(runtime.app_dir(), Permissions::from_mode(0o700)).unwrap();
    fs::write(runtime.socket(), b"leave this file alone").unwrap();
    assert!(!runtime.command().output().unwrap().status.success());
    assert_eq!(
        fs::read(runtime.socket()).unwrap(),
        b"leave this file alone"
    );
}

#[tokio::test]
async fn answers_concurrent_clients() {
    let runtime = Runtime::new();
    let daemon = runtime.start().await;
    let mut requests = JoinSet::new();
    for _ in 0..24 {
        let client = runtime.client();
        requests.spawn(async move { client.status().await.unwrap() });
    }
    while let Some(result) = requests.join_next().await {
        assert_eq!(result.unwrap().pid, daemon.0.id());
    }
}

#[tokio::test]
async fn delayed_status_reads_the_current_connection_count() {
    let runtime = Runtime::new();
    let _daemon = runtime.start().await;
    let mut waiting = UnixStream::connect(runtime.socket()).await.unwrap();
    let client = runtime.client();
    client.ping().await.unwrap();
    let current = client.status().await.unwrap();

    linux::send(&mut waiting, &Request::GetStatus)
        .await
        .unwrap();
    let response: Response = timeout(TEST_TIMEOUT, linux::receive(&mut waiting))
        .await
        .unwrap()
        .unwrap();
    let Response::Status(status) = response else {
        panic!("expected status, got {response:?}")
    };
    assert_eq!(status.connections_accepted, current.connections_accepted);
}

#[tokio::test]
async fn stalled_clients_are_bounded_and_time_out() {
    let runtime = Runtime::new();
    let _daemon = runtime.start().await;
    let mut stalled = Vec::new();
    for _ in 0..32 {
        stalled.push(UnixStream::connect(runtime.socket()).await.unwrap());
    }

    let mut next = UnixStream::connect(runtime.socket()).await.unwrap();
    linux::send(&mut next, &Request::Ping).await.unwrap();
    let mut reply = Box::pin(linux::receive::<Response>(&mut next));
    assert!(
        timeout(Duration::from_millis(100), &mut reply)
            .await
            .is_err()
    );
    assert_eq!(
        timeout(TEST_TIMEOUT, reply).await.unwrap().unwrap(),
        Response::Pong
    );
    assert_eq!(stalled[0].read(&mut [0]).await.unwrap(), 0);
}

#[tokio::test]
async fn rejects_malformed_oversized_and_unsupported_requests() {
    let runtime = Runtime::new();
    let _daemon = runtime.start().await;
    for (length, body, expected) in [
        (1, b"{".as_slice(), ErrorCode::InvalidRequest),
        (0, b"".as_slice(), ErrorCode::InvalidRequest),
        (u32::MAX, b"".as_slice(), ErrorCode::FrameTooLarge),
        (
            29,
            br#"{"version":2,"body":"future"}"#.as_slice(),
            ErrorCode::UnsupportedVersion,
        ),
    ] {
        let mut stream = UnixStream::connect(runtime.socket()).await.unwrap();
        stream.write_u32(length).await.unwrap();
        stream.write_all(body).await.unwrap();
        let response: Response = timeout(TEST_TIMEOUT, linux::receive(&mut stream))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, Response::Error(expected));
    }
    runtime.client().ping().await.unwrap();
}

#[tokio::test]
async fn a_disconnect_or_incomplete_frame_does_not_stop_the_daemon() {
    let runtime = Runtime::new();
    let _daemon = runtime.start().await;
    let mut disconnected = UnixStream::connect(runtime.socket()).await.unwrap();
    disconnected.write_u32(100).await.unwrap();
    disconnected.write_all(b"{").await.unwrap();
    drop(disconnected);

    let mut stalled = UnixStream::connect(runtime.socket()).await.unwrap();
    stalled.write_all(&[0, 0]).await.unwrap();
    runtime.client().ping().await.unwrap();
    assert_eq!(
        timeout(TEST_TIMEOUT, stalled.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    runtime.client().ping().await.unwrap();
}
