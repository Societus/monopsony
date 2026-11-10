//! Generated protobuf definitions for the Monopsony control plane.

// Sprint 34 P2: HeartbeatRequest gained `monopsony_version`, pushing the
// generated `client_message::Msg::Heartbeat` variant past clippy's
// large_enum_variant heuristic (320 vs 104 bytes vs TamperAlert). The
// codegen does not box message variants, so the lint is scoped to the
// generated module only — no hand-written code is affected.
#[allow(clippy::large_enum_variant)]
pub mod monopsony {
    tonic::include_proto!("monopsony");
}
