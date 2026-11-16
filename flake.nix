{
  description = "cv";

  inputs = {
    # 26.05 is the last nixpkgs release that supports x86_64-darwin (until the end of 2026).
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-26.05";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
    }:
    let
      inherit (nixpkgs) lib;
      defaultSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems =
        fn:
        lib.genAttrs defaultSystems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
              config.allowDeprecatedx86_64Darwin = true;
            };
            rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);
          in
          fn { inherit pkgs rustToolchain craneLib; }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, craneLib, ... }:
        {
          default = craneLib.buildPackage {
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                (craneLib.fileset.commonCargoSources ./.)
                ./migrations
              ];
            };
            strictDeps = true;
            # Git serves the checkout tests' fixture repository.
            nativeCheckInputs = [
              pkgs.git
              pkgs.postgresql_18
            ];
            # A Postgres of the build's own, reachable only through a socket, as `#[sqlx::test]`
            # expects.
            preCheck = ''
              export PGDATA="$TMPDIR/postgres" PGHOST="$TMPDIR/postgres"
              export DATABASE_URL="postgres:///postgres?host=$PGHOST&user=postgres"
              initdb --username=postgres --auth=trust --no-locale --encoding=UTF8 >/dev/null
              pg_ctl start --silent --log="$PGDATA/log" -o "-c listen_addresses= -k $PGHOST"
            '';
            meta.mainProgram = "cv";
          };
        }
      );

      devShells = forAllSystems (
        { pkgs, rustToolchain, ... }:
        let
          inherit (rustToolchain.availableComponents) rust-analyzer rust-src;
        in
        rec {
          # What builds need, and nothing more.
          ci = pkgs.mkShell {
            name = "cv-ci";
            packages = [
              rustToolchain
              pkgs.git
              pkgs.postgresql_18
            ];
          };

          # Adds editor support, from the same Rust release.
          default = pkgs.mkShell {
            name = "cv-dev";
            inputsFrom = [ ci ];
            packages = [ rust-analyzer ];
            RUST_SRC_PATH = "${rust-src}/lib/rustlib/src/rust/library";
          };
        }
      );

      nixosModules.default = import ./nix/module.nix self;

      checks = forAllSystems (
        { pkgs, ... }:
        let
          cv = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          flags = "grep -o -- '--[a-z-]*' | grep -vx -- '--help\\|--version' | sort -u";
        in
        {
          # Builds `cv`, running its tests, then fails unless `nix/module.nix` passes exactly the
          # flags `cv` takes.
          module-in-sync = pkgs.runCommand "module-in-sync" { } ''
            diff -u <(${lib.getExe cv} --help | ${flags}) <(< ${./nix/module.nix} ${flags})
            touch $out
          '';
        }
      );
    };
}
