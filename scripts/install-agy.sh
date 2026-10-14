#!/bin/sh
# Immutable official release artifacts, taken from the official installer's
# Linux manifests. Do not replace these with a latest-version download.
set -eu
case "${1:-$(uname -m)}" in
  arm64|aarch64)
    release_platform=linux-arm
    release_file=cli_linux_arm64.tar.gz
    release_sha512=8b57ef509bdddf6ed02824ba8cdf44ec4bcefef91066e1215d5c39566181d10f7b93206545b97fa2683595a595e12311b10cd34c12cd58b3afed65d4a70ac4dd
    ;;
  amd64|x86_64)
    release_platform=linux-x64
    release_file=cli_linux_x64.tar.gz
    release_sha512=0fa5ff7ca8661ef6379da219dd30552327b0deaf112ea896882daaba8bd6abff76ff097ef7b38211bffb78be6871f7494a1739b7160b483cb68481a24d435c47
    ;;
  *) echo 'Unsupported AGY Linux architecture' >&2; exit 1 ;;
esac
release_url="https://storage.googleapis.com/antigravity-public/antigravity-cli/1.3.5-6164039479459840/$release_platform/$release_file"
staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --connect-timeout 20 --max-time 300 "$release_url" -o "$staging_dir/agy.tar.gz"
printf '%s  %s\n' "$release_sha512" "$staging_dir/agy.tar.gz" | sha512sum --check --status
# Extract only the known executable, never all archive paths.
tar -xzf "$staging_dir/agy.tar.gz" -C "$staging_dir" antigravity
install -m 0755 "$staging_dir/antigravity" /usr/local/bin/agy
test "$(/usr/local/bin/agy --version)" = '1.3.5'
