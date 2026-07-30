{
  description = "cuttlefish: native tooling for agents — llama.cpp-backed task runner driven by Cuttlefish.spec, wasm task prep/return.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.python312
            pkgs.uv          # python env / runner

            # Rust — native runner + wasm host (wasmtime/wasmer bindings) + llama.cpp bindings.
            pkgs.rustc
            pkgs.cargo
            pkgs.maturin     # only needed if/when a PyO3 bridge crate shows up
            pkgs.libiconv    # darwin linking for any cdylib/native extension

            # wasm target for the task-prep/return programs Cuttlefish.spec compiles against.
            pkgs.wasm-pack
          ];

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
