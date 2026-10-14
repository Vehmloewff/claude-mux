{
  lib,
  rustPlatform,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "claude-mux";
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      (lib.fileset.difference (lib.fileset.fileFilter (file: file.hasExt "rs") ../.) (lib.fileset.maybeMissing ../target))
      (lib.fileset.maybeMissing ../migrations)
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  doCheck = true;
  preCheck = "export HOME=$TMPDIR";

  meta = {
    description = "Run and manage many Claude Code sessions from one place";
    homepage = "https://github.com/Vehmloewff/claude-mux";
    mainProgram = "claude-mux";
    platforms = lib.platforms.unix;
  };
}
