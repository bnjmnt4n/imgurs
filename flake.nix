{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?rev=a115bb9bd56831941be3776c8a94005867f316a7";
    flake-utils.url = "github:numtide/flake-utils?rev=5aed5285a952e0b949eb3ba02c12fa4fcfef535f";
    naersk.url = "github:nmattia/naersk";
    mozillapkgs = {
      url = "github:mozilla/nixpkgs-mozilla";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, naersk, mozillapkgs }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages."${system}";

        mozilla = pkgs.callPackage (mozillapkgs + "/package-set.nix") { };
        rust-channel = mozilla.rustChannelOf {
          # date = "2021-05-20";
          channel = "stable";
          sha256 = "DzNEaW724O8/B8844tt5AVHmSjSQ3cmzlU4BP90oRlY=";
        };
        rust = rust-channel.rust;
        rust-src = rust-channel.rust-src;

        naersk-lib = naersk.lib."${system}".override {
          cargo = rust;
          rustc = rust;
        };

        nativeBuildInputs = with pkgs; [ openssl pkg-config ];
        buildInputs = [ ];
      in
      rec {
        packages.package = naersk-lib.buildPackage {
          pname = "package";
          root = ./.;
          inherit nativeBuildInputs buildInputs;
        };
        defaultPackage = packages.package;

        apps.package = flake-utils.lib.mkApp {
          drv = packages.package;
        };
        defaultApp = apps.package;

        devShell = pkgs.mkShell {
          inherit nativeBuildInputs;
          buildInputs = [
            rust
            pkgs.rust-analyzer
            pkgs.rustfmt
          ] ++ buildInputs;
          RUST_SRC_PATH = "${rust-src}/lib/rustlib/src/rust/library";
          RUST_LOG = "info";
          RUST_BACKTRACE = 1;
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildInputs;
        };
      });
}
