{
  description = "Development environment for Konnect";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    ...
  }: let
    supportedSystems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    pkgsFor = system:
      import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
  in {
    packages = forAllSystems (
      system: let
        pkgs = pkgsFor system;
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
      in rec {
        konnect = rustPlatform.buildRustPackage {
          pname = "konnect";
          inherit version;

          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              pkgs.lib.cleanSourceFilter path type
              && !(type == "directory" && builtins.baseNameOf path == "target");
          };

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            cmake
            protobuf
            pkg-config
          ];

          PROTOC = "${pkgs.protobuf}/bin/protoc";
          PROTOC_INCLUDE = "${pkgs.protobuf}/include";

          cargoBuildFlags = [
            "-p"
            "konnect"
            "--bin"
            "konnect"
          ];

          cargoTestFlags = [
            "-p"
            "konnect"
            "--lib"
            "--tests"
          ];

          # Protocol tests spawn the packaged server. Give that child an
          # explicit writable state directory instead of Nix's read-only
          # sandbox home.
          preCheck = ''
            export KONNECT_STATE_DIR="$TMPDIR/konnect-state"
            mkdir -p "$KONNECT_STATE_DIR"
          '';

          meta.mainProgram = "konnect";
        };

        default = konnect;
      }
    );

    apps = forAllSystems (system: {
      konnect = {
        type = "app";
        program = "${self.packages.${system}.konnect}/bin/konnect";
      };
      default = self.apps.${system}.konnect;
    });

    devShells = forAllSystems (
      system: let
        pkgs = pkgsFor system;
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            protobuf
            cmake
            pkg-config
          ];

          # konnect-ipc/build.rs can discover these from PATH, but setting
          # them explicitly also makes protobuf's well-known types reliable
          # with Nix store paths.
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          PROTOC_INCLUDE = "${pkgs.protobuf}/include";
        };
      }
    );
  };
}
