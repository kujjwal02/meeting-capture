#!/bin/sh
# Creates the self-signed code-signing identity in the login keychain. Run it once; on another Mac,
# import the .p12 it writes instead.
# macOS ties mic and audio grants to the signing identity. A stable identity keeps those grants
# across rebuilds; an ad-hoc signature changes with every build.
# The release workflow signs with the same identity, so the script also writes it as a PKCS#12 file
# for the GitHub secret. Store that file and its password in a password manager: a new identity
# makes every Mac grant the app's permissions again.
# Usage: create-signing-cert.sh "<identity name>" <output .p12>. `just signing-cert` passes both.
set -eu

name=${1:?usage: create-signing-cert.sh "<identity name>" <output .p12>}
p12_out=${2:?usage: create-signing-cert.sh "<identity name>" <output .p12>}
if security find-identity -v -p codesigning | grep -q "\"$name\""; then
  echo "\"$name\" already exists"
  exit 0
fi

# macOS ships LibreSSL, which cannot write the legacy PKCS#12 the keychain reads.
openssl=$(brew --prefix openssl@3 2>/dev/null)/bin/openssl
if ! "$openssl" version 2>/dev/null | grep -q '^OpenSSL 3'; then
  echo "OpenSSL 3 is missing. Install it with: brew install openssl@3" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
"$openssl" req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$name" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -keyout "$work/key.pem" -out "$work/cert.pem"
umask 077
"$openssl" rand -base64 24 | tr -d '\n' > "$p12_out.password"
"$openssl" pkcs12 -export -legacy -name "$name" -passout file:"$p12_out.password" \
  -inkey "$work/key.pem" -in "$work/cert.pem" -out "$p12_out"
security import "$p12_out" -k login.keychain-db -P "$(cat "$p12_out.password")" \
  -T /usr/bin/codesign
echo "Let codesign use the key without a prompt. macOS asks for your login password."
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -D "$name" login.keychain-db >/dev/null
echo "Trust the certificate for code signing. macOS asks for your login password."
security add-trusted-cert -p codeSign -k login.keychain-db "$work/cert.pem"
security find-identity -v -p codesigning | grep "\"$name\""
echo "Wrote $p12_out and $p12_out.password. Store both in a password manager."
