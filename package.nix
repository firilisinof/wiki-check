{
  lib,
  rustPlatform,
  makeWrapper,
  git,
}:

rustPlatform.buildRustPackage {
  pname = "wiki-check";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./src
      ./Cargo.toml
      ./Cargo.lock
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];
  nativeCheckInputs = [ git ];

  postInstall = ''
    wrapProgram $out/bin/wiki-check --prefix PATH : ${lib.makeBinPath [ git ]}
  '';

  meta = {
    description = "Validation for a personal Markdown wiki kept in Git";
    license = lib.licenses.mit;
    mainProgram = "wiki-check";
    platforms = lib.platforms.unix;
  };
}
