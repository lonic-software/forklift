#!/bin/sh
# forklift installer — macOS, Linux, and Git Bash on Windows.
#
#   curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh
#
# Choose what to install (default is the CLI). Pass a component as an argument:
#   curl -fsSL .../install.sh | sh -s -- server   # the forklift-server head
#   curl -fsSL .../install.sh | sh -s -- all       # both heads
#
# Environment overrides:
#   FORKLIFT_COMPONENT    cli (default) | server | all — same as the positional argument
#   FORKLIFT_VERSION      install a specific tag, e.g. v0.1.0 (default: latest release)
#   FORKLIFT_INSTALL_DIR  where to put the binaries              (default: ~/.local/bin)
#   FORKLIFT_REPO         GitHub repo slug                       (default: lonic-software/forklift)
#   FORKLIFT_BASE_URL     full base URL for the assets (mirrors / air-gapped setups);
#                         overrides FORKLIFT_REPO/FORKLIFT_VERSION entirely
#   FORKLIFT_FORCE        install forklift-server even if one is running (skips the guard)
#   FORKLIFT_NO_ALIAS     skip creating the `fl` short alias next to the forklift binary
set -eu

REPO="${FORKLIFT_REPO:-lonic-software/forklift}"
VERSION="${FORKLIFT_VERSION:-latest}"
INSTALL_DIR="${FORKLIFT_INSTALL_DIR:-$HOME/.local/bin}"
COMPONENT="${1:-${FORKLIFT_COMPONENT:-cli}}"

say() { printf '%s\n' "$*"; }
err() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

# ── which heads to install ──────────────────────────────────────
case "$COMPONENT" in
    cli|forklift)           binaries="forklift" ;;
    server|forklift-server) binaries="forklift-server" ;;
    all|both)               binaries="forklift forklift-server" ;;
    *) err "unknown component '$COMPONENT' (want: cli | server | all)" ;;
esac

# ── detect platform ─────────────────────────────────────────────
os=$(uname -s)
case "$os" in
    Darwin)               suffix="apple-darwin";      ext="tar.gz" ;;
    Linux)                suffix="unknown-linux-musl"; ext="tar.gz" ;;
    MINGW*|MSYS*|CYGWIN*) suffix="pc-windows-msvc";   ext="zip" ;;
    *) err "unsupported OS: $os" ;;
esac

arch=$(uname -m)
case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64)  arch="x86_64" ;;
    *) err "unsupported architecture: $arch" ;;
esac
if [ "$suffix" = "pc-windows-msvc" ] && [ "$arch" = "aarch64" ]; then
    err "no Windows ARM build yet — build from source: cargo install --path crates/forklift"
fi

target="${arch}-${suffix}"
if [ -n "${FORKLIFT_BASE_URL:-}" ]; then
    base="$FORKLIFT_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
else
    base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        err "need curl or wget"
    fi
}

# ── checksums (best effort: skip only if no sha tool is available) ─
have_sha=0
if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
    if fetch "${base}/checksums.txt" "${tmp}/checksums.txt"; then
        have_sha=1
    else
        say "warning: could not fetch checksums.txt; skipping verification"
    fi
fi

verify() { # $1 = asset filename, already downloaded into $tmp
    [ "$have_sha" = 1 ] || return 0
    ( cd "$tmp"
      grep " $1\$" checksums.txt > "$1.sum" || err "$1 missing from checksums.txt"
      if command -v sha256sum >/dev/null 2>&1; then
          sha256sum -c "$1.sum" >/dev/null
      else
          shasum -a 256 -c "$1.sum" >/dev/null
      fi
    ) || err "checksum verification FAILED for $1 — refusing to install"
    say "  checksum ok"
}

# ── download + install one head ─────────────────────────────────
install_one() { # $1 = binary base name (forklift / forklift-server)
    name="$1"
    asset="${name}-${target}.${ext}"
    binfile="$name"
    [ "$ext" = zip ] && binfile="${name}.exe"
    dest="${INSTALL_DIR}/${binfile}"

    # Refuse to update a *running* server: replacing its binary would leave the live
    # process on the old code until it restarts (and there is no server self-update by
    # design). Stop it first, then re-run — see SERVER.md. Best effort (only with pgrep);
    # FORKLIFT_FORCE=1 overrides. The client is not guarded: it is short-lived, and
    # `forklift self-update` re-runs this script from a live `forklift` process.
    if [ "$name" = "forklift-server" ] && [ -z "${FORKLIFT_FORCE:-}" ] \
       && command -v pgrep >/dev/null 2>&1 && pgrep -x forklift-server >/dev/null 2>&1; then
        err "a forklift-server is running — updating now would swap its binary while it runs,
    and the live server would keep the old code until it restarts.
    Stop it first (e.g. 'systemctl stop forklift-server'), re-run this installer, then start it.
    Override with FORKLIFT_FORCE=1."
    fi

    say "downloading ${base}/${asset}"
    fetch "${base}/${asset}" "${tmp}/${asset}" \
        || err "download failed — does a release exist for ${target}?"
    verify "$asset"

    case "$ext" in
        tar.gz) tar -xzf "${tmp}/${asset}" -C "$tmp" ;;
        zip)
            command -v unzip >/dev/null 2>&1 || err "unzip is required"
            unzip -oq "${tmp}/${asset}" -d "$tmp"
            ;;
    esac
    [ -f "${tmp}/${binfile}" ] || err "archive did not contain ${binfile}"

    # Install via a temp file in the target dir + atomic rename, never a truncating write
    # over the destination. Renaming unlinks the old inode (a process still running the old
    # binary keeps it) and drops the new one in atomically — so replacing a live binary
    # (a manual server update, or the client updating itself) is safe on Linux instead of
    # failing with "text file busy".
    mkdir -p "$INSTALL_DIR"
    staged="${INSTALL_DIR}/.${binfile}.new.$$"
    cp "${tmp}/${binfile}" "$staged"
    chmod 755 "$staged"
    mv -f "$staged" "$dest"

    say "installed ${dest} — $("$dest" --version 2>/dev/null || echo "$name")"

    # The `fl` short alias (DESIGN.html §5.0 F): every install method converges on the
    # same `forklift alias install`, so this is the one place the behavior lives. Default
    # on, opt out with FORKLIFT_NO_ALIAS=1; best effort (never fails the install over it —
    # e.g. a read-only install dir, or a name collision it correctly refuses to clobber).
    if [ "$name" = "forklift" ] && [ -z "${FORKLIFT_NO_ALIAS:-}" ]; then
        "$dest" alias install || say "warning: could not create the \"fl\" alias (see above); \"$dest alias install\" retries it"
    fi
}

for b in $binaries; do
    install_one "$b"
done

# ── PATH hint ───────────────────────────────────────────────────
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        say ""
        say "note: ${INSTALL_DIR} is not on your PATH. Add this to your shell profile:"
        say "    export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac
