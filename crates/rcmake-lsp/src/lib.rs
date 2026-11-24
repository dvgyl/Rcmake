//! Language server for CMake listfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod analysis;
mod catalog;
mod features;
mod json;
mod position;
pub mod rpc;
pub mod server;
mod uri;

pub use position::{Position, PositionEncoding};
pub use rpc::{RequestId, RpcMessage};
pub use server::{SemanticContext, SemanticLocation, SemanticProvider, SemanticUpdate, Server};
