#!/bin/sh
# Regenerates the TLS test certificates used by tests/tls.rs:
#   ca.der             self-signed test root CA (trusted via Config::tls_root_certificates)
#   localhost.der      leaf certificate for the DNS name `localhost` only (no IP
#                      SANs), signed by the CA
#   localhost.key.der  the leaf's private key (PKCS#8)
# PEM copies of the certificates are kept for `openssl x509 -text -in ...`.
# Both certificates are valid from 2025 for about 100 years (tolerating clock
# skew; `-not_before` needs OpenSSL 3.4 or later). The CA key is discarded.
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/ca.key"
openssl req -x509 -new -key "$tmp/ca.key" -sha256 -not_before 20250101000000Z -not_after 21250101000000Z -subj "/CN=Sand Test Root CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -addext "subjectKeyIdentifier=hash" \
    -out ca.pem

openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/leaf.key"
openssl req -new -key "$tmp/leaf.key" -subj "/CN=localhost" -out "$tmp/leaf.csr"
cat > "$tmp/leaf.ext" <<EXT
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
authorityKeyIdentifier=keyid
subjectKeyIdentifier=hash
EXT
openssl x509 -req -in "$tmp/leaf.csr" -CA ca.pem -CAkey "$tmp/ca.key" -CAserial "$tmp/ca.srl" -CAcreateserial \
    -sha256 -not_before 20250101000000Z -not_after 21250101000000Z -extfile "$tmp/leaf.ext" -out localhost.pem

openssl x509 -in ca.pem -outform DER -out ca.der
openssl x509 -in localhost.pem -outform DER -out localhost.der
openssl pkcs8 -topk8 -nocrypt -in "$tmp/leaf.key" -outform DER -out localhost.key.der
