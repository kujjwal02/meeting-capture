#!/bin/sh
# Creates a self-signed code-signing identity in the login keychain, once per Mac.
# macOS ties mic and audio grants to the signing identity. A stable identity keeps those grants
# across rebuilds; an ad-hoc signature changes with every build.
# Usage: create-signing-cert.sh "<identity name>". `just signing-cert` passes the name.
set -eu

name=${1:?usage: create-signing-cert.sh "<identity name>"}
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
# The password only protects this temporary file, which the trap deletes.
"$openssl" pkcs12 -export -legacy -name "$name" -passout pass:meeting-capture \
  -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/identity.p12"
# -x keeps the private key from being exported out of the keychain.
security import "$work/identity.p12" -k login.keychain-db -P meeting-capture -x \
  -T /usr/bin/codesign
echo "Trust the certificate for code signing. macOS asks for your login password."
security add-trusted-cert -p codeSign -k login.keychain-db "$work/cert.pem"
security find-identity -v -p codesigning | grep "\"$name\""
