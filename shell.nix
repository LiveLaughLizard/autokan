{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  # bindgenHook lets the SAML dependency (samael -> xmlsec1) generate its C bindings.
  nativeBuildInputs = with pkgs; [ cargo rustc clippy rustfmt gcc pkg-config rustPlatform.bindgenHook nodejs just kubernetes-helm ];
  # C libraries for SAML signature verification. Not needed with `--no-default-features`.
  buildInputs = with pkgs; [ libxml2 xmlsec libxslt libtool openssl ];
}
