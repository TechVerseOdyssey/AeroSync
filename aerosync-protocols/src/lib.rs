pub mod http;
pub mod quic;
pub mod traits;

pub use http::HttpTransfer;
pub use quic::QuicTransfer;
pub use traits::{TransferProtocol, ProtocolConfig};