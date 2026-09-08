# renet-cross-core

Renet 2.0.0 protocol core maintained for `renet-cross`. Derived from Lucas Poffo's
MIT/Apache-2.0 licensed Renet; see PATCHES.md for provenance and changes.

This fork fixes ACK feedback and statistics window accounting without changing
the Renet 2.0 wire format. Use `renet_cross::renet` for the core types consumed by
renet-cross 0.7. Upstream `renet` types are distinct Rust types and cannot be mixed
with this fork in the same connection.
