{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "stash";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;

  src = let
    fs = lib.fileset;
    s = ../.;
  in
    fs.toSource {
      root = s;
      fileset = fs.unions [
        (fs.fileFilter (file: builtins.any file.hasExt ["rs"]) (s + /src))
        (s + /Cargo.lock)
        (s + /Cargo.toml)
      ];
    };

  cargoLock.lockFile = "${finalAttrs.src}/Cargo.lock";
  enableParallelBuilding = true;

  postInstall = ''
    mkdir -p $out
    install -Dm755 ${../vendor/stash.service} $out/share/stash.service
  '';

  meta = {
    description = "Wayland clipboard manager with fast persistent history and multi-media support";
    maintainers = [lib.maintainers.NotAShelf];
    license = lib.licenses.mpl20;
    mainProgram = "stash";
  };
})
