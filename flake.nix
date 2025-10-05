{
  description = "A flake for building rsplug.nvim";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };
        # TODO: Load specific Rust version from ./rust-toolchain.toml
        toolchain = fenix.packages.${system}.minimal.toolchain;
      in
      {
        packages.default =
          (
            (pkgs.makeRustPlatform {
              cargo = toolchain;
              rustc = toolchain;
            }).buildRustPackage
            {
              name = "rsplug";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              nativeBuildInputs = with pkgs; [
                libgit2
                pkg-config
                openssl
              ];
            }
          ).overrideAttrs
            (old: {
              OPENSSL_DIR = "${pkgs.openssl.dev}";
              OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            });
        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/rsplug";
        };
      }
    );
}
