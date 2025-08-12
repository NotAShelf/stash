{
  mkShell,
  rust-analyzer,
  rustfmt,
  clippy,
  cargo,
  rustPlatform,
}:
mkShell {
  name = "rust";
  packages = [
    rust-analyzer
    rustfmt
    clippy
    cargo
  ];

  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
}
