// Reference documentation:
// https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_data_device
// https://docs.rs/wl-clipboard-rs/latest/wl_clipboard_rs
// https://github.com/YaLTeR/wl-clipboard-rs/blob/master/wl-clipboard-rs-tools/src/bin/wl_copy.rs
pub mod wl_copy;
pub mod wl_paste;

use std::env;

/// Extract the base name from argv[0].
fn get_base(argv0: &str) -> &str {
  std::path::Path::new(argv0)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("")
}

/// Dispatch multicall binary logic based on `argv[0]`.
/// Returns `true` if a multicall command was handled and the process should
/// exit.
pub fn multicall_dispatch() -> bool {
  let argv0 = env::args().next().unwrap_or_else(|| {
    log::warn!("unable to determine program name");
    String::new()
  });
  let base = get_base(&argv0);
  match base {
    "stash-copy" | "wl-copy" => {
      if let Err(e) = wl_copy::wl_copy_main() {
        log::error!("copy failed: {e}");
        std::process::exit(1);
      }
      true
    },
    "stash-paste" | "wl-paste" => {
      if let Err(e) = wl_paste::wl_paste_main() {
        log::error!("paste failed: {e}");
        std::process::exit(1);
      }
      true
    },
    _ => false,
  }
}
