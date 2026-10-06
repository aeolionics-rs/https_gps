#!/bin/sh
# Generates the self-signed certificate and private key that the firmware
# serves HTTPS with. They're written to certs/ (which git ignores) and embedded
# into the firmware at build time.
#
# Usage: tools/gen-cert.sh [IP-address-or-hostname ...]
#
# Each argument is added to the certificate's subject alternative names, so
# clients that trust the certificate can also verify the board's address,
# e.g. tools/gen-cert.sh 192.168.178.80
set -eu

cd "$(dirname "$0")/.."
mkdir -p certs

san="DNS:https-gps.local"
for name in "$@"; do
    case $name in
        *[!0-9.]*) san="$san,DNS:$name" ;;
        *) san="$san,IP:$name" ;;
    esac
done

openssl ecparam -genkey -name prime256v1 -noout -out certs/server.key.pem
openssl req -new -x509 -key certs/server.key.pem -days 3650 \
    -subj "/CN=https-gps" \
    -addext "subjectAltName=$san" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=serverAuth" \
    -out certs/server.crt.pem

# The firmware embeds the DER encodings.
openssl x509 -in certs/server.crt.pem -outform DER -out certs/server.crt.der
openssl pkey -in certs/server.key.pem -outform DER -out certs/server.key.der

echo "Wrote certs/server.{crt,key}.{pem,der} for $san"
