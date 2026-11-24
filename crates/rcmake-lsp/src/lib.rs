//! Language server for CMake listfiles.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod analysis;
mod catalog;
mod expression;
mod features;
mod json;
mod lexical;
mod position;
mod roles;
pub mod rpc;
pub mod server;
mod uri;
mod workspace;

pub use position::{Position, PositionEncoding};
pub use rpc::{RequestId, RpcMessage};
pub use server::{SemanticContext, SemanticLocation, SemanticProvider, SemanticUpdate, Server};
