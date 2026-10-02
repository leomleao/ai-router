#!/bin/sh
# Immutable official release artifacts, taken from the official installer's
# Linux manifests. Do not replace these with a latest-version download.
set -eu
case "${1:-$(uname -m)}" in
  arm64|aarch64)
    release_platform=linux-arm
    release_file=cli_linux_arm64.tar.gz
    release_sha512=199e6419fd7549fa296f51c6035c9e0246f9ecd39b6624c46ff9a416f72b0b6579999c692c9499d218a9f88746d60ee78d0f592b31c8fab8be85b0d40533991d
    ;;
  amd64|x86_64)
    release_platform=linux-x64
    release_file=cli_linux_x64.tar.gz
    release_sha512=6d2e2eeda0cad6eac8e8b2df11257d684210f8d384a2ee011dc6ad0edacfa33e1ec9c554589f6ff6ef0697f4c3c777ba46b6e84639711f173377cc1554c67436
    ;;
  *) echo 'Unsupported AGY Linux architecture' >&2; exit 1 ;;
esac
release_url="https://storage.googleapis.com/antigravity-public/antigravity-cli/1.2.15-5434575321694208/$release_platform/$release_file"
staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --connect-timeout 20 --max-time 300 "$release_url" -o "$staging_dir/agy.tar.gz"
printf '%s  %s\n' "$release_sha512" "$staging_dir/agy.tar.gz" | sha512sum --check --status
# Extract only the known executable, never all archive paths.
tar -xzf "$staging_dir/agy.tar.gz" -C "$staging_dir" antigravity
install -m 0755 "$staging_dir/antigravity" /usr/local/bin/agy
test "$(/usr/local/bin/agy --version)" = '1.2.15'
