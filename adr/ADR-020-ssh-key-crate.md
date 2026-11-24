# ADR-020: The `ssh-key` crate for SSHSIG signatures

**Status:** Accepted by the human maintainer on 2026-11-14 (STEMMA-234), including the
`BSD-3-Clause` exceptions. The dependency is a level 3 change (ADR-004: the level 3 row names
signatures and keys, and rule 7 names cryptography crates). The scheme itself, SSHSIG with
Ed25519, is decided in STEMMA-224 and recorded in ADR-003. A second key type, the Ed25519 key on a
hardware token, was decided for phase 5 on 2026-11-24 (STEMMA-275, see "A key on a hardware
token"); it is not implemented yet.

## Context

A proof signs a log entry or an approval (ADR-003, ADR-004). The signature must be verifiable
by anyone who holds the repository, without a server, and by a tool that is already on the
machine of a developer. SSHSIG is the format of `ssh-keygen -Y sign` and `ssh-keygen -Y verify`
(`PROTOCOL.sshsig` of OpenSSH). It wraps a signature in a namespace and a hash algorithm, so a
signature made for one purpose does not verify for another.

Signing and verifying differ in what they need:

- **Signing** uses a key that the actor holds, often in an agent or on a token. Stemma never
  reads the private key. The trait `Signer` hides the process: its production implementation
  calls `ssh-keygen -Y sign`, and a test uses a fake `Signer`.
- **Verifying** will happen in `stemma-core`, in the authorizer and in the check of the log, on
  every machine, in many places per command, and in tests without a key on disk. A child process for
  each verification is slow, needs `ssh-keygen` of a recent version, needs an `allowed_signers`
  file on disk, and is absent on a plain Windows installation.

Candidates, measured on 2026-11-13 in a scratch crate against `deny.toml` (toolchain 1.99.1):

- **`ssh-key` 0.6.7** (RustCrypto), `default-features = false`, features `alloc` and `ed25519`.
  It parses and encodes OpenSSH public keys and implements `SshSig` (`PublicKey::verify`,
  `PrivateKey::sign`) for Ed25519. License `Apache-2.0 OR MIT`, MSRV 1.65. The tree has 29 crates
  (33 entries in the lockfile with the build dependencies `syn`, `quote`, and `proc-macro2`);
  the most important are `ed25519-dalek` 2.2.0, `curve25519-dalek` 4.1.3, `sha2` 0.10.9,
  `signature` 2.2.0, `ssh-encoding` 0.2.0, `zeroize`, and `subtle`. `cargo audit` reports no
  advisory (1509 advisories, 33 crates). `cargo deny check` reports no advisory and no source
  problem. It **fails the license check** on three crates, `curve25519-dalek`, `ed25519-dalek`,
  and `subtle`, which carry `BSD-3-Clause` only. The version 0.7.0 is a release candidate
  (`0.7.0-rc.12`) and is not used. With these two features, a signature made by
  `ssh-keygen -Y sign` (OpenSSH 10.5, SHA-512) parses and verifies in process, and a wrong
  namespace or message fails. The crate also signs and verifies on its own; a key from a fixed
  seed (`Ed25519Keypair::from_seed`) needs no random number generator.
- **`ed25519-dalek` and `sha2` directly**, with the SSHSIG framing and the OpenSSH key format
  written in `stemma-core`. The tree is smaller by `ssh-key`, `ssh-encoding`, `ssh-cipher`, and
  `pem-rfc7468`, but the licenses are the same, and the project would own a binary format whose
  mistakes are mistakes of a signature check. This is the code that should be written least of all
  by the project itself.
- **`ssh-keygen -Y verify` as a child process.** No dependency, but it has the three costs above,
  and the result depends on the version of OpenSSH on the machine.
- **Another scheme** (minisign, age, GPG). Not considered: the scheme is decided in STEMMA-224.

## Decision

- **`ssh-key` 0.6 with `default-features = false` and the features `alloc` and `ed25519`.**
  Only `stemma-core` depends on it. The `Signer` stays a trait of `stemma-core` and is not part
  of `ssh-key`; the implementation through `ssh-keygen -Y sign` is the only code that starts a
  process.
- **Verification in process, signing through `ssh-keygen`.** `stemma-core` never holds a private
  key. The crate is used for the public key, the signature, and the check.
- **Ed25519 only.** A key of another type, and a hash algorithm other than SHA-512 in the
  signature, is refused. The code does not enable `ecdsa`, `rsa`, `dsa`, `p256`, `p384`, or
  `p521`.
- **`BSD-3-Clause` is allowed for `curve25519-dalek`, `ed25519-dalek`, and `subtle` only**,
  through `[[licenses.exceptions]]` entries in `deny.toml`, as ADR-016 did for `Zlib`. The
  license is permissive and compatible with `MIT OR Apache-2.0`. It requires the copyright
  notice in a binary distribution; this is a task of the release (see the open points).
- **The version is a caret range, the lockfile is the pin.** A change of the minor version
  (0.6 to 0.7) needs a new decision here, because it changes the verification code.

### A key on a hardware token

Decided for phase 5 (STEMMA-275); not implemented yet. The change is level 3, as is the change of
the register format that names the key type.

- Beside Ed25519 the register accepts the key type `sk-ssh-ed25519@openssh.com`. "Ed25519 only"
  above becomes "Ed25519 and `sk-ssh-ed25519@openssh.com` only".
- The verifier in the core demands the flag "user present" on every signature of such a key. It
  knows no exception by type of proof: the signed challenge of a hub and the anchor need the flag
  as well (ADR-024, ADR-026). A key that needs no touch (`no-touch-required`) is refused. A PIN
  (user verification) is not demanded.
- Signing stays `ssh-keygen -Y sign` behind the trait `Signer`. A hub checks with the same
  function of the core and builds nothing of its own.
- The key type is voluntary. No rule asks for it, and the policy format gets no new field; the
  register shows every reader which key an entry holds (ADR-003).
- A token is touched once per signature. A push of a human with a token key costs three touches
  (the challenge, the ref update, the anchor), and a fetch of a private repository costs one. Who
  pushes often does it as an agent with a file key; the human with the token approves and makes
  the acts of level 3.

Read from the source of `ssh-key` 0.6.7 and not run: the crate verifies signatures of this key
type and carries the flags and the counter in the signed data. Not verified: that
`ssh-keygen -Y sign` signs with a hardware token in the way the `Signer` calls it.

Rejected: a second key for transport only (a new attribute in the register format and a
distinction in the authorizer, and a stolen transport key reads private repositories and sends
anchors), and an anchor that travels inside the signed ref update, to save a touch (it would
reopen the decisions on the anchor and on the ref update).

## Open Points

- [x] Acceptance of this ADR by the human maintainer, on 2026-11-14.
- [ ] Notice of the third-party licenses in the distribution of a release (the clause of
      `BSD-3-Clause`). `just release-check` does not check it yet.
- [ ] Owners of `ssh-key`, `ed25519-dalek`, and `curve25519-dalek` on crates.io were not looked
      up, and the Windows tree was not built. Both belong to the level 3 review of the change
      that adds the dependency.
- [ ] Migration to `ssh-key` 0.7 when it is released.
- [ ] The key type `sk-ssh-ed25519@openssh.com`: run the verification with `ssh-key` and try
      `ssh-keygen -Y sign` with a hardware token, before the slice builds the check. How
      `register add-key` and `enforce --owner-key` take such a key belongs to the command surface.

## Consequences

- Verification works without a process, without a file on disk, and on every platform that
  builds `stemma-core`. The tests of the logic need no key on disk: they derive an Ed25519 key
  from a fixed seed in memory. Only the tests of `SshKeygen` create a key in a temporary
  directory. They are skipped with a notice when `ssh-keygen` is missing, and
  `STEMMA_REQUIRE_SSH_KEYGEN=1` turns the skip into a failure.
- The lockfile grows by about 30 crates, all of them from crates.io and all of them from the
  RustCrypto project and the dalek project. `cargo audit` and `cargo deny` run with the change
  (`just deps`).
- A flaw in `ssh-key` or in `ed25519-dalek` is a flaw in the verification of every approval and
  of every owner override. The two defences in the code of Stemma are the domain separation
  (the namespace and the type and version of the payload are part of the signed bytes) and the
  tests of the refusals: a wrong namespace, type, version, key, and hash algorithm fail.
