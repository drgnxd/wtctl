{
  lib,
  rustPlatform,
  git,
  git-annex,
  lsof,
}:

rustPlatform.buildRustPackage {
  pname = "wtctl";
  version = "0.1.0";
  # A local `target/` must never enter the source hash.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  nativeCheckInputs = [
    git
    git-annex
    lsof
  ];
  checkFeatures = [ "test-agent-names" ];

  meta = {
    description = "Task worktree lifecycle for parallel coding agents";
    license = lib.licenses.mit;
    mainProgram = "wtctl";
    platforms = [
      "aarch64-darwin"
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
