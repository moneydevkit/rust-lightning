{
  description = "Rust Lightning Development Environment";

  inputs = {
    # Nixpkgs channel. New channels are released every 6 months.
    # See: https://github.com/NixOS/nixpkgs/tags
    nixpkgs.url = "github:nixos/nixpkgs/25.11";

    # This makes it easy for the flake to be multi-platform.
    # See: https://github.com/numtide/flake-utils
    flake-utils.url = "github:numtide/flake-utils";

    # Provides Rust toolchains.
    # See: https://github.com/oxalica/rust-overlay
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # Overlays provide additional packages not available in the channels.
        overlays = [
          # Provides the rust-bin package; a set of pre-built Rust toolchains.
          (import rust-overlay)
        ];

        # The final set of packages.
        pkgs = import nixpkgs {
          # Inheriting from system is what makes this multi-platform.
          # We also inherit the overlays that we want to use.
          inherit system overlays;
        };

        # The specific Rust toolchain that we use in development shells.
        rust-toolchain = pkgs.rust-bin.stable."1.85.1".default.override {
          extensions = [ "rust-src" ]; # Needed for the rust-analyzer extension to work.
        };

        # Common packages that are required to build and test the software.
        commonPackages = with pkgs; [
          rust-toolchain # Rust toolchain
          nodejs # JavaScript runtime, required for MCP tools
          mold # Fast linker for Rust/C/C++
          pnpm # Package manager for JavaScript, required for MCP tools
          stdenv.cc.cc.lib # C++ standard library for runtime
        ];

        # Common environment variables that are required.
        commonEnv = {
          # OpenSSL configuration for Nix
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          # C++ standard library path for runtime
          LD_LIBRARY_PATH = "${pkgs.stdenv.cc.cc.lib}/lib";
        };

        # The script to be executed upon entering the development shell.
        # Useful for configuration and validation of the environment.
        commonShellHook = ''
          echo "=========================================="
          echo "  Rust Lightning Development Shell"
          echo "=========================================="

          # Verify Rust version
          rustc --version
          cargo --version

          echo "=========================================="
          echo "Terminal ready. All systems operational."
          echo "=========================================="
        '';

        # Helper function to create a development shell that is pre-configured
        # with common packages, environment variables, and shell hook.
        mkDevShell =
          {
            name ? "",
            packages ? [ ],
            env ? { },
            shellHook ? "",
          }:
          pkgs.mkShell {
            inherit name;
            packages = commonPackages ++ packages;
            shellHook = commonShellHook + shellHook;
            env = commonEnv // env;
          };
      in
      {
        # The development shells.
        devShells = {
          # The default development shell for humans.
          # Use `nix develop` or direnv to enter the shell.
          default = mkDevShell {
            name = "rust-lightning-dev-shell";
          };
        };
      }
    );
}

