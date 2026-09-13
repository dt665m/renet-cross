# Changelog

## 0.7.0

- Add authenticated session issuance with bounded credentials, grants, expiry,
  replay protection and endpoint binding for UDP and WebRTC.
- Add bounded bootstrap request bodies, session capacity and redacted errors.
- Add configurable connection settings and secure connection helpers. Public
  connection option structs and authentication errors have new fields/variants;
  update exhaustive constructors and matches when migrating from 0.6.
- Add per-peer egress budgets and control reserves with separate native wire-byte
  and WebRTC logical-byte accounting. Retain stock Renet message delivery,
  fragmentation and retransmission.
- Preserve peer identity and queued receive events across transport lifecycle
  changes, and cover handshake confirmation recovery under continuous payloads.
- Require the documented application-root `renetcode` fix override while its
  handshake correction is unpublished upstream. The crates.io package uses
  registry dependencies and cannot apply this override for its consumers.

## 0.6.1

- Add opt-in, bounded WebRTC packet fingerprints and local-discard tracing.
- Pin the reviewed SCTP correction for repository development builds and document
  the workspace-root override required by applications while upstream PR #57 is
  pending. The crates.io package does not itself distribute the unpublished fix.
- Document independent receive polling and Renet send cadence.

No public API or Renet behavior changes.
