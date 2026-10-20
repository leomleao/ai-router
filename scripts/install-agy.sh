#!/bin/sh
# Immutable official release artifacts, taken from the official installer's
# Linux manifests. Do not replace these with a latest-version download.
set -eu
case "${1:-$(uname -m)}" in
  arm64|aarch64)
    release_platform=linux-arm
    release_file=cli_linux_arm64.tar.gz
    release_sha512=a906ef290e077871b333cb6f13d4d76ccdfc03ad15f012d6b745c02fa471edc27c10bc3059b87142bf6de0bacd64328f62e608d5736655524b97e8d4b4a0882a
    ;;
  amd64|x86_64)
    release_platform=linux-x64
    release_file=cli_linux_x64.tar.gz
    release_sha512=9b726b86280a56e8ffa84b3b3a1c00d19972833a1f6c0b21d85c5fc6e408af6492e4c61353cad9b500bee599af264c92cfc2ead1f5048ab9525328e1e4199c40
    ;;
  *) echo 'Unsupported AGY Linux architecture' >&2; exit 1 ;;
esac
release_url="https://storage.googleapis.com/antigravity-public/antigravity-cli/1.4.3-4577820882632704/$release_platform/$release_file"
staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --connect-timeout 20 --max-time 300 "$release_url" -o "$staging_dir/agy.tar.gz"
printf '%s  %s\n' "$release_sha512" "$staging_dir/agy.tar.gz" | sha512sum --check --status
# Extract only the known executable, never all archive paths.
tar -xzf "$staging_dir/agy.tar.gz" -C "$staging_dir" antigravity
install -m 0755 "$staging_dir/antigravity" /usr/local/bin/agy
test "$(/usr/local/bin/agy --version)" = '1.4.3'
