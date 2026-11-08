#![cfg(feature = "device-test")]
//! Process-level IPC regressions require uinput permissions. No test emits
//! keys.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    Ok(stream)
}

fn exchange(stream: &mut UnixStream, commands: &str, count: usize) -> Vec<String> {
    stream.write_all(commands.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    (0..count)
        .map(|_| {
            let mut reply = String::new();
            reader.read_line(&mut reply).unwrap();
            assert!(!reply.is_empty(), "connection closed before replying");
            reply
        })
        .collect()
}

fn send(socket: &Path, command: &str) -> String {
    exchange(&mut connect(socket).unwrap(), &format!("{command}\n"), 1)
        .pop()
        .unwrap()
}

struct DaemonGuard {
    child: Child,
    dir: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    log: PathBuf,
}

impl DaemonGuard {
    fn start() -> Self {
        Self::start_with_config("", false)
    }

    fn start_with_config(extra: &str, watch_config: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("xremap-ipc-{}-{:x}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.yml");
        let socket = dir.join("test.sock");
        let log = dir.join("daemon.log");
        std::fs::write(&config, format!("{extra}ipc:\n  socket: {}\n", socket.display())).unwrap();
        let logfile = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_xremap"));
        command
            .arg(&config)
            .args([
                "--device",
                "nonexistent-ipc-test-device",
                if watch_config {
                    "--watch=config,device"
                } else {
                    "--watch=device"
                },
                "--desktop",
                "none",
            ])
            .stdout(Stdio::from(logfile.try_clone().unwrap()))
            .stderr(Stdio::from(logfile));
        let child = command.spawn().unwrap();
        let mut guard = Self {
            child,
            dir,
            config,
            socket: socket.clone(),
            log,
        };
        guard.wait_for_socket(&socket);
        guard
    }

    fn wait_for_log(&mut self, message: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(self.child.try_wait().unwrap().is_none(), "daemon exited");
            let log = std::fs::read_to_string(&self.log).unwrap();
            if log.matches(message).count() >= count {
                return;
            }
            assert!(Instant::now() < deadline, "did not see {message:?}: {log}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("daemon exited with {status}: {}", std::fs::read_to_string(&self.log).unwrap());
            }
            if let Ok(mut stream) = connect(socket) {
                assert_eq!(exchange(&mut stream, "ping\n", 1), ["pong\n"]);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon/socket not ready: {}",
                std::fs::read_to_string(&self.log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn socket_survives_a_config_reload() {
    let mut daemon = DaemonGuard::start();
    let mut stream = connect(&daemon.socket).unwrap();
    // One write forces set/get/reload/get to share the same input batch.
    assert_eq!(
        exchange(&mut stream, "set-mode after_reload\nget-mode\nreload-config\nget-mode\nping\n", 5),
        ["ok\n", "after_reload\n", "ok\n", "after_reload\n", "pong\n"]
    );

    // A failed reload must be an error reply, retaining the old configuration,
    // mode, listener, and the remaining commands in this same batch.
    std::fs::write(&daemon.config, "ipc: [\n").unwrap();
    let replies = exchange(&mut stream, "reload-config\nget-mode\nping\n", 3);
    assert!(replies[0].starts_with("err Failed to load config:"), "{replies:?}");
    assert_eq!(&replies[1..], ["after_reload\n", "pong\n"]);

    // A bind failure must also keep the current socket usable.
    let invalid_path = daemon.dir.join("missing/test.sock");
    std::fs::write(&daemon.config, format!("ipc:\n  socket: {}\n", invalid_path.display())).unwrap();
    assert!(send(&daemon.socket, "reload-config").starts_with("err Failed to start IPC server"));
    assert_eq!(send(&daemon.socket, "ping"), "pong\n");

    // Successful path changes reply over the old connection, then accept new
    // clients at the new path and unlink the previous socket file.
    let socket2 = daemon.dir.join("test2.sock");
    std::fs::write(&daemon.config, format!("ipc:\n  socket: {}\n", socket2.display())).unwrap();
    assert_eq!(send(&daemon.socket, "reload-config"), "ok\n");
    daemon.wait_for_socket(&socket2);
    assert!(!daemon.socket.exists());
    assert_eq!(send(&socket2, "get-mode"), "after_reload\n");
}

#[test]
fn mode_replies_escape_config_values_without_desynchronizing_clients() {
    let daemon = DaemonGuard::start_with_config("default_mode: \"first\\nsecond\\rthird\\\\four\"\n", false);
    let mut stream = connect(&daemon.socket).unwrap();
    let replies = exchange(&mut stream, "get-mode\nping\nget-mode\n", 3);
    assert_eq!(
        replies,
        [
            "first\\nsecond\\rthird\\\\four\n",
            "pong\n",
            "first\\nsecond\\rthird\\\\four\n"
        ]
    );
}

#[test]
fn client_accepts_a_complete_command_as_one_argument() {
    let daemon = DaemonGuard::start();
    let output = Command::new(env!("CARGO_BIN_EXE_xremap"))
        .arg("--socket")
        .arg(&daemon.socket)
        .arg("--client")
        .arg("set-mode normal")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
    assert_eq!(send(&daemon.socket, "get-mode"), "normal\n");
}
