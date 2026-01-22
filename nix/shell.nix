{
  mkShell,
  rustc,
  cargo,
  rustfmt,
  clippy,
  taplo,
  rust-analyzer-unwrapped,
  cargo-nextest,
  rustPlatform,
}:
mkShell {
  name = "rust";

  packages = [
    rustc
    cargo

    (rustfmt.override {asNightly = true;})
    clippy
    cargo
    taplo
    rust-analyzer-unwrapped

    # Additional Cargo Tooling
    cargo-nextest
  ];

  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
}
