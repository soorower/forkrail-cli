#!/bin/sh
# Installs the forkrail CLI: the newest release's program for this computer, checked against its
# SHA-256, into ~/.local/bin (or FORKRAIL_BIN). Then `forkrail install` sets forkrail up in your
# own AWS account.
#
#   curl -fsSL https://forkrail.com/install | sh
#
# forkrail.com/install redirects to this script in the newest release:
# https://github.com/soorower/forkrail-cli/releases/latest/download/install.sh
#
# Plain POSIX sh, ASCII only, every variable in braces where text follows it: macOS's /bin/sh is
# bash 3.2, which in a UTF-8 terminal reads a non-ASCII letter (an ellipsis, say) right after $name as part of the name.
#
# Everything is in main, called on the last line: sh runs a piped script as it arrives, so a
# download cut short must run nothing at all rather than the first half of a command.
set -eu

main() {
  REPO="${FORKRAIL_REPO:-soorower/forkrail-cli}"
  BIN="${FORKRAIL_BIN:-$HOME/.local/bin}"

  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os-$arch" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    MINGW* | MSYS* | CYGWIN*)
      echo "On Windows, install forkrail from PowerShell: irm https://forkrail.com/install.ps1 | iex" >&2
      exit 1
      ;;
    *)
      echo "forkrail has no build for ${os} ${arch} yet." >&2
      exit 1
      ;;
  esac

  url="https://github.com/$REPO/releases/latest/download/forkrail-$target.tar.gz"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "Downloading forkrail for ${os} ${arch}..."
  curl -fsSL "$url" -o "$tmp/forkrail.tar.gz"
  curl -fsSL "$url.sha256" -o "$tmp/sum"
  want="$(cut -d' ' -f1 <"$tmp/sum")"
  if command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$tmp/forkrail.tar.gz" | cut -d' ' -f1)"
  else
    got="$(sha256sum "$tmp/forkrail.tar.gz" | cut -d' ' -f1)"
  fi
  if [ "$want" != "$got" ]; then
    echo "The download doesn't match its SHA-256. Nothing was installed; try again." >&2
    exit 1
  fi
  tar -xzf "$tmp/forkrail.tar.gz" -C "$tmp"
  mkdir -p "$BIN"
  cp "$tmp/forkrail" "$BIN/forkrail"
  chmod 755 "$BIN/forkrail"
  echo "Installed $("$BIN/forkrail" --version 2>/dev/null || echo forkrail) in $BIN."
  case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "Add it to your PATH first: export PATH=\"$BIN:\$PATH\"" ;;
  esac
  echo "Next, run: forkrail install"
}

main "$@"
