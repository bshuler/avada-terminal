//! Rights service (docs/modules-fanout-plan.md, track H2): resolves what a module may
//! do from its signed install record, the user's `never|always|workspace|ask` values
//! and the workspace override column. NOT `permissions/`, which probes the OS.
