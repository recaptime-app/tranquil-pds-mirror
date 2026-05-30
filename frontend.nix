{
  lib,
  stdenvNoCC,
  nodejs,
  pnpm_11,
  pnpmConfigHook,
  fetchPnpmDeps,
  nix-update-script,
}:
let
  toml = (lib.importTOML ./Cargo.toml).workspace.package;
  pnpm = pnpm_11;
in
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "tranquil-frontend";
  inherit (toml) version;

  src = ./frontend;

  pnpmDeps = fetchPnpmDeps {
    inherit (finalAttrs) pname version src;
    inherit pnpm;
    fetcherVersion = 3;
    hash = "sha256-dOQToZX/i2NV09rDeCKmb/ueYg0vLakxL5JSq8F9KB0=";
  };

  nativeBuildInputs = [
    pnpm
    nodejs
    pnpmConfigHook
  ];

  buildPhase = ''
    runHook preBuild
    pnpm build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    cp -r ./dist $out
    runHook postInstall
  '';

  passthru.updateScript = nix-update-script {
    extraArgs = [
      "--version"
      "SKIP"
    ];
  };
})
