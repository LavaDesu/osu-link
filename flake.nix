{
  description = "nix dev environment";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem(system:
    let
      overlays = [ (import rust-overlay) ];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain;

      rustPlatform = pkgs.makeRustPlatform {
        inherit (toolchain) cargo rustc;
      };
    in {
      devShell = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          toolchain
          gtk3
          pkg-config
          sqlite
        ];

        RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
      };
    });
}
