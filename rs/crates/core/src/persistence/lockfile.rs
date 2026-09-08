//! `modules.lock` on disk (docs/modules-fanout-plan.md, track H5): where the lockfile and
//! the per-workspace module state live, atomic write, and the read path that tolerates a
//! missing file. The wire format itself is `avada_module_sdk::install::Lockfile`.
//! Owned by the H5 track in Wave 1.
