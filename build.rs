use std::{env, fs, path::Path};

/// List of multicall symlinks to create (name, target)
const MULTICALL_LINKS: &[&str] =
  &["stash-copy", "stash-paste", "wl-copy", "wl-paste"];

fn main() {
  // Only run on Unix-like systems
  #[cfg(not(unix))]
  {
    println!(
      "cargo:warning=Multicall symlinks are only supported on Unix-like \
       systems."
    );
    return;
  }

  // OUT_DIR is something like .../target/debug/build/<pkg>/out
  // We want .../target/debug or .../target/release
  let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
  let bin_dir = Path::new(&out_dir)
    .ancestors()
    .nth(3)
    .expect("Failed to find binary dir");

  // Path to the main stash binary
  let stash_bin = bin_dir.join("stash");

  // Create symlinks for each multicall binary
  for link in MULTICALL_LINKS {
    let link_path = bin_dir.join(link);
    // Remove existing symlink or file if present
    let _ = fs::remove_file(&link_path);
    #[cfg(unix)]
    {
      use std::os::unix::fs::symlink;
      match symlink(&stash_bin, &link_path) {
        Ok(()) => {
          println!(
            "cargo:warning=Created symlink: {} -> {}",
            link_path.display(),
            stash_bin.display()
          );
        },
        Err(e) => {
          println!(
            "cargo:warning=Failed to create symlink {} -> {}: {}",
            link_path.display(),
            stash_bin.display(),
            e
          );
        },
      }
    }
  }
}
