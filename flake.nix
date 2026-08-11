{
  description = "Rust Lightning Development Environment";

  inputs = {
    # Nixpkgs channel. New channels are released every 6 months.
    # See: https://github.com/NixOS/nixpkgs/tags
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    # This makes it easy for the flake to be multi-platform.
    # See: https://github.com/numtide/flake-utils
    flake-utils.url = "github:numtide/flake-utils";

    # Provides pinned Rust toolchains.
    # See: https://github.com/nix-community/fenix
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Pinned stable toolchain. The version is deliberately bounded:
        #   * clippy must be >= 1.87 so `./ci/check-lint.sh` resolves the lints it
        #     `-A`s (e.g. `clippy::manual_is_multiple_of`); older clippy E0602s on the
        #     unknown lint name.
        #   * but NOT 1.92+, whose clippy grows `assertions_on_constants` (fires on
        #     `assert!(cfg!(fuzzing), ..)` in lightning/src/chain/channelmonitor.rs)
        #     and would break `-D warnings` on the pre-existing tree.
        # 1.90.0 sits in that window and matches what CI's `stable` resolves to.
        rust-toolchain =
          (fenix.packages.${system}.toolchainOf {
            channel = "1.90.0";
            sha256 = "sha256-SJwZ8g0zF2WrKDVmHrVG3pD2RGoQeo24MEXnNx5FyuI=";
          }).withComponents
            [
              "rustc"
              "cargo"
              "clippy"
              "rustfmt"
              "rust-src" # Needed for the rust-analyzer extension to work.
            ];
      in
      {
        # The default development shell. Use `nix develop` or direnv to enter it.
        devShells.default = pkgs.mkShell {
          name = "rust-lightning-dev-shell";

          packages = [
            rust-toolchain
          ]
          ++ (with pkgs; [
            just # Command runner
            nodejs # JavaScript runtime, required for MCP tools
            mold # Fast linker for Rust/C/C++
            pnpm # Package manager for JavaScript, required for MCP tools
            stdenv.cc.cc.lib # C++ standard library for runtime
          ]);

          env = {
            # OpenSSL configuration for Nix
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
            # C++ standard library path for runtime
            LD_LIBRARY_PATH = "${pkgs.stdenv.cc.cc.lib}/lib";
          };

          shellHook = ''
            echo "Rust Lightning dev shell"
            rustc --version
            cargo --version
          '';
        };
      }
    );
}
