use ibc_proto::ibc::core::client::v1::Height;
use serde::{Deserialize, Serialize};

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PacketInfo {
    /// Height at which packet event was emitted height
    pub height: u64,
    /// Packet sequence
    pub sequence: u64,
    /// Source port
    pub source_port: String,
    /// Source channel
    pub source_channel: String,
    /// Destination port
    pub destination_port: String,
    /// Destination channel
    pub destination_channel: String,
    /// Channel order
    pub channel_order: String,
    /// Opaque packet data
    pub data: Vec<u8>,
    /// Timeout height
    pub timeout_height: Height,
    /// Timeout timestamp
    pub timeout_timestamp: u64,
    /// Packet acknowledgement
    pub ack: Option<Vec<u8>>,
}
