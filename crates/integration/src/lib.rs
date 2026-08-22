//! Cross-subsystem integration tests for the AI kernel.
//!
//! This crate deliberately contains no code. Its whole purpose is its `tests/` directory and
//! its dev-dependencies: the assertions there are about what happens when the store, the
//! transcript, the memory, the scheduler, the policy engine and the tool registry are all in
//! one kernel at once, and no single subsystem's suite can host them without that crate
//! taking a dependency on its siblings — which is exactly the coupling the architecture
//! avoids.
//!
//! Each subsystem still owns its own tests. What lives here is only what is true of the
//! *seams*: that a kernel holding every durable subsystem starts and stops, that a principal
//! means the same thing on both sides of a boundary, and that shutdown reaches work in
//! flight against a database three subsystems share.
