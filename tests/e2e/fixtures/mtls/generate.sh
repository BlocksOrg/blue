#!/bin/sh
set -eu

out=/certs
rm -f "$out"/*

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
  -subj /CN=blue-e2e-ca -keyout "$out/ca.key" -out "$out/ca.crt"

openssl req -newkey rsa:2048 -nodes -sha256 -subj /CN=blue-control-api \
  -keyout "$out/tls.key" -out "$out/server.csr"
printf '%s\n' 'subjectAltName=IP:127.0.0.1,DNS:blue' 'extendedKeyUsage=serverAuth' > "$out/server.ext"
openssl x509 -req -days 2 -sha256 -in "$out/server.csr" \
  -CA "$out/ca.crt" -CAkey "$out/ca.key" -CAcreateserial \
  -extfile "$out/server.ext" -out "$out/tls.crt"

openssl req -newkey rsa:2048 -nodes -sha256 -subj /CN=blue-inference-proxy \
  -keyout "$out/client.key" -out "$out/client.csr"
printf '%s\n' 'extendedKeyUsage=clientAuth' > "$out/client.ext"
openssl x509 -req -days 2 -sha256 -in "$out/client.csr" \
  -CA "$out/ca.crt" -CAkey "$out/ca.key" -CAcreateserial \
  -extfile "$out/client.ext" -out "$out/client.crt"
cat "$out/client.crt" "$out/client.key" > "$out/client.pem"

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -sha256 \
  -subj /CN=blue-e2e-rogue-ca -keyout "$out/rogue-ca.key" -out "$out/rogue-ca.crt"
openssl req -newkey rsa:2048 -nodes -sha256 -subj /CN=rogue-client \
  -keyout "$out/rogue-client.key" -out "$out/rogue-client.csr"
openssl x509 -req -days 2 -sha256 -in "$out/rogue-client.csr" \
  -CA "$out/rogue-ca.crt" -CAkey "$out/rogue-ca.key" -CAcreateserial \
  -extfile "$out/client.ext" -out "$out/rogue-client.crt"

# These are disposable E2E credentials shared across containers in one
# short-lived Compose project, not production key permissions.
chmod 0444 "$out"/*
