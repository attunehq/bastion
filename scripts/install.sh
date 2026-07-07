#!/usr/bin/env bash
set -euo pipefail

# bastion installer script
#
# Usage:
#   curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash
#   curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- -b /usr/local/bin
#   curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- -v 0.1.0
#
# Options:
#   -v, --version    Specify a version (default: latest)
#   -b, --bin-dir    Specify the installation directory (default: $HOME/.local/bin)
#   -t, --tmp-dir    Specify the temporary directory (default: system temp directory)
#   -l, --libc       Force the Linux C runtime: 'gnu' or 'musl' (default: autodetect)
#   -h, --help       Show help message
#
# The libc choice can also be forced with the BASTION_LIBC environment variable,
# which is handy when piping into bash (no `-s --` needed):
#   curl -sSfL .../install.sh | BASTION_LIBC=musl bash
#
# The musl build is statically linked and has no glibc version dependency, so
# 'musl' runs on any Linux regardless of how old the host glibc is.
#
# Supported hosts: Linux, macOS, and Windows under a bash environment (Git
# Bash, or `shell: bash` on a CI runner), where it installs bastion.exe. For
# plain PowerShell on Windows, use scripts/install.ps1 instead.

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# GitHub repository configuration
REPO="jssblck/bastion"
GITHUB_BASE="https://github.com/${REPO}"
GITHUB_DOWNLOAD="${GITHUB_BASE}/releases/download"

# Minimum glibc the prebuilt gnu binary supports. This must match the Ubuntu
# release the gnu targets are built on in .github/workflows/release.yml
# (currently ubuntu-22.04, glibc 2.35): a binary links against its build host's
# glibc, so that runner sets the floor. On a host with an older or undetectable
# glibc, the installer falls back to the statically linked musl build.
GLIBC_MIN_MAJOR=2
GLIBC_MIN_MINOR=35

# Fail with an error message
fail() {
  echo -e "${RED}Error: $1${NC}" >&2
  exit 1
}

# Print an informational message
info() {
  echo -e "${GREEN}$1${NC}" >&2
}

# Print a warning message
warn() {
  echo -e "${YELLOW}Warning: $1${NC}" >&2
}

# Check for required commands
check_requirements() {
  local missing=()

  # Check for curl
  if ! command -v curl > /dev/null; then
    missing+=("curl")
  fi

  # Check for tar
  if ! command -v tar > /dev/null; then
    missing+=("tar")
  fi

  # Check for checksum utility
  if ! command -v sha256sum > /dev/null && ! command -v shasum > /dev/null; then
    missing+=("sha256sum or shasum")
  fi

  if [[ ${#missing[@]} -gt 0 ]]; then
    fail "Missing required commands: ${missing[*]}

Please install the missing commands and try again:

  Debian/Ubuntu:  apt-get update && apt-get install -y curl tar coreutils
  Alpine:         apk add --no-cache curl tar coreutils
  RHEL/CentOS:    yum install -y curl tar coreutils
  macOS:          (should be pre-installed)"
  fi
}

# Autodetect which Linux C-runtime build to install. Echoes "musl" or "gnu".
#
# The choice is driven by whether the host can actually run the gnu binary, not
# by a single distro check: pick the static musl build for musl systems (Alpine
# and friends) and for any glibc host older than the gnu build's floor (or where
# the glibc version cannot be determined), and pick gnu only when the host glibc
# is confirmed new enough. A musl binary is statically linked, so it runs on a
# glibc host too; the reverse is not true, which is why musl is the safe default
# whenever support is in doubt.
detect_linux_libc() {
  # Definitive musl systems: install the musl build.
  if [[ -e /etc/alpine-release ]]; then
    echo "musl"
    return
  fi

  # `ldd --version` is the most portable libc probe. glibc prints its version;
  # musl prints "musl libc ..." (on stderr, with a non-zero exit). Fold both
  # streams together and read it once.
  local ldd_out
  ldd_out=$(ldd --version 2>&1 || true)
  if grep -qi musl <<< "$ldd_out"; then
    echo "musl"
    return
  fi

  # A glibc host: determine its version and compare against the floor. getconf is
  # the cleanest source ("glibc 2.35"); fall back to the version ldd reports
  # ("ldd (Ubuntu GLIBC 2.35-...) 2.35").
  local version=""
  if command -v getconf > /dev/null 2>&1; then
    version=$(getconf GNU_LIBC_VERSION 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -n1 || true)
  fi
  if [[ -z "$version" ]]; then
    version=$(grep -oE '[0-9]+\.[0-9]+' <<< "$ldd_out" | head -n1 || true)
  fi

  if [[ "$version" =~ ^([0-9]+)\.([0-9]+)$ ]]; then
    local major="${BASH_REMATCH[1]}"
    local minor="${BASH_REMATCH[2]}"
    if (( major > GLIBC_MIN_MAJOR || (major == GLIBC_MIN_MAJOR && minor >= GLIBC_MIN_MINOR) )); then
      echo "gnu"
      return
    fi
    info "Host glibc ${version} is older than ${GLIBC_MIN_MAJOR}.${GLIBC_MIN_MINOR}; using the static musl build"
    echo "musl"
    return
  fi

  # Could not determine the glibc version: fall back to the portable musl build.
  warn "Could not determine the host glibc version; using the static musl build"
  echo "musl"
}

# Detect the operating system and architecture
detect_platform() {
  local kernel
  local machine
  local os
  local arch

  kernel=$(uname -s)
  machine=$(uname -m)

  case "$kernel" in
    Linux)
      os="unknown-linux"
      ;;
    Darwin)
      os="apple-darwin"
      ;;
    MINGW* | MSYS* | CYGWIN*)
      # Git Bash / MSYS on Windows: install the Windows build. Plain PowerShell
      # users should prefer scripts/install.ps1; this path exists for bash-based
      # environments (Git Bash terminals, `shell: bash` on Windows CI runners).
      os="pc-windows-gnu"
      ;;
    *)
      fail "Unsupported operating system: $kernel"
      ;;
  esac

  case "$machine" in
    x86_64 | amd64)
      arch="x86_64"
      ;;
    arm64 | aarch64)
      arch="aarch64"
      ;;
    *)
      fail "Unsupported architecture: $machine"
      ;;
  esac

  # Only an x86_64 Windows build is published.
  if [[ "$os" == "pc-windows-gnu" && "$arch" != "x86_64" ]]; then
    fail "Unsupported architecture on Windows: $machine (only x86_64 builds are published)"
  fi

  # Select the C runtime variant on Linux. By default this is autodetected (see
  # detect_linux_libc); `--libc` or $BASTION_LIBC can force it. Forcing 'musl'
  # yields the statically linked build, which has no glibc version dependency and
  # so runs on any Linux regardless of how old the host glibc is.
  if [[ "$os" == "unknown-linux" ]]; then
    case "$LIBC" in
      musl | gnu)
        os="$os-$LIBC"
        ;;
      "")
        os="$os-$(detect_linux_libc)"
        ;;
      *)
        fail "Invalid libc '$LIBC': expected 'gnu' or 'musl'"
        ;;
    esac
  elif [[ -n "$LIBC" ]]; then
    warn "Ignoring libc override '$LIBC': it only applies to Linux"
  fi

  echo "${arch}-${os}"
}

# Parse command line arguments
parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -v|--version)
        VERSION="$2"
        shift 2
        ;;
      -b|--bin-dir)
        BIN_DIR="$2"
        shift 2
        ;;
      -t|--tmp-dir)
        TMP_DIR="$2"
        shift 2
        ;;
      -l|--libc)
        LIBC="$2"
        shift 2
        ;;
      -h|--help)
        echo "bastion installer"
        echo
        echo "Usage: curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash [args]"
        echo
        echo "Options:"
        echo "  -v, --version    Specify a version (default: latest)"
        echo "  -b, --bin-dir    Specify the installation directory (default: \$HOME/.local/bin)"
        echo "  -t, --tmp-dir    Specify the temporary directory (default: system temp directory)"
        echo "  -l, --libc       Force the Linux C runtime: 'gnu' or 'musl' (default: autodetect)"
        echo "  -h, --help       Show this help message"
        echo
        echo "The libc choice can also be forced with the BASTION_LIBC environment variable."
        echo "The musl build is statically linked with no glibc version dependency, so 'musl'"
        echo "runs on any Linux regardless of how old the host glibc is."
        echo
        echo "Examples:"
        echo "  # Install latest version"
        echo "  curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash"
        echo
        echo "  # Install specific version"
        echo "  curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- -v 0.1.0"
        echo
        echo "  # Install to custom directory"
        echo "  curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- -b /usr/local/bin"
        echo
        echo "  # Force the static musl build (works on old-glibc systems)"
        echo "  curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | bash -s -- --libc musl"
        echo "  # ...or via the environment, with no '-s --' needed:"
        echo "  curl -sSfL https://raw.githubusercontent.com/jssblck/bastion/main/scripts/install.sh | BASTION_LIBC=musl bash"
        exit 0
        ;;
      *)
        fail "Unknown option: $1"
        ;;
    esac
  done
}

# Get the latest version number from the releases redirect
get_latest_version() {
  # Resolve the tag from the redirect target of /releases/latest rather than
  # the JSON API. api.github.com is tightly rate limited for unauthenticated
  # callers (60 requests/hour/IP), so it 403s from shared NATs and CI runners;
  # the github.com redirect has no such limit.
  local latest_url="${GITHUB_BASE}/releases/latest"
  local effective_url
  local version

  if ! effective_url=$(curl -sSfL -o /dev/null -w '%{url_effective}' "$latest_url" 2>&1); then
    fail "Failed to resolve latest release from $latest_url. Error: $effective_url"
  fi

  # The redirect lands on .../releases/tag/vX.Y.Z; take the segment after /tag/.
  version="${effective_url##*/tag/}"
  version="${version#v}"

  if [[ -z "$version" || "$version" == "$effective_url" ]]; then
    fail "Could not parse version from latest release URL: $effective_url"
  fi

  echo "$version"
}

# Download a file
download() {
  local url="$1"
  local dest="$2"

  info "Downloading from $url"

  if ! curl -sSfL "$url" -o "$dest"; then
    fail "Failed to download from $url"
  fi
}

# Install the binary
install_binary() {
  local platform="$1"
  local version="$2"
  local bin_dir="$3"
  local tmp_dir="$4"
  local archive_name="bastion-${platform}.tar.gz"
  # The Windows archive ships bastion.exe; every other platform ships bastion.
  local binary_name="bastion"
  case "$platform" in
    *pc-windows*) binary_name="bastion.exe" ;;
  esac

  version="${version#v}"
  local tag="v${version}"
  local download_url="${GITHUB_DOWNLOAD}/${tag}/${archive_name}"
  local checksums_url="${GITHUB_DOWNLOAD}/${tag}/checksums.txt"

  # Create temporary directory
  local workdir="$tmp_dir/bastion-install-$$"
  mkdir -p "$workdir"
  cd "$workdir"

  # Download archive and checksums
  download "$download_url" "$archive_name"
  download "$checksums_url" "checksums.txt"

  # Verify checksum
  info "Verifying checksum"
  local expected_checksum
  expected_checksum=$(grep "$archive_name" checksums.txt | awk '{print $1}')
  if [[ -z "$expected_checksum" ]]; then
    fail "Couldn't find checksum for $archive_name"
  fi

  local actual_checksum
  if command -v sha256sum > /dev/null; then
    actual_checksum=$(sha256sum "$archive_name" | awk '{print $1}')
  elif command -v shasum > /dev/null; then
    actual_checksum=$(shasum -a 256 "$archive_name" | awk '{print $1}')
  else
    fail "Neither sha256sum nor shasum found, cannot verify download"
  fi

  if [[ "$expected_checksum" != "$actual_checksum" ]]; then
    fail "Checksum verification failed! Expected: $expected_checksum, got: $actual_checksum"
  fi

  info "Checksum verified"

  # Extract archive and binary
  info "Extracting archive"
  tar -xzf "$archive_name"
  mkdir -p "$bin_dir"

  local extracted_binary
  extracted_binary=$(find . -name "$binary_name" -type f | head -n 1)
  if [[ -z "$extracted_binary" ]]; then
    fail "Could not find $binary_name in the extracted archive"
  fi
  cp "$extracted_binary" "$bin_dir/$binary_name"
  chmod +x "$bin_dir/$binary_name"

  # Clean up
  cd - > /dev/null
  rm -rf "$workdir"

  local installed_version
  installed_version=$("$bin_dir/$binary_name" --version 2>/dev/null || echo "bastion")
  info "Installed '$installed_version' to '$bin_dir/$binary_name'"

  # Check if bin_dir is in PATH
  if [[ ":$PATH:" != *":$bin_dir:"* ]]; then
    warn "'$bin_dir' is not in your PATH. You may need to add it to your shell's configuration."
    echo "" >&2
    echo "Add the following to your shell configuration file:" >&2
    echo "  export PATH=\"$bin_dir:\$PATH\"" >&2
  fi
}

# Main function
main() {
  # Set defaults
  local VERSION=""
  local BIN_DIR="$HOME/.local/bin"
  local TMP_DIR="${TMPDIR:-/tmp}"
  local LIBC="${BASTION_LIBC:-}"

  # Parse command line arguments
  parse_args "$@"

  # Check for required commands
  check_requirements

  # Detect platform
  local PLATFORM
  PLATFORM=$(detect_platform)
  info "Detected platform: $PLATFORM"

  # If version not specified, get latest
  if [[ -z "$VERSION" ]]; then
    VERSION=$(get_latest_version)
    info "Installing latest version: $VERSION"
  else
    info "Installing version: $VERSION"
  fi

  # Install binary
  install_binary "$PLATFORM" "$VERSION" "$BIN_DIR" "$TMP_DIR"

  echo "" >&2
  info "Installation complete! Run 'bastion --help' to get started."
}

# Run main function
main "$@"
