{
  lib,
  rustPlatform,
  fetchurl,
  linkFarm,
  makeWrapper,
  pkg-config,
  oniguruma,
  git,
  openssh,
}:

let
  # sentence-transformers/all-MiniLM-L6-v2, with weights, tokenizer, and
  # configuration pinned to one revision
  modelRevision = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";
  modelFile =
    name: hash:
    fetchurl {
      url = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/${modelRevision}/${name}";
      inherit hash;
    };
  model = linkFarm "all-MiniLM-L6-v2-${builtins.substring 0 7 modelRevision}" {
    "config.json" = modelFile "config.json" "sha256-lT+cDUY0hrEKaHHML9WfIjsscBhPSYFefvvKtdiQi0E=";
    "tokenizer.json" = modelFile "tokenizer.json" "sha256-vlDDYo8r9bteOn8XsfdGEbJWGjon7qsF5aow9BFXIDc=";
    "model.safetensors" = modelFile "model.safetensors" "sha256-U6pRFy0ULInZASzOFa5NbMDKaJWJURQ3nKy0+rEo2ds=";
  };
in
rustPlatform.buildRustPackage {
  pname = "wiki-mcp";
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

  nativeBuildInputs = [
    makeWrapper
    pkg-config
  ];
  # nixpkgs builds onig_sys against the system library
  buildInputs = [ oniguruma ];
  nativeCheckInputs = [ git ];

  # keep the SQLite pinned by Cargo.lock, which is built with FTS5, instead of
  # the system library that the cargo setup hook selects
  preBuild = ''
    unset LIBSQLITE3_SYS_USE_PKG_CONFIG
  '';

  # the embedding test runs only when it finds the model
  preCheck = ''
    export WIKI_MCP_MODEL_DIR=${model}
  '';

  postInstall = ''
    wrapProgram $out/bin/wiki-mcp --prefix PATH : ${
      lib.makeBinPath [
        git
        openssh
      ]
    }
  '';

  passthru = { inherit model; };

  meta = {
    description = "MCP service for a personal Markdown wiki kept in Git";
    license = lib.licenses.mit;
    mainProgram = "wiki-mcp";
    platforms = lib.platforms.unix;
  };
}
