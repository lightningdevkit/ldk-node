//! Client-side library to interact with LDK Server.

#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![deny(missing_docs)]

/// Implements a ldk-ldk-server-client ([`client::LdkServerClient`]) to access a hosted instance of LDK Server.
pub mod client;

/// Implements the error type ([`error::LdkServerError`]) returned on interacting with [`client::LdkServerClient`]
pub mod error;

/// Request/Response structs required for interacting with the ldk-ldk-server-client.
pub use ldk_server_protos;
