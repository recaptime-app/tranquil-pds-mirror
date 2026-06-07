nixpkgs: {
  tranquil-pds = nixpkgs.callPackage ./default.nix { };
  tranquil-pds-aarch64 = nixpkgs.pkgsCross.aarch64-multiplatform.callPackage ./default.nix { };
  tranquil-frontend = nixpkgs.callPackage ./frontend.nix { };
}
