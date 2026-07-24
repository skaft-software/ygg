#!/bin/sh
set -eu

repository="https://github.com/skaft-software/ygg"
version="v0.1.1-alpha"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
bin_dir="$cargo_home/bin"

if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' \
        "Ygg requires Rust 1.86 or newer." \
        "Install Rust from https://rustup.rs/ and run this installer again." >&2
    exit 1
fi

printf 'Installing Ygg %s from %s\n' "$version" "$repository"
cargo install \
    --locked \
    --git "$repository" \
    --tag "$version" \
    --bin ygg \
    ygg-coding-agent

path_present=false
case ":${PATH:-}:" in
    *":$bin_dir:"*) path_present=true ;;
esac

profile=""
path_line='export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"'
case "${SHELL:-}" in
    */zsh) profile="$HOME/.zshrc" ;;
    */bash)
        if [ -f "$HOME/.bash_profile" ]; then
            profile="$HOME/.bash_profile"
        else
            profile="$HOME/.bashrc"
        fi
        ;;
    */sh|*/dash|*/ksh) profile="$HOME/.profile" ;;
esac

marker="# Added by the Ygg installer"
if [ "$path_present" = false ] && [ -n "$profile" ]; then
    if ! grep -F "$marker" "$profile" >/dev/null 2>&1; then
        printf '\n%s\n%s\n' "$marker" "$path_line" >> "$profile"
        printf 'Added %s to PATH in %s\n' "$bin_dir" "$profile"
    fi
fi

"$bin_dir/ygg" --version

if ! command -v rg >/dev/null 2>&1; then
    printf '%s\n' \
        "Note: Ygg also requires ripgrep (rg)." \
        "Install it with 'brew install ripgrep' on macOS or your Linux package manager."
fi

if [ "$path_present" = false ]; then
    printf 'Restart your shell, or run:\n  export PATH="%s:$PATH"\n' "$bin_dir"
fi

printf '%s\n' "Ygg is installed. Run 'ygg --help' to get started."
