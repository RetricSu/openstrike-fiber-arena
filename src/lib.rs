pub mod client;
pub mod fiber;
pub mod net;
#[cfg(feature = "openstrike")]
pub mod openstrike;
pub mod protocol;
pub mod security;
pub mod settlement;
pub mod sim;

pub const PROTOCOL_ID: u64 = 0x4f53_4649_4245_5231;
pub const TICK_HZ: u64 = 64;
