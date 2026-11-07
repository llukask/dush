{
  description = "dush dev shell — Rust toolchain + gpui system libs for the GUI binary";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        # Libraries the gpui stack needs. `libxkbcommon` and `libxcb`
        # are linked at build time; the rest are dlopened at runtime
        # (Wayland and Vulkan by the blade renderer, fontconfig by
        # font-kit), so they also have to end up on the binary's rpath
        # or on LD_LIBRARY_PATH in the dev shell. Shared between the GUI
        # package and the dev shell so the two don't drift.
        gpuiLibs = with pkgs; [
          libxkbcommon
          libxcb
          libx11
          wayland
          vulkan-loader
          fontconfig
          freetype
        ];

        # CLI build: pure Rust, no system libs required. We disable
        # default features explicitly to be defensive in case `gpui`
        # ever becomes default.
        dush = pkgs.rustPlatform.buildRustPackage {
          pname = "dush";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildNoDefaultFeatures = true;
          # Only build the CLI bin — the GUI bin has `required-features
          # = ["gpui"]` and would be skipped anyway, but being explicit
          # keeps the build narrow and the closure small.
          cargoBuildFlags = [ "--bin" "dush" ];
          # The CLI has no integration tests that need the GUI stack;
          # run the default test set.
          doCheck = true;
          meta = {
            description = cargoToml.package.description;
            license = pkgs.lib.licenses.mit;
            mainProgram = "dush";
          };
        };

        # GUI builds. Both GUIs need the same system libraries, so one
        # builder serves both. No wrapper hook is needed, but the
        # dlopened libraries in `gpuiLibs` are invisible to the linker,
        # so the fixup phase's rpath shrinking would drop them. We add
        # them back afterwards in `postFixup`.
        mkGui = { pname, feature, flavor }: pkgs.rustPlatform.buildRustPackage {
          inherit pname;
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildFeatures = [ feature ];
          cargoBuildFlags = [ "--bin" pname ];
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = gpuiLibs;
          # `cargo test` also runs the library tests; keep it on so we
          # don't silently regress them.
          doCheck = true;
          postFixup = ''
            patchelf --add-rpath ${pkgs.lib.makeLibraryPath gpuiLibs} $out/bin/${pname}
          '';
          meta = {
            description = cargoToml.package.description + " (${flavor} GUI)";
            license = pkgs.lib.licenses.mit;
            mainProgram = pname;
          };
        };

        dush-gpui = mkGui { pname = "dush-gpui"; feature = "gpui"; flavor = "gpui"; };
        dush-kit = mkGui { pname = "dush-kit"; feature = "gpui-kit"; flavor = "GPUI Kit"; };
      in {
        packages = {
          inherit dush dush-gpui dush-kit;
          default = dush;
        };

        # `nix run` resolves to `apps.default`; declaring it explicitly
        # (rather than relying on the package fallback) makes the entry
        # point unambiguous if the package ever gains extra binaries.
        apps = {
          default = {
            type = "app";
            program = "${dush}/bin/dush";
          };
          dush = {
            type = "app";
            program = "${dush}/bin/dush";
          };
          dush-gpui = {
            type = "app";
            program = "${dush-gpui}/bin/dush-gpui";
          };
          dush-kit = {
            type = "app";
            program = "${dush-kit}/bin/dush-kit";
          };
        };

        devShells.default = pkgs.mkShell {
          # Build-time helpers used by `cargo build`: a Rust toolchain
          # plus `pkg-config`, which the xkbcommon and xcb bindings use to
          # find the libraries below.
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            pkg-config
          ];

          buildInputs = gpuiLibs;

          # `cargo run --bin dush-gpui` has no rpath for the dlopened
          # gpui libraries, so expose them the old-fashioned way.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiLibs;
        };
      });
}
