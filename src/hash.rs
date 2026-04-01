/// FNV-1a hasher for deterministic hashing across process runs.
///
/// Unlike `std::collections::hash_map::DefaultHasher` (which uses SipHash
/// with a random seed), this produces stable hashes suitable for persistent
/// storage and cross-process comparison.
///
/// # Example
///
/// ```
/// use std::hash::Hasher;
///
/// use stash::hash::Fnv1aHasher;
///
/// let mut hasher = Fnv1aHasher::new();
/// hasher.write(b"hello");
/// let hash = hasher.finish();
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Fnv1aHasher {
  state: u64,
}

impl Fnv1aHasher {
  const FNV_OFFSET: u64 = 0xCBF29CE484222325;
  const FNV_PRIME: u64 = 0x100000001B3;

  /// Creates a new hasher initialized with the FNV-1a offset basis.
  #[must_use]
  pub fn new() -> Self {
    Self {
      state: Self::FNV_OFFSET,
    }
  }
}

impl Default for Fnv1aHasher {
  fn default() -> Self {
    Self::new()
  }
}

impl std::hash::Hasher for Fnv1aHasher {
  fn write(&mut self, bytes: &[u8]) {
    for byte in bytes {
      self.state ^= u64::from(*byte);
      self.state = self.state.wrapping_mul(Self::FNV_PRIME);
    }
  }

  fn finish(&self) -> u64 {
    self.state
  }
}

#[cfg(test)]
mod tests {
  use std::hash::Hasher;

  use super::*;

  #[test]
  fn test_fnv1a_basic() {
    let mut hasher = Fnv1aHasher::new();
    hasher.write(b"hello");
    // FNV-1a hash for "hello" (little-endian u64)
    assert_eq!(hasher.finish(), 0xA430D84680AABD0B);
  }

  #[test]
  fn test_fnv1a_empty() {
    let hasher = Fnv1aHasher::new();
    // Empty input should return offset basis
    assert_eq!(hasher.finish(), Fnv1aHasher::FNV_OFFSET);
  }

  #[test]
  fn test_fnv1a_deterministic() {
    // Same input must produce same hash
    let mut h1 = Fnv1aHasher::new();
    let mut h2 = Fnv1aHasher::new();
    h1.write(b"test data");
    h2.write(b"test data");
    assert_eq!(h1.finish(), h2.finish());
  }

  #[test]
  fn test_default_trait() {
    let h1 = Fnv1aHasher::new();
    let h2 = Fnv1aHasher::default();
    assert_eq!(h1.finish(), h2.finish());
  }

  #[test]
  fn test_copy_trait() {
    let mut hasher = Fnv1aHasher::new();
    hasher.write(b"data");
    let copied = hasher;
    // Both should have same state after copy
    assert_eq!(hasher.finish(), copied.finish());
  }
}
