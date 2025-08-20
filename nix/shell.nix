{
  mkShell,
  rustc,
  cargo,
  rustfmt,
  clippy,
  taplo,
  rust-analyzer-unwrapped,
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
  ];

  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
}
