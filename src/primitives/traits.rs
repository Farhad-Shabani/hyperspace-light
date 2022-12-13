use crate::core::packets::types::PacketInfo;
use crate::cosmos::events::IbcEventWithHeight;
use futures::stream::Stream;
use ibc_proto::{
    google::protobuf::Any,
    ibc::core::{
        channel::v1::{
            QueryChannelResponse, QueryChannelsResponse, QueryNextSequenceReceiveResponse,
            QueryPacketAcknowledgementResponse, QueryPacketCommitmentResponse,
            QueryPacketReceiptResponse,
        },
        client::v1::{QueryClientStateResponse, QueryConsensusStateResponse},
        connection::v1::{IdentifiedConnection, QueryConnectionResponse},
    },
};
use ibc_relayer_types::applications::transfer::msgs::transfer::MsgTransfer;
use ibc_relayer_types::{
    applications::transfer::PrefixedCoin,
    clients::ics07_tendermint::{client_state::ClientState, consensus_state::ConsensusState},
    core::{
        ics02_client::client_type::ClientType,
        ics23_commitment::commitment::CommitmentPrefix,
        ics24_host::identifier::{ChannelId, ClientId, ConnectionId, PortId},
    },
    timestamp::Timestamp,
    Height,
};
use std::{pin::Pin, time::Duration};
use tendermint::Time;

#[async_trait::async_trait]
pub trait Chain: IbcProvider + KeyProvider + Send + Sync {
    /// Name of this chain, used in logs.
    fn name(&self) -> &str;

    /// Should return a numerical value for the max weight of transactions allowed in a block.
    fn block_max_weight(&self) -> u64;

    /// Should return an estimate of the weight of a batch of messages.
    async fn estimate_weight(&self, msg: Vec<Any>) -> Result<u64, Self::Error>;

    /// Return a stream that yields when new [`IbcEvents`] are ready to be queried.
    async fn finality_notifications(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Self::FinalityEvent> + Send + Sync>>;

    /// This should be used to submit new messages [`Vec<Any>`] from a counterparty chain to this chain.
    /// Should return the transaction id
    async fn submit(&self, messages: Vec<Any>) -> Result<Self::TransactionId, Self::Error>;
}

/// Provides an interface for accessing new events and Ibc data on the chain which must be
/// relayed to the counterparty chain.
#[async_trait::async_trait]
pub trait IbcProvider {
    /// Finality event type, passed on to [`Chain::query_latest_ibc_events`]
    type FinalityEvent: Clone + Send + Sync + std::fmt::Debug;

    /// A representation of the transaction id for the chain
    type TransactionId: Clone + Send + Sync + std::fmt::Debug;

    /// Error type, just needs to implement standard error trait.
    type Error: std::error::Error + From<String> + Send + Sync + 'static;

    /// Query the latest ibc events finalized by the recent finality event. Use the counterparty
    /// [`Chain`] to query the on-chain [`ClientState`] so you can scan for new events in between
    /// the client state and the new finality event.
    async fn query_latest_ibc_events<T>(
        &mut self,
        finality_event: Self::FinalityEvent,
        counterparty: &T,
    ) -> Result<(Any, Vec<IbcEventWithHeight>, UpdateType), anyhow::Error>
    where
        T: Chain;

    /// Return a stream that yields when new [`IbcEvents`] are parsed from a finality notification
    async fn ibc_events(&self) -> Pin<Box<dyn Stream<Item = IbcEventWithHeight>>>;

    /// Query client consensus state with proof
    /// return the consensus height for the client along with the response
    async fn query_client_consensus(
        &self,
        at: Height,
        client_id: ClientId,
        consensus_height: Height,
    ) -> Result<QueryConsensusStateResponse, Self::Error>;

    /// Query client state with proof
    async fn query_client_state(
        &self,
        at: Height,
        client_id: ClientId,
    ) -> Result<QueryClientStateResponse, Self::Error>;

    /// Query connection end with proof
    async fn query_connection_end(
        &self,
        at: Height,
        connection_id: ConnectionId,
    ) -> Result<QueryConnectionResponse, Self::Error>;

    /// Query channel end with proof
    async fn query_channel_end(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<QueryChannelResponse, Self::Error>;

    /// Query proof for provided key path
    async fn query_proof(&self, at: Height, keys: Vec<Vec<u8>>) -> Result<Vec<u8>, Self::Error>;

    /// Query packet commitment with proof
    async fn query_packet_commitment(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketCommitmentResponse, Self::Error>;

    /// Query packet acknowledgement commitment with proof
    async fn query_packet_acknowledgement(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketAcknowledgementResponse, Self::Error>;

    /// Query next sequence to be received
    async fn query_next_sequence_recv(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
    ) -> Result<QueryNextSequenceReceiveResponse, Self::Error>;

    /// Query packet receipt
    async fn query_packet_receipt(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketReceiptResponse, Self::Error>;

    /// Return latest finalized height and timestamp
    async fn latest_height_and_timestamp(&self) -> Result<(Height, Timestamp), Self::Error>;

    async fn query_packet_commitments(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error>;

    async fn query_packet_acknowledgements(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error>;

    /// Given a list of counterparty packet commitments, the querier checks if the packet
    /// has already been received by checking if a receipt exists on this
    /// chain for the packet sequence. All packets that haven't been received yet
    /// are returned in the response
    /// Usage: To use this method correctly, first query all packet commitments on
    /// the sending chain using the query_packet_commitments method.
    /// and send the request to this Query/UnreceivedPackets on the **receiving**
    /// chain. This method should then return the list of packet sequences that
    /// are yet to be received on the receiving chain.
    /// NOTE: WORKS ONLY FOR UNORDERED CHANNELS
    async fn query_unreceived_packets(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error>;

    /// Given a list of packet acknowledgements sequences from the sink chain
    /// return a list of acknowledgement sequences that have not been received on the source chain
    async fn query_unreceived_acknowledgements(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error>;

    /// Channel whitelist
    fn channel_whitelist(&self) -> Vec<(ChannelId, PortId)>;

    /// Query all channels for a connection
    async fn query_connection_channels(
        &self,
        at: Height,
        connection_id: &ConnectionId,
    ) -> Result<QueryChannelsResponse, Self::Error>;

    /// Query send packets
    /// This represents packets that for which the `SendPacket` event was emitted
    async fn query_send_packets(
        &self,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<PacketInfo>, Self::Error>;

    /// Query received packets with their acknowledgement
    /// This represents packets for which the `ReceivePacket` and `WriteAcknowledgement` events were
    /// emitted.
    async fn query_recv_packets(
        &self,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<PacketInfo>, Self::Error>;

    /// Return the expected block time for this chain
    fn expected_block_time(&self) -> Duration;

    /// Query the time and height at which this client was updated on this chain for the given
    /// client height
    async fn query_client_update_time_and_height(
        &self,
        client_id: ClientId,
        client_height: Height,
    ) -> Result<(Height, Time), Self::Error>;

    /// Return a proof for the host consensus state at the given height to be included in the
    /// consensus state proof.
    async fn query_host_consensus_state_proof(
        &self,
        height: Height,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Should return the list of ibc denoms available to this account to spend.
    async fn query_ibc_balance(&self) -> Result<Vec<PrefixedCoin>, Self::Error>;

    /// Return the chain connection prefix
    fn connection_prefix(&self) -> CommitmentPrefix;

    /// Return the host chain's light client id on counterparty chain
    fn client_id(&self) -> ClientId;

    /// Return the connection id on this chain
    fn connection_id(&self) -> ConnectionId;

    /// Returns the client type of this chain.
    fn client_type(&self) -> ClientType;

    /// Should return timestamp in nanoseconds of chain at a given block height
    async fn query_timestamp_at(&self, block_number: u64) -> Result<u64, Self::Error>;

    /// Should return a list of all clients on the chain
    async fn query_clients(&self) -> Result<Vec<ClientId>, Self::Error>;

    /// Should return a list of all clients on the chain
    async fn query_channels(&self) -> Result<Vec<(ChannelId, PortId)>, Self::Error>;

    /// Query all connection states for associated client
    async fn query_connection_using_client(
        &self,
        height: u32,
        client_id: String,
    ) -> Result<Vec<IdentifiedConnection>, Self::Error>;

    /// Returns a boolean value that determines if the light client should receive a mandatory
    /// update
    fn is_update_required(
        &self,
        latest_height: u64,
        latest_client_height_on_counterparty: u64,
    ) -> bool;

    /// This should return a subjectively chosen client and consensus state for this chain.
    async fn initialize_client_state(&self) -> Result<(ClientState, ConsensusState), Self::Error>;

    /// Should find client id that was created in this transaction
    async fn query_client_id_from_tx_hash(
        &self,
        tx_id: Self::TransactionId,
    ) -> Result<ClientId, Self::Error>;
}

#[async_trait::async_trait]
pub trait TestProvider: Chain + Clone + 'static {
    /// Initiate an ibc transfer on chain.
    async fn send_transfer(&self, params: MsgTransfer) -> Result<(), Self::Error>;

    /// Send a packet on an ordered channel
    async fn send_ordered_packet(
        &self,
        channel_id: ChannelId,
        timeout: u64,
    ) -> Result<(), Self::Error>;

    /// Returns a stream that yields chain Block number
    async fn subscribe_blocks(&self) -> Pin<Box<dyn Stream<Item = u64> + Send + Sync>>;

    /// Set the channel whitelist for the relayer task.
    fn set_channel_whitelist(&mut self, channel_whitelist: Vec<(ChannelId, PortId)>);
}

/// Provides an interface for managing key management for signing.
pub trait KeyProvider {
    /// Should return the relayer's account id on the host chain as a string in the expected format
    /// Could be a hexadecimal, bech32 or ss58 string, any format the chain supports
    fn account_id(&self) -> ibc_relayer_types::signer::Signer;
}

pub enum UpdateMessage {
    Single(Any),
    Batch(Vec<Any>),
}

pub enum UpdateType {
    // contains an authority set change.
    Mandatory,
    // doesn't contain an authority set change
    Optional,
}

impl UpdateType {
    pub fn is_optional(&self) -> bool {
        match self {
            UpdateType::Mandatory => false,
            UpdateType::Optional => true,
        }
    }
}
