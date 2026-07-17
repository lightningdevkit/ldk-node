{
  description = "ldk-node development environment";

  inputs = {
    # Pinned to nixos-25.05: newer revisions of nixpkgs ship a
    # blockstream-electrs whose CLI moved from `--cookie-file` to `--cookie`,
    # This is a break in the `electrs` crate.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          # `blockstream-electrs` in this nixpkgs revision transitively pulls
          # in `python3.12-ecdsa-0.19.1`, which nixpkgs marks insecure.
          config.permittedInsecurePackages = [ "python3.12-ecdsa-0.19.1" ];
        };

        # Upstream tests are failing. Skips the tests on install.
        blockstream-electrs = pkgs.blockstream-electrs.overrideAttrs (_: {
          doCheck = false;
        });
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.bitcoind
            blockstream-electrs
          ];

          shellHook = ''
            export BITCOIND_EXE="${pkgs.bitcoind}/bin/bitcoind"
            export ELECTRS_EXE="${blockstream-electrs}/bin/electrs"
          '';
        };
      });
}
