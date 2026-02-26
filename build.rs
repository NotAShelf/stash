use std::{env, fs, path::Path};

/// List of multicall symlinks to create (name, target)
const MULTICALL_LINKS: &[&str] =
  &["stash-copy", "stash-paste", "wl-copy", "wl-paste"];

/// Wayland-specific symlinks that can be disabled separately
const WAYLAND_LINKS: &[&str] = &["wl-copy", "wl-paste"];

fn main() {
  // OUT_DIR is something like .../target/debug/build/<pkg>/out
  // We want .../target/debug or .../target/release
  let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
  let bin_dir = Path::new(&out_dir)
    .ancestors()
    .nth(3)
    .expect("Failed to find binary dir");

  // Path to the main stash binary
  let stash_bin = bin_dir.join("stash");

  // Check for environment variables to disable symlinking
  let disable_all_symlinks = env::var("STASH_NO_SYMLINKS").is_ok();
  let disable_wayland_symlinks = env::var("STASH_NO_WL_SYMLINKS").is_ok();

  // Create symlinks for each multicall binary
  for link in MULTICALL_LINKS {
    if disable_all_symlinks {
      println!("cargo:warning=Skipping symlink {link} (all symlinks disabled)");
      continue;
    }

    if disable_wayland_symlinks && WAYLAND_LINKS.contains(link) {
      println!(
        "cargo:warning=Skipping symlink {link} (wayland symlinks disabled)"
      );
      continue;
    }

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
