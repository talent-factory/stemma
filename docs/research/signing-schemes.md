# Signing schemes of Git, Jujutsu and Sigstore/gitsign

Research note for STEMMA-223 (phase 4: signatures and keys). It collects facts for the signature
decision of phase 4: the signing schemes of Git (OpenPGP, SSH, X.509), Jujutsu, and
Sigstore/gitsign; where keys live and how they are rotated or revoked; what is signed; whether a
signature can be verified without a server; and which Rust crates exist and how they are
maintained. It records facts only and decides nothing for Stemma.

## Sources and method

Sources are primary: the documentation and specifications of the projects that own the behaviour,
read on 2026-11-11 from the default branch of each repository (not pinned to a revision, so
details can move). Each claim names its source in square brackets.

- `[git-sig]` <https://git-scm.com/docs/gitformat-signature>
- `[git-gpg]` `Documentation/config/gpg.adoc` in <https://github.com/git/git>
- `[git-push]`, `[git-recv]` `git-push.adoc` and `git-receive-pack.adoc` in the same repository
- `[sshsig]` `PROTOCOL.sshsig` in <https://github.com/openssh/openssh-portable>
- `[ssh-keygen]` <https://man.openbsd.org/ssh-keygen.1>
- `[rfc9580]` <https://www.rfc-editor.org/rfc/rfc9580.txt> (OpenPGP)
- `[jj-config]` `docs/config.md` and `docs/git-compatibility.md` in <https://github.com/jj-vcs/jj>
- `[gitsign]` `README.md` in <https://github.com/sigstore/gitsign>
- `[gitsign-cms]` `docs/bundle-cms.md` in the same repository
- `[sigstore-sec]` <https://docs.sigstore.dev/about/security/>
- `[sigstore-bundle]` `protos/sigstore_bundle.proto` in
  <https://github.com/sigstore/protobuf-specs>
- `[cosign-verify]` <https://docs.sigstore.dev/cosign/verifying/verify/>
- `[dsse]` `protocol.md` in <https://github.com/secure-systems-lab/dsse>
- `[in-toto]` `spec/v1/README.md` and `spec/v1/statement.md` in
  <https://github.com/in-toto/attestation>
- `[crates.io]` the crates.io API, queried on 2026-11-11, and the README of each crate repository

Not verified (no primary source retrieved): the revocation behaviour of X.509 signatures through
`gpgsm` (CRL and OCSP handling), the exact format of the Git push certificate (the pages read
describe only the hook side), and whether `gix-object` can verify signatures.

## 1. Signing schemes of Git

### 1.1 One mechanism, three formats

Git builds a payload from the object (or transaction), calls an external program for a
detached signature, and embeds that signature in the object [git-sig]. The format is chosen with
`gpg.format`; the default is `openpgp`, the other values are `x509` and `ssh` [git-gpg]. Armor
lines differ per format [git-sig]:

| `gpg.format` | Armor | Default program | Verification |
|---|---|---|---|
| `openpgp` | `-----BEGIN PGP SIGNATURE-----` | `gpg` | `gpg --verify` [git-gpg] |
| `x509` | `-----BEGIN SIGNED MESSAGE-----` | `gpgsm` | `gpgsm` [git-gpg] |
| `ssh` | `-----BEGIN SSH SIGNATURE-----` | `ssh-keygen` | `ssh-keygen -Y` [git-gpg] |

Git delegates all cryptography to those programs. `gpg.<format>.program` replaces the program.
For OpenPGP the replacement must mimic `gpg --verify $signature - <$file` (exit code 0 means
good) and `gpg -bsau $key` (armored detached signature on standard output) [git-gpg].

### 1.2 What is signed and where the signature sits

[git-sig] defines three embeddings plus the signed push:

- **Commit** (`git commit -S`): the payload is the commit object without the signature. The
  signature is the multi-line header `gpgsig`; continuation lines start with one space, also the
  empty ones.
- **Tag** (`git tag -s`): the payload is the annotated tag object. The signature block is
  appended after the tag message.
- **Mergetag** (`git merge` of a signed tag): the whole signed tag object is embedded in the merge
  commit as the header `mergetag`.
- **Push** (`git push --signed`): the push request is signed so that the receiving side can check
  it with hooks or log it. `--signed=true` fails if the server does not support signed pushes;
  `if-asked` signs only if the server does [git-push].

The header is named `gpgsig` for all three formats. A commit signature covers the tree, the
parents, the author and the committer lines, and the message, because those are the bytes of the
commit object without the header. It is a signature of one commit, not of the history: the parent
IDs are inside the payload, so the hash chain (ADR-001) carries the history.

### 1.3 Keys, trust, rotation, revocation per format

**OpenPGP** (`gpg`). Trust comes from the keyring of the verifier and its web of trust.
`gpg.minTrustLevel` sets the minimum level (`undefined`, `never`, `marginal`, `fully`,
`ultimate`); unset, merges need `marginal` and other operations `undefined` [git-gpg]. The
standard describes expiry and revocation in the key itself [rfc9580]:

- Expiry is a self-signature subpacket, "Key Expiration Time" (seconds after key creation); absent
  or zero means never.
- Revocation is a signature of type 0x20 (key), 0x28 (subkey), or 0x30 (certification).
  "A revoked key is not to be used."
- The "Reason for Revocation" subpacket separates the cases: code 2 (compromised) means "all
  signatures created by that key are suspect"; codes 1 (superseded) and 3 (retired) mean "old
  signatures are still valid"; code 0 (no reason) is not defined further.
- A revocation is a signed packet that travels with the public key, so it can be published and
  imported later (escrowed revocation signatures are mentioned in the table of contents; the
  section itself was not read).

**SSH** (`ssh`). SSH has no trust levels. A signature is `fully` trusted when its key is in the
file `gpg.ssh.allowedSignersFile`; otherwise it is `undefined` and `git verify-commit` and
`git verify-tag` fail [git-gpg]. Facts about that file [git-gpg] [ssh-keygen]:

- Line format: principals, options, key type, base64 key. Options include `cert-authority`
  (an SSH CA may sign certificates for signers), `namespaces`, `valid-after`, and `valid-before`.
- The file may live outside the repository, may be generated by a server from the keys of users
  with push access, or may be stored in the repository (a path relative to the top of the working
  tree), so that only committers with an already valid key can change the keyring [git-gpg].
- Rotation: since OpenSSH 8.8 the options `valid-after` and `valid-before` bound a key. Git
  checks the committer or tagger timestamp against that range. The page notes that this "gives
  no real security guarantee because the signer can choose the timestamp arbitrarily", but it
  lets users change signing keys without invalidating earlier signatures [git-gpg].
- Revocation: `gpg.ssh.revocationFile` is an OpenSSH KRL or a plain list of revoked public keys.
  A key found there is always trust level `never` and its signatures show as invalid. Note that
  this is not time-aware: every signature of the key becomes invalid, including old ones [git-gpg].
- `gpg.ssh.defaultKeyCommand` can supply the key when `user.signingKey` is unset, for example
  when keys or SSH certificates rotate often [git-gpg].
- A KRL is binary, lists keys by hash or content and certificates by serial number or key ID, and
  can be queried with `ssh-keygen -Q` [ssh-keygen].

**X.509** (`x509`). Git calls `gpgsm`; the signature is a CMS message. Trust, expiry and
revocation follow the X.509 rules of `gpgsm` (not verified here, see above).

### 1.4 The SSH signature format (SSHSIG)

The format is specified in `PROTOCOL.sshsig` [sshsig]:

- The armored blob holds a preamble `SSHSIG`, a version (1), the public key, a namespace, a
  reserved string, a hash algorithm (`sha256` or `sha512`), and the signature.
- The signed data is not the message but the concatenation of the preamble, the namespace, the
  reserved string, the hash algorithm, and the hash of the message. The namespace "MUST NOT be the
  empty string"; it guards against signatures reused in another protocol. Git uses the namespace
  `git` (the page of the specification names no namespace; this value is not verified here).
- RSA signatures must use `rsa-sha2-256` or `rsa-sha2-512`, not legacy `ssh-rsa`.
- Only detached, armored signatures are supported.
- `ssh-keygen -Y verify` takes the namespace, the signer identity, the allowed signers file, and
  optionally a revocation file; `-O verify-time=` validates at a given time [ssh-keygen].

### 1.5 Verification without a server

All three formats verify from local material:

- OpenPGP: the public key, with its revocation signatures, in the local keyring.
- SSH: the allowed signers file and optional revocation file, which may be a file in the
  repository [git-gpg].
- X.509: the certificate chain and trust roots known to `gpgsm`.

No server is asked in any case. Trust is a local decision: the verifier decides which keys are
accepted. A pushed signature check on the server side is a separate, optional use.

### 1.6 Signed push (push certificate)

`git push --signed` sends a signed certificate; the receiver stores it as a blob and exposes it to
`pre-receive` and `post-receive` through `GIT_PUSH_CERT` (the blob), `GIT_PUSH_CERT_SIGNER`,
`GIT_PUSH_CERT_KEY`, `GIT_PUSH_CERT_STATUS` (the GPG result), and `GIT_PUSH_CERT_NONCE` [git-recv].
Replay protection uses a server-chosen nonce; `GIT_PUSH_CERT_NONCE_STATUS` is one of `UNSOLICITED`,
`MISSING`, `BAD`, `OK`, `SLOP`, and a mismatch "may indicate that the certificate is a valid one
that is being replayed" [git-recv]. The pages read say the check is done with GPG; whether SSH
signatures work for push certificates is not stated there. This is the existing Git analogue of
the signed ref-update record that ADR-003 proposes for the hub.

## 2. Jujutsu

- Backends: `gpg`, `gpgsm` (X.509 / PKCS#12 certificates), `ssh`, or `none` (signing off)
  [jj-config]. The Git-compatible commits carry the same `gpgsig` header as Git; the
  compatibility page lists "Signed commits: Yes" [jj-config].
- `signing.behavior`: `drop` (never sign, discard a signature when a commit is modified), `keep`
  (re-sign a modified commit if it was signed and you authored it), `own` (sign every commit you
  authored when you modify it), `force` (sign every modified commit). Because `jj` rewrites
  commits often, signatures are recreated often. `git-sign-on-push` signs matching commits just
  before a push [jj-config].
- `jj sign` and `jj unsign` sign by hand; `jj sign` "always signs commits, even if they are
  already signed by the user" [jj-config].
- Verification and display are disabled by default; `ui.show-cryptographic-signatures = true`
  enables them. Verification uses the settings of the backend: for SSH an allowed signers file
  (`backends.ssh.allowed-signers`) and a revocation list (`backends.ssh.revocation-list`), where a
  signature from a listed key is marked invalid; for gpg and gpgsm the option
  `allow-expired-keys` (expired keys are rejected by default) [jj-config].
- Tags: the compatibility page says only that annotated tags cannot be created, and does not
  mention signed tags [jj-config].
- Sigstore: the official configuration page does not mention Sigstore or gitsign. A third-party
  page describes wiring gitsign through the `gpgsm` backend; that claim is not from a primary
  source and was not tested.

## 3. Sigstore and gitsign

### 3.1 How gitsign signs

[gitsign] signs Git commits (and tags) with `gpg.format x509` and `gpg.x509.program gitsign`:

- The signature is CMS/PKCS7 in the `gpgsig` header and can be inspected with `openssl`.
- Keyless flow: the user logs in with an OIDC identity in the browser; Fulcio issues a short-lived
  code signing certificate (about ten minutes in the example) for an ephemeral key. The identity
  (an email address) is the certificate's SAN and the OIDC issuer is a Fulcio extension. Default
  Fulcio is `https://fulcio.sigstore.dev`.
- Because the certificate expires, a later verifier needs proof that the signature was made while
  it was valid: Rekor, the transparency log, provides it. "Fulcio was designed to avoid
  revocation by issuing short-lived certificates instead" [sigstore-sec]; the verifier checks the
  signed Rekor timestamp against the certificate validity [sigstore-sec].

### 3.2 What is stored where

- Online mode (default): a HashedRekord entry (SHA-256 of the commit and the certificate) is
  uploaded; it is not embedded, so verification looks the entry up through the Rekor API
  [gitsign].
- Offline mode (new, "experimental" in the README): the Rekor entry is embedded in the CMS
  signature as an unsigned attribute with OID `1.3.6.1.4.1.57264.3.1`; an RFC 3161 timestamp can be
  embedded as `1.2.840.113549.1.9.16.2.14`. Only the leaf certificate is in the signature;
  "intermediates and roots come from the trusted root, not the signature" [gitsign-cms].
  The on-disk CMS format is the same whether the option is set [gitsign-cms].
- The README states that online verification "will be deprecated in favor of offline in the
  future" [gitsign].

### 3.3 Verification

- `gitsign verify --certificate-identity=... --certificate-oidc-issuer=... <rev>` checks the
  signature and the expected identity. Plain `git verify-commit` cannot check who signed
  ("Validated Certificate claims: false") [gitsign].
- Offline verification needs the trust root (the Sigstore root, distributed through TUF) and the
  embedded Rekor entry; neither needs a call to Rekor. The Sigstore trust root "consists of a
  rotation of five keyholders" and follows TUF [sigstore-sec].
- A verifier "MUST verify that the signature was computed at the time the certificate was valid";
  the bundle carries the log entries and timestamps that make this possible, but trust must come
  from outside the bundle [sigstore-bundle].
- Limits stated by the README: signing needs the Internet and a browser login; GitHub does not
  show gitsign commits as verified; public Rekor entries may expose emails or repository
  identifiers [gitsign]. The Sigstore model needs third parties to monitor the logs, otherwise
  "any misbehavior by Rekor and Fulcio might go undetected" [sigstore-sec].

### 3.4 Rotation and revocation

There is no revocation. The key is ephemeral ("only exists in memory") and the certificate lives
minutes; compromise of an OIDC account is detected by monitoring the certificate log [sigstore-sec].
Rotation of the trust root happens through TUF [sigstore-sec]. Self-hosted Fulcio, Rekor or a
custom CA is possible (`SIGSTORE_ROOT_FILE`, `--ca-roots`) [cosign-verify], at the price of
running that infrastructure.

## 4. What is signed: commit, tag, attestation

| Thing | Git | Jujutsu | Sigstore |
|---|---|---|---|
| Commit | `gpgsig` header over the commit object | same header | gitsign: CMS in `gpgsig` |
| Tag | signature appended to the tag object | not stated; no annotated tags | not verified |
| Ref update | signed push certificate | not applicable (no hub in `jj`) | not applicable |
| Approval, review | no native object | none | attestation (below) |
| Generic statement | none | none | DSSE envelope with in-toto Statement |

Attestations are not a Git feature. The generic format of the Sigstore ecosystem is:

- **DSSE** [dsse]: `SIGNATURE = Sign(PAE(UTF8(PAYLOAD_TYPE), SERIALIZED_BODY))` with
  `PAE(type, body) = "DSSEv1" SP LEN(type) SP type SP LEN(body) SP body`. The payload type is
  authenticated, so a signed message cannot be reinterpreted in another context; `KEYID` is only a
  hint. A verifier "MUST NOT re-parse the envelope after verification to pull out the payload".
  An envelope may carry several signatures and be accepted if at least `t` of `n` trusted keys
  verify.
- **in-toto Statement v1** [in-toto]: fields `_type`, `subject` (artifacts, each with a digest map
  from algorithm to hex value), `predicateType` (a URI), and an optional `predicate`. Subjects are
  matched by digest only. The specification does not list permitted digest algorithms, so a
  `b3:` digest would need a name for the algorithm key (not decided anywhere).
- **Sigstore bundle** [sigstore-bundle]: wraps either a message signature or a DSSE envelope with
  the verification material (a leaf certificate or a public key hint), log entries and RFC 3161
  timestamps. Version 0.3 has the media type `application/vnd.dev.sigstore.bundle.v0.3+json`;
  keyless signing uses the single-certificate form.

## 5. Facts that bear on Stemma's ADRs

These are constraints read from the repository, not a decision.

- ADR-001: object IDs have the form `b3:<hex>`; SHA-1 is not used. Git's `gpgsig` header signs the
  bytes of the object and does not depend on the hash function, so a signature over a Stemma
  commit encoding is independent of Git's SHA-1 or SHA-256 object format. A DSSE or in-toto
  subject digest would name the algorithm explicitly.
- ADR-002: every commit and operation log entry records an actor (human or agent with model,
  session, sponsor). A signature of a key says nothing about this actor line unless the key is
  bound to it; the open item "agent identities" belongs to phase 4.
- ADR-003 (proposal): signatures reside in the repository and are verified independently of the
  server; the hub keeps a signed record of ref updates. The facts above show the Git forms of both:
  SSH allowed signers and revocation files stored in the repository, and the push certificate.
- ADR-004 (proposal): approvals are signed; "signatures and keys" and "cryptography crates" are
  level 3 (human owner, plus an ADR); the policy file is itself level 3 and signed.

## 6. Rust crates

Data from the crates.io API (2026-11-11). "Updated" is the `updated_at` field of the crate record
(it can change for reasons other than a release). Maintenance state is stated only where the
project says it; everything else is derived from the version and date and is marked "derived".

- `ssh-key` 0.6.7 (updated 2026-11-09): SSH keys, SSHSIG signing and verification, OpenSSH
  certificates. RustCrypto; feature-gated algorithms (none enabled by default); FIDO keys verify
  only; no audit statement in the README.
- `sequoia-openpgp` 2.4.2 (updated 2026-10-14): full OpenPGP (RFC 9580), sign, verify, revocation.
  LGPL-2+ license; default backend Nettle (C library), other backends selectable; companion `sqv`
  verifier.
- `pgp` (rPGP) 0.21.0 (updated 2026-10-03): OpenPGP, RFC 9580, sign and verify. Apache-2.0 or MIT;
  low-level: "explicitly does not deal with" expiry, revocation and key flags (layer 4); points to
  a `SECURITY_STATUS.md`.
- `sigstore` (sigstore-rs) 0.15.0 (updated 2026-11-05): cosign-style verify, Fulcio and Rekor
  clients. official org; "will not be considered stable until the 1.0 release"; "does not handle
  verification of attestations yet".
- `sigstore-verify` and related 0.16.0 (updated 2026-11-09): bundle v0.1-0.3, keyless
  verification, Rekor inclusion proofs and SETs, RFC 3161, DSSE. repo `sigstore/sigstore-rust` (a
  separate workspace of 12 crates); README gives no stability statement; default backend
  `aws-lc-rs`; relation to `sigstore-rs` not stated.
- `x509-cert`, `cms` 0.3.0, 0.2.3 (updated 2026-07, 2026-11): parse X.509 certificates and CMS
  (for reading gitsign signatures). RustCrypto `formats`; parsing, not path validation.
- `rustls-webpki` 0.103.16 (updated 2026-10-20): X.509 path validation (web PKI profile). widely
  used; a web-PKI profile, not a general code-signing validator (derived).
- `ed25519-dalek` 3.0.0 (updated 2026-07-06): Ed25519 signatures. most downloaded signature crate.
- `ring` 0.17.14 (updated 2025-03-11): Ed25519, ECDSA, RSA. no release for over a year (derived).
- `aws-lc-rs` 1.19.0 (updated 2026-10-28): same primitives, FIPS-capable build. AWS; C library
  underneath.
- `signature` / `ed25519` 3.0.0 (updated 2026-05): RustCrypto traits. trait crates.
- `minisign-verify` 0.3.0 (updated 2026-09-25): verify minisign signatures only. tiny,
  verify-only.
- `in-toto` 0.4.0 (updated 2024-12-11): in-toto layouts and links. no release for almost two years
  (derived).
- `dsse` 0.1.6 (updated 2026-10-30): DSSE envelope. created 2026-09; young, 20k downloads.
- `gix-object` 0.66.1 (updated 2026-10-22): gitoxide object parsing. whether it verifies
  signatures is not verified.

Observations:

- Only `ssh-key` and `sequoia-openpgp`/`pgp` implement a Git signature format end to end for
  creating and checking, in Rust, without calling an external program. Git itself always calls
  `gpg`, `gpgsm` or `ssh-keygen`.
- `ssh-key` supports SSHSIG, which matches the format Git uses with `gpg.format ssh` [sshsig].
- OpenPGP trust semantics (expiry, revocation, web of trust) are in `sequoia-openpgp`; `pgp` leaves
  them to the caller.
- Offline verification of Sigstore signatures in Rust is covered by `sigstore-verify`; the official
  `sigstore` crate is pre-1.0 and has no attestation verification.
- Cryptography crates are level 3 in ADR-004, so every new one needs an ADR; `deny.toml` and
  `cargo audit` apply (CLAUDE.md, `just deps`).

## 7. Comparison

- OpenPGP:
  - Key storage: keyring, smart card, agent
  - Trust root: web of trust / own keyring
  - Expiry: key expiration subpacket
  - Revocation: signed revocation packet, with reason (hard or soft)
  - Offline verify: yes
  - Needs a server to sign: no
  - Git support: `gpg.format openpgp`
  - Jujutsu support: `gpg`
- SSH (SSHSIG):
  - Key storage: file, `ssh-agent`, FIDO key
  - Trust root: allowed signers file
  - Expiry: `valid-after` / `valid-before`
  - Revocation: KRL / revoked-keys file, all signatures of the key
  - Offline verify: yes
  - Needs a server to sign: no
  - Git support: `gpg.format ssh`
  - Jujutsu support: `ssh`
- X.509 (`gpgsm`):
  - Key storage: `gpgsm` store, PKCS#12
  - Trust root: CA chain
  - Expiry: certificate validity
  - Revocation: CRL / OCSP (not verified)
  - Offline verify: yes
  - Needs a server to sign: no (CA to issue)
  - Git support: `gpg.format x509`
  - Jujutsu support: `gpgsm`
- Sigstore/gitsign:
  - Key storage: ephemeral, none stored
  - Trust root: Sigstore TUF root, identity policy
  - Expiry: about 10 minutes, by design
  - Revocation: none; short lifetime and log monitoring
  - Offline verify: yes, with embedded entry and local trust root
  - Needs a server to sign: yes (OIDC, Fulcio, Rekor)
  - Git support: `gpg.format x509` with `gitsign`
  - Jujutsu support: not documented

## 8. Open questions this note cannot answer

- Revocation and CRL behaviour of `gpgsm` for Git X.509 signatures.
- The byte format of the Git push certificate, and whether SSH keys can sign it.
- Whether a Stemma commit should be signed in a Git-compatible header or in a Stemma-specific
  field (a format question for ADR-005 and the phase 4 ADR).
- Whether the time-unaware SSH revocation (all signatures of a key become invalid) or the
  OpenPGP reason-aware model is the better fit for long-lived history.
- The licence fit of `sequoia-openpgp` (LGPL-2+) with the dual MIT/Apache-2.0 licence of Stemma.

