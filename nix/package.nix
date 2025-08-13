{
  lib,
  craneLib,
  runCommandNoCCLocal,
}: let
  inherit (craneLib) buildDepsOnly buildPackage mkDummySrc;

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

  # basically avoid crane rebuilding everything
  # when the package version changes
  replacedSrc = let
    rgxIn = ''
      name = "${pname}"
      version = "${version}"
    '';
    rgxOut = ''
      name = "${pname}"
      version = "0.9.6"
    '';
  in
    runCommandNoCCLocal "bakaSrc" {} ''
      cp -r ${src} $out
      substituteInPlace $out/Cargo.toml \
         --replace-fail '${rgxIn}' '${rgxOut}'
      substituteInPlace $out/Cargo.lock \
         --replace-fail '${rgxIn}' '${rgxOut}'
    '';

  cargoArtifacts = buildDepsOnly {
    name = "${pname}-deps";
    strictDeps = true;
    dummySrc = mkDummySrc {src = replacedSrc;};
  };
in
  buildPackage {
    inherit cargoArtifacts pname src version;
    strictDeps = true;
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
  }
