//! Client-side library to interact with LDK Node Server.

#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![deny(missing_docs)]

/// Implements a client ([`client::LdkNodeServerClient`]) to access a hosted instance of LDK Node Server.
pub mod client;

/// Implements the error type ([`error::LdkNodeServerError`]) returned on interacting with [`client::LdkNodeServerClient`]
pub mod error;

/// Request/Response structs required for interacting with the client.
pub use protos;
