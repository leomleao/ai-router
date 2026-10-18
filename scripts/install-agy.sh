#!/bin/sh
# Immutable official release artifacts, taken from the official installer's
# Linux manifests. Do not replace these with a latest-version download.
set -eu
case "${1:-$(uname -m)}" in
  arm64|aarch64)
    release_platform=linux-arm
    release_file=cli_linux_arm64.tar.gz
    release_sha512=f4655468d3303e6039cbcf1a30769e3cb5c4e16d149fba0d08e83f91f0f2b07cb623a4adb8615ca7a8fc73bf2445301e0e1ab82ea89f6aa44cbd9a9e96703e19
    ;;
  amd64|x86_64)
    release_platform=linux-x64
    release_file=cli_linux_x64.tar.gz
    release_sha512=4a393afc1369cee94e817c1b430c9128d02b12ebd4d5703ace5cd47b2333e216d673fb0097cbf7195f757b110163ba9850e9eea3fdbbaddd4b25668b82c03f05
    ;;
  *) echo 'Unsupported AGY Linux architecture' >&2; exit 1 ;;
esac
release_url="https://storage.googleapis.com/antigravity-public/antigravity-cli/1.4.2-4835981233356800/$release_platform/$release_file"
staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --connect-timeout 20 --max-time 300 "$release_url" -o "$staging_dir/agy.tar.gz"
printf '%s  %s\n' "$release_sha512" "$staging_dir/agy.tar.gz" | sha512sum --check --status
# Extract only the known executable, never all archive paths.
tar -xzf "$staging_dir/agy.tar.gz" -C "$staging_dir" antigravity
install -m 0755 "$staging_dir/antigravity" /usr/local/bin/agy
test "$(/usr/local/bin/agy --version)" = '1.4.2'
