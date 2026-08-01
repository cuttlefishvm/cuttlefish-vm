{
  description = "cuttlefish: native tooling for agents — llama.cpp-backed task runner driven by Cuttlefish.spec, wasm task prep/return.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Guest proc-blocks compile to wasm32-unknown-unknown; the host is
        # native. Pinned as one toolchain so both targets come from the same
        # rustc — nixpkgs' plain `rustc` ships no wasm std, which is why this
        # goes through rust-overlay rather than pkgs.rustc.
        #
        # On wasm64: guests are capped at 4 GiB of linear memory here, which is
        # deliberate rather than accidental. Bulk data never enters guest memory
        # (blocks pull bounded windows through Open/Slice handles), so the cap
        # binds on nothing we actually do. If a future block genuinely needs a
        # >4 GiB resident working set, switch THAT block to
        # wasm64-unknown-unknown rather than the whole project: one wasmtime
        # engine with wasm_memory64(true) runs 32- and 64-bit modules side by
        # side, and cuttlefish-host detects width via MemoryType::is_64.
        # The cost is real, so weigh it: wasm64 is Tier 3 with no prebuilt std,
        # so it needs
        #   pkgs.rust-bin.nightly.latest.default.override {
        #     targets = [ "wasm64-unknown-unknown" ];
        #     extensions = [ "rust-src" ];   # required by -Z build-std
        #   }
        # plus `-Z build-std=std,panic_abort` on the guest build, and 64-bit
        # memories lose wasmtime's guard-page bounds-check elision.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [ "rust-src" "rust-analyzer" "llvm-tools-preview" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.python312
            pkgs.uv          # python env / runner

            # Rust — host runtime, wasm guest blocks, and the CLI.
            rustToolchain
            pkgs.maturin     # only needed if/when a PyO3 bridge crate shows up
            pkgs.libiconv    # darwin linking for any cdylib/native extension

            # Coverage, matching what CI reports. Needs the llvm-tools-preview
            # component above.
            pkgs.cargo-llvm-cov

            # Only needed for the optional `llamacpp` feature, which compiles
            # llama.cpp from source: cmake drives that build, and bindgen needs
            # libclang to generate the FFI. Present unconditionally so that
            # `--features llamacpp` works in this shell without extra setup;
            # they cost nothing when the feature is off.
            pkgs.cmake
            pkgs.libclang
          ];

          # bindgen locates libclang through this; without it the `llamacpp`
          # feature fails to build with a message that does not mention clang.
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

          shellHook = ''
            echo "Python $(python3 --version)"
            echo "uv $(uv --version)"
            echo "rustc $(rustc --version)"
            echo "cargo $(cargo --version)"

            # .venv/bin on PATH unconditionally so `nix develop --command ...` (bash -c, CI,
            # scripts) and non-interactive shells get project scripts too, not just a fully
            # interactive zsh session.
            _repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
            [ -d "$_repo_root/.venv/bin" ] && export PATH="$_repo_root/.venv/bin:$PATH"

            # Only launch interactive zsh when no --command was given.
            if [ -z "$*" ] && [ -t 0 ]; then
              # ZDOTDIR trick: wrap user's .zshrc and re-prepend nix bins AFTER
              # homebrew shellenv runs (which would otherwise shadow nix packages).
              _zd=$(mktemp -d)
              printf '[ -f "$HOME/.zshrc" ] && ZDOTDIR="" source "$HOME/.zshrc"\n' > "$_zd/.zshrc"
              printf 'export PROMPT="%%F{cyan}[nix]%%f $PROMPT"\n' >> "$_zd/.zshrc"
              printf 'alias vim=nvim\n' >> "$_zd/.zshrc"
              printf 'autoload -Uz compinit && compinit\n' >> "$_zd/.zshrc"
              printf 'echo "\xe2\x9c\x93 nix nvim: $(nvim --version | head -1)"\n' >> "$_zd/.zshrc"

              ZDOTDIR="$_zd" exec zsh
            fi
          '';
        };
      }
    );
}
