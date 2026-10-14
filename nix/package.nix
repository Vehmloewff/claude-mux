{
  lib,
  stdenv,
  rustPlatform,
  callPackage,
  zig_0_16,
  darwin,
  xcbuild,
  ghostty-src,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
  # Ghostty's own zon2nix output: its Zig dependencies, pre-fetched, so the
  # libghostty-vt-sys build script doesn't need network access.
  zigDeps = callPackage "${ghostty-src}/build.zig.zon.nix" { name = "ghostty-zig-deps"; };
in
rustPlatform.buildRustPackage {
  pname = "claudex";
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

  cargoLock = {
    lockFile = ../Cargo.lock;
    outputHashes."libghostty-vt-0.2.1" = "sha256-XepsHhhauHZnaz8ZpWNaas/7sI/22VoH7b3+0dP4NJk=";
  };

  # Zig is only invoked by the libghostty-vt-sys build script; keep its setup
  # hook from taking over the build/check/install phases.
  nativeBuildInputs = [
    zig_0_16
  ]
  # Zig locates the macOS SDK through xcrun, as in Ghostty's own Nix build.
  ++ lib.optionals stdenv.hostPlatform.isDarwin [
    darwin.cctools
    xcbuild
  ];
  dontUseZigBuild = true;
  dontUseZigCheck = true;
  dontUseZigInstall = true;

  preBuild = ''
    export HOME="$TMPDIR"
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
    cp -r ${ghostty-src} "$TMPDIR/ghostty-src"
    chmod -R u+w "$TMPDIR/ghostty-src"
    export GHOSTTY_SOURCE_DIR="$TMPDIR/ghostty-src"
    export GHOSTTY_ZIG_SYSTEM_DIR=${zigDeps}
  '';

  # The test suite drives libghostty only; keep it in the build.
  doCheck = true;
  preCheck = "export HOME=$TMPDIR";

  meta = {
    description = "Run and manage many Claude Code sessions from one place";
    homepage = "https://github.com/Vehmloewff/claudex";
    mainProgram = "claudex";
    platforms = lib.platforms.unix;
  };
}
