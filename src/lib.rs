pub mod client;
#[cfg(feature = "openstrike")]
pub mod devmap;
pub mod fiber;
#[cfg(feature = "desktop")]
pub mod neon;
pub mod net;
#[cfg(feature = "openstrike")]
pub mod openstrike;
pub mod protocol;
pub mod security;
pub mod settlement;
pub mod sim;

// "OSFIBER2": the hold-invoice and token-bound Fiber identity wire protocol.
pub const PROTOCOL_ID: u64 = 0x4f53_4649_4245_5232;
pub const TICK_HZ: u64 = 64;
