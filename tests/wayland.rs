//! Exercises Stash against the compositor selected by the test environment.
//!
//! To play nicely with, e.g., Nixpkgs test suite (at least while building)
//! those tests will skip outside a Wayland session. We use the real session
//! clipboard for the tests and must therefore remain serialized in one test.
#![cfg(target_os = "linux")]

use std::{
  env,
  fs,
  os::unix::fs::symlink,
  path::{Path, PathBuf},
  process::{Child, Command, Output, Stdio},
  thread,
  time::{Duration, Instant},
};

use tempfile::TempDir;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WATCH_TIMEOUT: Duration = Duration::from_secs(10);

struct StashBinaries {
  root:  TempDir,
  stash: PathBuf,
}

impl StashBinaries {
  fn new() -> Self {
    let root =
      tempfile::tempdir().expect("failed to create temporary directory");
    let stash = PathBuf::from(env!("CARGO_BIN_EXE_stash"));
    let bin = root.path().join("bin");
    fs::create_dir(&bin).expect("failed to create temporary bin directory");

    for name in ["wl-copy", "wl-paste"] {
      symlink(&stash, bin.join(name))
        .expect("failed to create multicall binary symlink");
    }

    Self { root, stash }
  }

  fn multicall(&self, name: &str) -> Command {
    Command::new(self.root.path().join("bin").join(name))
  }

  fn stash(&self) -> Command {
    Command::new(&self.stash)
  }
}

struct Watcher(Child);

impl Drop for Watcher {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

fn compositor_available(binaries: &StashBinaries) -> bool {
  let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") else {
    return false;
  };
  let Some(display) = env::var_os("WAYLAND_DISPLAY") else {
    return false;
  };

  if !Path::new(&runtime_dir).join(display).exists() {
    return false;
  }

  let output = binaries
    .multicall("wl-paste")
    .arg("--list-types")
    .output()
    .expect("failed to probe Wayland compositor");
  if output.status.success() {
    return true;
  }

  let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
  !error.contains("couldn't connect")
    && !error.contains("could not find wayland compositor")
    && !error.contains("no seats available")
}

fn run(command: &mut Command, input: &[u8]) -> Output {
  command
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .and_then(|mut child| {
      use std::io::Write;

      child
        .stdin
        .take()
        .expect("child stdin must be piped")
        .write_all(input)?;
      child.wait_with_output()
    })
    .expect("failed to run stash command")
}

fn assert_success(output: &Output) {
  assert!(
    output.status.success(),
    "command failed: {}",
    String::from_utf8_lossy(&output.stderr),
  );
}

fn copy(binaries: &StashBinaries, args: &[&str], contents: &[u8]) {
  let output = run(binaries.multicall("wl-copy").args(args), contents);
  assert_success(&output);
}

fn paste(binaries: &StashBinaries, args: &[&str]) -> Output {
  binaries
    .multicall("wl-paste")
    .args(args)
    .output()
    .expect("failed to paste clipboard contents")
}

#[test]
fn real_wayland_clipboard_round_trip_and_watch() {
  let binaries = StashBinaries::new();
  if !compositor_available(&binaries) {
    eprintln!("skipping Wayland integration test: no compositor available");
    return;
  }

  let binary_mime = "application/x-stash-integration";
  let binary_contents = [0, 255, b's', b't', b'a', b's', b'h', b'\n'];
  copy(&binaries, &["--type", binary_mime], &binary_contents);

  let types = paste(&binaries, &["--list-types"]);
  assert_success(&types);
  assert!(
    String::from_utf8_lossy(&types.stdout)
      .lines()
      .any(|mime| mime == binary_mime),
    "custom MIME type was not advertised: {}",
    String::from_utf8_lossy(&types.stdout),
  );

  let pasted = paste(&binaries, &["--no-newline", "--type", binary_mime]);
  assert_success(&pasted);
  assert_eq!(pasted.stdout, binary_contents);

  let text = b"stash integration text";
  copy(&binaries, &["--type", "text/plain"], text);
  let pasted = paste(&binaries, &["--no-newline"]);
  assert_success(&pasted);
  assert_eq!(pasted.stdout, text);
  let pasted = paste(&binaries, &[]);
  assert_success(&pasted);
  assert_eq!(pasted.stdout, [text.as_slice(), b"\n"].concat());

  let db = binaries.root.path().join("watch.sqlite");
  let _watcher = Watcher(
    binaries
      .stash()
      .args(["--db-path"])
      .arg(&db)
      .args(["watch", "--mime-type", "text"])
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .expect("failed to start stash watch"),
  );

  // Let the watcher read the current clipboard as its baseline before the
  // next copy, otherwise it may correctly treat the test value as preexisting.
  thread::sleep(Duration::from_secs(1));
  let watched_text = b"stash watch integration text";
  copy(&binaries, &["--type", "text/plain"], watched_text);

  let deadline = Instant::now() + WATCH_TIMEOUT;
  loop {
    let output = binaries
      .stash()
      .args(["--db-path"])
      .arg(&db)
      .args(["list", "--format", "json"])
      .output()
      .expect("failed to query watcher database");
    assert_success(&output);

    if String::from_utf8_lossy(&output.stdout)
      .contains("stash watch integration text")
    {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "watch did not persist clipboard contents within {WATCH_TIMEOUT:?}",
    );
    thread::sleep(POLL_INTERVAL);
  }
}
