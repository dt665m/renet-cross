# Changelog

## 0.6.1

- Add opt-in, bounded WebRTC packet fingerprints and local-discard tracing.
- Pin the reviewed SCTP correction for repository development builds and document
  the workspace-root override required by applications while upstream PR #57 is
  pending. The crates.io package does not itself distribute the unpublished fix.
- Document independent receive polling and Renet send cadence.

No public API or Renet behavior changes.
