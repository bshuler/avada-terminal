//! Licensing (docs/modules-fanout-plan.md §2 "Free vs commercial", track G8): the
//! offline EdDSA JWT verifier, the RFC 7662 introspection client, the three install
//! paths, expiry/grace evaluation and the in-process stub issuer used by tests.
//!
//! Claims and the state machine are `avada_module_sdk::license`; this module owns the
//! crypto and the I/O. Scaffolded by the orchestrator; track G8 fills it.
