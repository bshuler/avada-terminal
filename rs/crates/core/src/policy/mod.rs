//! Policy traits with open-source implementations (docs/modules-fanout-plan.md §2
//! "Free vs commercial", track G7): what the commercial crate swaps at build time.
//! First occupant: the artifact notarization verifier (macOS codesign/spctl + Team ID,
//! Windows Authenticode, Linux minisign) behind one trait, with the free build's
//! "source-compiled only" policy as the default. Scaffolded by the orchestrator.
