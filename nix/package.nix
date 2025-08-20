{
  lib,
  craneLib,
}: let
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

  cargoArtifacts = craneLib.buildDepsOnly {
    name = "${pname}-deps";
    strictDeps = true;
    inherit src;
  };
in
  craneLib.buildPackage {
    inherit pname src version cargoArtifacts;

    strictDeps = true;

    # Install Systemd service for Stash into $out/share.
    # This can be used to use Stash in 'systemd.packages'
    postInstall = ''
      mkdir -p $out
      install -Dm755 ${../vendor/stash.service} $out/share/stash.service
    '';

    meta = {
      description = "Wayland clipboard manager with fast persistent history and multi-media support";
      homepage = "https://github.com/notashelf/stash";
      license = lib.licenses.mpl20;
      maintainers = [lib.maintainers.NotAShelf];
      mainProgram = "stash";
    };
  }
