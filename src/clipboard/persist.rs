use std::{
  process::exit,
  sync::atomic::{AtomicI32, Ordering},
};

use wl_clipboard_rs::copy::{
  ClipboardType,
  MimeType as CopyMimeType,
  Options,
  PreparedCopy,
  ServeRequests,
  Source,
};

/// Maximum number of paste requests to serve before exiting. This (hopefully)
/// prevents runaway processes while still providing persistence.
const MAX_SERVE_REQUESTS: usize = 1000;

/// PID of the current clipboard persistence child process. Used to detect when
/// clipboard content is from our own serve process.
static SERVING_PID: AtomicI32 = AtomicI32::new(0);

/// Get the current serving PID if any. Used by the watch loop to avoid
/// duplicate persistence processes.
pub fn get_serving_pid() -> Option<i32> {
  let pid = SERVING_PID.load(Ordering::SeqCst);
  if pid != 0 { Some(pid) } else { None }
}

/// Result type for persistence operations.
pub type PersistenceResult<T> = Result<T, PersistenceError>;

/// Errors that can occur during clipboard persistence.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
  #[error("Failed to prepare copy: {0}")]
  PrepareFailed(String),

  #[error("Failed to fork: {0}")]
  ForkFailed(String),

  #[error("Clipboard data too large: {0} bytes")]
  DataTooLarge(usize),

  #[error("Clipboard content is empty")]
  EmptyContent,

  #[error("No MIME types to offer")]
  NoMimeTypes,
}

/// Clipboard data with all MIME types for persistence.
#[derive(Debug, Clone)]
pub struct ClipboardData {
  /// The actual clipboard content.
  pub content: Vec<u8>,

  /// All MIME types offered by the source. Preserves order.
  pub mime_types: Vec<String>,

  /// The MIME type that was selected for storage.
  pub selected_mime: String,
}

impl ClipboardData {
  /// Create new clipboard data.
  pub fn new(
    content: Vec<u8>,
    mime_types: Vec<String>,
    selected_mime: String,
  ) -> Self {
    Self {
      content,
      mime_types,
      selected_mime,
    }
  }

  /// Check if data is valid for persistence.
  pub fn is_valid(&self) -> Result<(), PersistenceError> {
    const MAX_SIZE: usize = 100 * 1024 * 1024; // 100MB

    if self.content.is_empty() {
      return Err(PersistenceError::EmptyContent);
    }

    if self.content.len() > MAX_SIZE {
      return Err(PersistenceError::DataTooLarge(self.content.len()));
    }

    if self.mime_types.is_empty() {
      return Err(PersistenceError::NoMimeTypes);
    }

    Ok(())
  }
}

/// Persist clipboard data by forking a background process that serves it.
///
/// 1. Prepares a clipboard copy operation with all MIME types
/// 2. Forks a child process
/// 3. The child serves clipboard data indefinitely (until MAX_SERVE_REQUESTS)
/// 4. The parent returns immediately
///
/// # Safety
///
/// This function uses `libc::fork()` which is unsafe. The child process
/// must not modify any shared state or file descriptors.
pub unsafe fn persist_clipboard(data: ClipboardData) -> PersistenceResult<()> {
  // Validate data
  data.is_valid()?;

  // Prepare the copy operation
  let prepared = prepare_clipboard_copy(&data)?;

  // Fork and serve
  unsafe { fork_and_serve(prepared) }
}

/// Prepare a clipboard copy operation with all MIME types.
fn prepare_clipboard_copy(
  data: &ClipboardData,
) -> PersistenceResult<PreparedCopy> {
  let mut opts = Options::new();
  opts.clipboard(ClipboardType::Regular);
  opts.serve_requests(ServeRequests::Only(MAX_SERVE_REQUESTS));
  opts.foreground(true); // we'll fork manually for better control

  // Determine MIME type for the primary offer
  let mime_type = if data.selected_mime.starts_with("text/") {
    CopyMimeType::Text
  } else {
    CopyMimeType::Specific(data.selected_mime.clone())
  };

  // Prepare the copy
  let prepared = opts
    .prepare_copy(Source::Bytes(data.content.clone().into()), mime_type)
    .map_err(|e| PersistenceError::PrepareFailed(e.to_string()))?;

  Ok(prepared)
}

/// Fork a child process to serve clipboard data.
///
/// The child process will:
///
/// 1. Register its process ID with the self-detection module
/// 2. Serve clipboard requests until MAX_SERVE_REQUESTS
/// 3. Exit cleanly
///
/// The parent stores the child `PID` in `SERVING_PID` and returns immediately.
unsafe fn fork_and_serve(prepared: PreparedCopy) -> PersistenceResult<()> {
  // Enable automatic child reaping to prevent zombie processes
  unsafe {
    libc::signal(libc::SIGCHLD, libc::SIG_IGN);
  }

  match unsafe { libc::fork() } {
    0 => {
      // Child process - clear serving PID
      // Look at me. I'm the server now.
      SERVING_PID.store(0, Ordering::SeqCst);
      serve_clipboard_child(prepared);
      exit(0);
    },

    -1 => {
      // Oops.
      Err(PersistenceError::ForkFailed(
        "libc::fork() returned -1".to_string(),
      ))
    },

    pid => {
      // Parent process, store child PID for loop detection
      log::debug!("Forked clipboard persistence process (pid: {pid})");
      SERVING_PID.store(pid, Ordering::SeqCst);
      Ok(())
    },
  }
}

/// Child process entry point for serving clipboard data.
fn serve_clipboard_child(prepared: PreparedCopy) {
  let pid = std::process::id() as i32;
  log::debug!("Clipboard persistence child process started (pid: {pid})");

  // Serve clipboard requests. The PreparedCopy::serve() method blocks and
  // handles all the Wayland protocol interactions internally via
  // wl-clipboard-rs
  match prepared.serve() {
    Ok(()) => {
      log::debug!("Clipboard persistence: serve completed normally");
    },

    Err(e) => {
      log::error!("Clipboard persistence: serve failed: {e}");
      exit(1);
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_clipboard_data_validation() {
    // Valid data
    let valid = ClipboardData::new(
      b"hello".to_vec(),
      vec!["text/plain".to_string()],
      "text/plain".to_string(),
    );
    assert!(valid.is_valid().is_ok());

    // Empty content
    let empty = ClipboardData::new(
      vec![],
      vec!["text/plain".to_string()],
      "text/plain".to_string(),
    );
    assert!(matches!(
      empty.is_valid(),
      Err(PersistenceError::EmptyContent)
    ));

    // No MIME types
    let no_mimes =
      ClipboardData::new(b"hello".to_vec(), vec![], "text/plain".to_string());
    assert!(matches!(
      no_mimes.is_valid(),
      Err(PersistenceError::NoMimeTypes)
    ));

    // Too large
    let huge = ClipboardData::new(
      vec![0u8; 101 * 1024 * 1024], // 101MB
      vec!["text/plain".to_string()],
      "text/plain".to_string(),
    );
    assert!(matches!(
      huge.is_valid(),
      Err(PersistenceError::DataTooLarge(_))
    ));
  }

  #[test]
  fn test_clipboard_data_creation() {
    let data = ClipboardData::new(
      b"test content".to_vec(),
      vec!["text/plain".to_string(), "text/html".to_string()],
      "text/plain".to_string(),
    );

    assert_eq!(data.content, b"test content");
    assert_eq!(data.mime_types.len(), 2);
    assert_eq!(data.selected_mime, "text/plain");
  }
}
