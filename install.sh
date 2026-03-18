#!/bin/sh
set -eu

REPO="querymt/querymt"
INSTALL_DIR="${QMT_INSTALL_DIR:-$HOME/.local/bin}"
CHANNEL="latest"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --nightly)
            CHANNEL="nightly"
            ;;
        --latest)
            CHANNEL="latest"
            ;;
        --help|-h)
            cat <<'EOF'
Usage: install.sh [--nightly|--latest]

Installs qmt and qmtcode into ~/.local/bin (or $QMT_INSTALL_DIR).
Set QMT_CHANNEL=nightly as an alternative to --nightly.
EOF
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
    shift
done

if [ "${QMT_CHANNEL:-}" = "nightly" ]; then
    CHANNEL="nightly"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
            *) echo "Unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
        esac
        EXT="tar.gz"
        ;;
    Darwin)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-apple-darwin" ;;
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            *) echo "Unsupported macOS architecture: $ARCH" >&2; exit 1 ;;
        esac
        EXT="tar.gz"
        ;;
    FreeBSD)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-freebsd" ;;
            aarch64|arm64) echo "aarch64 FreeBSD is not currently supported in release artifacts" >&2; exit 1 ;;
            *) echo "Unsupported FreeBSD architecture: $ARCH" >&2; exit 1 ;;
        esac
        EXT="tar.gz"
        ;;
    *)
        echo "Unsupported OS: $OS" >&2
        exit 1
        ;;
esac

if command -v curl >/dev/null 2>&1; then
    fetch_text() {
        curl -fsSL "$1"
    }
    fetch_file() {
        curl -fsSL "$1" -o "$2"
    }
elif command -v wget >/dev/null 2>&1; then
    fetch_text() {
        wget -qO- "$1"
    }
    fetch_file() {
        wget -qO "$2" "$1"
    }
elif command -v fetch >/dev/null 2>&1; then
    fetch_text() {
        fetch -qo - "$1"
    }
    fetch_file() {
        fetch -qo "$2" "$1"
    }
else
    echo "Need one of: curl, wget, fetch" >&2
    exit 1
fi

release_api_url() {
    if [ "$CHANNEL" = "nightly" ]; then
        echo "https://api.github.com/repos/$REPO/releases/tags/nightly"
    else
        echo "https://api.github.com/repos/$REPO/releases/latest"
    fi
}

asset_url_for() {
    binary="$1"

    json="$(fetch_text "$(release_api_url)")"
    urls="$(printf '%s' "$json" | grep -o '"browser_download_url":[[:space:]]*"[^"]*"' | sed 's/^"browser_download_url":[[:space:]]*"//; s/"$//')"

    if [ "$CHANNEL" = "nightly" ]; then
        pattern="/${binary}-nightly-[^/]*-${TARGET}\.${EXT}$"
    else
        pattern="/${binary}-[^/]*-${TARGET}\.${EXT}$"
    fi

    url="$(printf '%s\n' "$urls" | grep -E "$pattern" | head -n 1 || true)"
    if [ -z "$url" ]; then
        echo "Could not find asset for ${binary} (${TARGET}, ${CHANNEL})" >&2
        exit 1
    fi

    printf '%s\n' "$url"
}

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t qmt-install)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

mkdir -p "$INSTALL_DIR"

install_binary() {
    binary="$1"
    archive="$TMP_DIR/${binary}.${EXT}"
    url="$(asset_url_for "$binary")"

    echo "Downloading ${binary} (${CHANNEL}, ${TARGET})..."
    fetch_file "$url" "$archive"

    extract_dir="$TMP_DIR/extract-${binary}"
    mkdir -p "$extract_dir"
    tar -xzf "$archive" -C "$extract_dir"

    src="$(find "$extract_dir" -type f -name "$binary" | head -n 1 || true)"
    if [ -z "$src" ]; then
        echo "Failed to locate ${binary} in extracted archive" >&2
        exit 1
    fi

    install -m 0755 "$src" "$INSTALL_DIR/$binary"
}

install_binary "qmt"
install_binary "qmtcode"

echo "Installed to: $INSTALL_DIR"
if command -v qmt >/dev/null 2>&1; then
    qmt --version || true
else
    "$INSTALL_DIR/qmt" --version || true
fi
if command -v qmtcode >/dev/null 2>&1; then
    qmtcode --version || true
else
    "$INSTALL_DIR/qmtcode" --version || true
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "Add to PATH if needed: export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
