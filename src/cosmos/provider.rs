use super::client::CosmosClient;
use super::events::{
    event_is_type_channel, event_is_type_client, event_is_type_connection,
    ibc_event_try_from_abci_event,
};
use crate::core::error::Error;
use crate::core::primitives::{Chain, IbcProvider, UpdateType};
use core::time::Duration;
use futures::{
    stream::{self, select_all},
    Stream, StreamExt,
};
use ibc_proto::{
    google::protobuf::Any,
    ibc::core::{
        channel::v1::{
            QueryChannelResponse, QueryChannelsRequest, QueryChannelsResponse,
            QueryConnectionChannelsRequest, QueryNextSequenceReceiveResponse,
            QueryPacketAcknowledgementResponse, QueryPacketCommitmentResponse,
            QueryPacketReceiptResponse,
        },
        client::v1::{
            QueryClientStateResponse, QueryClientStatesRequest, QueryConsensusStateResponse,
        },
        connection::v1::{IdentifiedConnection, QueryConnectionResponse, QueryConnectionsRequest},
    },
    protobuf::Protobuf,
};
use ibc_relayer_types::{
    applications::transfer::PrefixedCoin,
    clients::ics07_tendermint::{
        client_state::{AllowUpdate, ClientState},
        consensus_state::ConsensusState,
        header::Header,
    },
    core::{
        ics02_client::{
            client_type::ClientType, events as ClientEvents, trust_threshold::TrustThreshold,
        },
        ics04_channel::channel::ChannelEnd,
        ics23_commitment::{commitment::CommitmentPrefix, specs::ProofSpecs},
        ics24_host::{
            identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId},
            path::ChannelEndsPath,
        },
    },
    events::IbcEvent,
    Height,
};

use std::pin::Pin;
use std::str::FromStr;
use tendermint::block::Height as TmHeight;
use tendermint::Time;
use tendermint_rpc::{
    endpoint::tx::Response,
    event::{Event, EventData},
    query::{EventType, Query},
    Client, Error as RpcError, Order, SubscriptionClient, WebSocketClient,
};
use tonic::{metadata::AsciiMetadataValue, transport::Channel};
pub enum FinalityEvent {
    Tendermint(Header),
}

pub struct TransactionId<Hash> {
    pub hash: Hash,
}

#[async_trait::async_trait]
impl<H> IbcProvider for CosmosClient<H>
where
    H: Clone + Send + Sync + 'static,
{
    type FinalityEvent = FinalityEvent;
    type TransactionId = TransactionId<tendermint::abci::transaction::Hash>;
    type Error = Error;

    async fn query_latest_ibc_events<C>(
        &mut self,
        finality_event: Self::FinalityEvent,
        counterparty: &C,
    ) -> Result<(Any, Vec<IbcEvent>, UpdateType), anyhow::Error>
    where
        C: Chain,
    {
        todo!()
    }

    async fn ibc_events(&self) -> Pin<Box<dyn Stream<Item = IbcEvent>>> {
        // Create websocket client
        let (ws_client, ws_driver) = WebSocketClient::new(self.websocket_url.clone())
            .await
            .map_err(|e| Error::from(format!("Web Socket Client Error {:?}", e)))
            .unwrap();
        tokio::spawn(ws_driver.run());
        let query_all = vec![
            Query::from(EventType::NewBlock),
            Query::eq("message.module", "ibc_client"),
            Query::eq("message.module", "ibc_connection"),
            Query::eq("message.module", "ibc_channel"),
        ];
        let mut subscriptions = vec![];
        for query in &query_all {
            let subscription = ws_client
                .subscribe(query.clone())
                .await
                .map_err(|e| Error::from(format!("Web Socket Client Error {:?}", e)))
                .unwrap();
            subscriptions.push(subscription);
        }

        // Collect IBC events from each RPC event
        let all_subs: Box<
            dyn Stream<Item = core::result::Result<Event, RpcError>> + Send + Sync + Unpin,
        > = Box::new(select_all(subscriptions));
        let chain_id = self.chain_id.clone();
        let events = all_subs
            .map(move |event| {
                let mut all_events: Vec<IbcEvent> = vec![];
                let Event {
                    data,
                    events,
                    query,
                } = event.unwrap();
                match data {
                    EventData::NewBlock { block, .. }
                        if query == Query::from(EventType::NewBlock).to_string() =>
                    {
                        let height = Height::new(
                            ChainId::chain_version(chain_id.to_string().as_str()),
                            u64::from(block.as_ref().ok_or("tx.height").unwrap().header.height),
                        )
                        .map_err(|e| Error::from(format!("Error {:?}", e)))
                        .unwrap();
                        all_events.push(ClientEvents::NewBlock::new(height).into());
                        // all_events.append(&mut extract_block_events(height, &events));
                    }
                    EventData::Tx { tx_result } => {
                        for abci_event in &tx_result.result.events {
                            if let Ok(ibc_event) = ibc_event_try_from_abci_event(abci_event) {
                                if query == Query::eq("message.module", "ibc_client").to_string()
                                    && event_is_type_client(&ibc_event)
                                {
                                    all_events.push(ibc_event);
                                } else if query
                                    == Query::eq("message.module", "ibc_connection").to_string()
                                    && event_is_type_connection(&ibc_event)
                                {
                                    all_events.push(ibc_event);
                                } else if query
                                    == Query::eq("message.module", "ibc_channel").to_string()
                                    && event_is_type_channel(&ibc_event)
                                {
                                    all_events.push(ibc_event);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                stream::iter(all_events)
            })
            .flatten()
            .boxed();
        events
    }

    async fn query_client_consensus(
        &self,
        at: Height,
        client_id: ClientId,
        consensus_height: Height,
    ) -> Result<QueryConsensusStateResponse, Self::Error> {
        todo!()
    }

    async fn query_client_state(
        &self,
        at: Height,
        client_id: ClientId,
    ) -> Result<QueryClientStateResponse, Self::Error> {
        todo!()
    }

    async fn query_connection_end(
        &self,
        at: Height,
        connection_id: ConnectionId,
    ) -> Result<QueryConnectionResponse, Self::Error> {
        use ibc_proto::ibc::core::connection::v1 as connection;
        use tonic::IntoRequest;

        let mut grpc_client =
            connection::query_client::QueryClient::connect(self.grpc_url.clone().to_string())
                .await
                .map_err(|e| Error::from(e.to_string()))?;

        let mut request = connection::QueryConnectionRequest {
            connection_id: connection_id.to_string(),
        }
        .into_request();

        let height = at.revision_height().to_string();
        let height_param = AsciiMetadataValue::try_from(height.as_str()).unwrap();

        request
            .metadata_mut()
            .insert("x-cosmos-block-height", height_param);

        let response = grpc_client
            .connection(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        Ok(response.into_inner())
    }

    async fn query_channel_end(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<QueryChannelResponse, Self::Error> {
        let res = self
            .query(ChannelEndsPath(port_id, channel_id), at, true)
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let channel_end =
            ChannelEnd::decode_vec(&res.value).map_err(|e| Error::from(e.to_string()))?;

        Ok(QueryChannelResponse {
            channel: Some(channel_end.into()),
            proof: vec![],
            proof_height: Some(at.into()),
        })
    }

    async fn query_proof(&self, at: Height, keys: Vec<Vec<u8>>) -> Result<Vec<u8>, Self::Error> {
        todo!()
    }

    async fn query_packet_commitment(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketCommitmentResponse, Self::Error> {
        todo!()
    }

    async fn query_packet_acknowledgement(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketAcknowledgementResponse, Self::Error> {
        todo!()
    }

    async fn query_next_sequence_recv(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
    ) -> Result<QueryNextSequenceReceiveResponse, Self::Error> {
        todo!()
    }

    async fn query_packet_receipt(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketReceiptResponse, Self::Error> {
        todo!()
    }

    async fn latest_height_and_timestamp(&self) -> Result<(Height, Time), Self::Error> {
        let response = self
            .rpc_client
            .status()
            .await
            .map_err(|e| Error::RpcError(format!("{:?}", e)))?;

        if response.sync_info.catching_up {
            return Err(Error::from(format!("Node is still syncing")));
        }

        let time = response.sync_info.latest_block_time;
        let height = Height::new(
            ChainId::chain_version(response.node_info.network.as_str()),
            u64::from(response.sync_info.latest_block_height),
        )
        .map_err(|e| Error::from(format!("Error {:?}", e)))?;

        Ok((height, time.into()))
    }

    async fn query_packet_commitments(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error> {
        todo!()
    }

    async fn query_packet_acknowledgements(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error> {
        todo!()
    }

    async fn query_unreceived_packets(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error> {
        todo!()
    }

    async fn query_unreceived_acknowledgements(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error> {
        todo!()
    }

    fn channel_whitelist(&self) -> Vec<(ChannelId, PortId)> {
        todo!()
    }

    async fn query_connection_channels(
        &self,
        at: Height,
        connection_id: &ConnectionId,
    ) -> Result<QueryChannelsResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?;

        let request = tonic::Request::new(QueryConnectionChannelsRequest {
            connection: connection_id.to_string(),
            pagination: None,
        });

        let response = grpc_client
            .connection_channels(request)
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?
            .into_inner();

        let channels = QueryChannelsResponse {
            channels: response.channels,
            pagination: response.pagination,
            height: response.height,
        };

        Ok(channels)
    }

    async fn query_send_packets(
        &self,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u8>, Self::Error> {
        todo!()
    }

    async fn query_recv_packets(
        &self,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u8>, Self::Error> {
        todo!()
    }

    fn expected_block_time(&self) -> Duration {
        todo!()
    }

    async fn query_client_update_time_and_height(
        &self,
        client_id: ClientId,
        client_height: Height,
    ) -> Result<(Height, Time), Self::Error> {
        todo!()
    }

    async fn query_host_consensus_state_proof(
        &self,
        height: Height,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        todo!()
    }

    async fn query_ibc_balance(&self) -> Result<Vec<PrefixedCoin>, Self::Error> {
        todo!()
    }

    fn connection_prefix(&self) -> CommitmentPrefix {
        todo!()
    }

    fn client_id(&self) -> ClientId {
        self.client_id()
    }

    fn client_type(&self) -> ClientType {
        todo!()
    }

    fn connection_id(&self) -> ConnectionId {
        todo!()
    }

    async fn query_timestamp_at(&self, block_number: u64) -> Result<Time, Self::Error> {
        let height = TmHeight::try_from(block_number)
            .map_err(|e| Error::from(format!("Invalid block number: {}", e)))?;
        let response = self
            .rpc_client
            .block(height)
            .await
            .map_err(|e| Error::RpcError(e.to_string()))?;
        let time = response.block.header.time;
        Ok(time)
    }

    async fn query_clients(&self) -> Result<Vec<ClientId>, Self::Error> {
        let request = tonic::Request::new(QueryClientStatesRequest { pagination: None });
        let grpc_client = ibc_proto::ibc::core::client::v1::query_client::QueryClient::connect(
            self.grpc_url.clone().to_string(),
        )
        .await
        .map_err(|e| Error::RpcError(format!("{:?}", e)))?;
        let response = grpc_client
            .clone()
            .client_states(request)
            .await
            .map_err(|e| {
                Error::from(format!(
                    "Failed to query client states from grpc client: {:?}",
                    e
                ))
            })?
            .into_inner();

        // Deserialize into domain type
        let mut clients: Vec<ClientId> = response
            .client_states
            .into_iter()
            .filter_map(|cs| {
                let id = ClientId::from_str(&cs.client_id).ok()?;
                Some(id)
            })
            .collect();
        Ok(clients)
    }

    async fn query_channels(&self) -> Result<Vec<(ChannelId, PortId)>, Self::Error> {
        let request = tonic::Request::new(QueryChannelsRequest { pagination: None });
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?;
        let response = grpc_client
            .channels(request)
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?
            .into_inner()
            .channels
            .into_iter()
            .filter_map(|c| {
                let id = ChannelId::from_str(&c.channel_id).ok()?;
                let port_id = PortId::from_str(&c.port_id).ok()?;
                Some((id, port_id))
            })
            .collect::<Vec<_>>();
        Ok(response)
    }

    async fn query_connection_using_client(
        &self,
        height: u32,
        client_id: String,
    ) -> Result<Vec<IdentifiedConnection>, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::connection::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?;

        let request = tonic::Request::new(QueryConnectionsRequest { pagination: None });

        let response = grpc_client
            .connections(request)
            .await
            .map_err(|e| Error::from(format!("{:?}", e)))?
            .into_inner();

        let connections = response
            .connections
            .into_iter()
            .filter_map(|co| {
                IdentifiedConnection::try_from(co.clone())
                    .map_err(|e| Error::from(format!("Failed to convert connection end: {:?}", e)))
                    .ok()
            })
            .collect();
        Ok(connections)
    }

    fn is_update_required(
        &self,
        latest_height: u64,
        latest_client_height_on_counterparty: u64,
    ) -> bool {
        todo!()
    }

    async fn initialize_client_state(&self) -> Result<(ClientState, ConsensusState), Self::Error> {
        let latest_height_timestamp = self.latest_height_and_timestamp().await.unwrap();
        let client_state = ClientState::new(
            self.chain_id.clone(),
            TrustThreshold::default(),
            Duration::new(64000, 0),
            Duration::new(128000, 0),
            Duration::new(15, 0),
            latest_height_timestamp.0,
            ProofSpecs::default(),
            vec!["upgrade".to_string(), "upgradedIBCState".to_string()],
            AllowUpdate {
                after_expiry: true,
                after_misbehaviour: true,
            },
        )
        .map_err(|e| Error::from(format!("Invalid client state {}", e)))?;
        let light_block = self
            .light_client
            .verify(
                latest_height_timestamp.0,
                latest_height_timestamp.0,
                &client_state,
            )
            .map_err(|e| Error::from(format!("Invalid light block {}", e)))?;
        let consensus_state = ConsensusState::from(light_block.clone().signed_header.header);
        Ok((client_state, consensus_state))
    }

    async fn query_client_id_from_tx_hash(
        &self,
        tx_id: Self::TransactionId,
    ) -> Result<ClientId, Self::Error> {
        const WAIT_BACKOFF: Duration = Duration::from_millis(300);
        const TIME_OUT: Duration = Duration::from_millis(30000);
        let start_time = std::time::Instant::now();

        let response: Response = loop {
            let response = self
                .rpc_client
                .tx_search(
                    Query::eq("tx.hash", tx_id.hash.to_string()),
                    false,
                    1,
                    1, // get only the first Tx matching the query
                    Order::Ascending,
                )
                .await
                .map_err(|e| Error::from(format!("Failed to query tx hash: {}", e)))?;
            match response.txs.into_iter().next() {
                None => {
                    let elapsed = start_time.elapsed();
                    if &elapsed > &TIME_OUT {
                        return Err(Error::from(format!(
                            "Timeout waiting for tx {:?} to be included in a block",
                            tx_id.hash
                        )));
                    } else {
                        std::thread::sleep(WAIT_BACKOFF);
                    }
                }
                Some(resp) => break resp,
            }
        };

        let deliver_tx_result = response.tx_result;
        if deliver_tx_result.code.is_err() {
            Err(Error::from(format!(
                "Transaction failed with code {:?} and log {:?}",
                deliver_tx_result.code, deliver_tx_result.log
            )))
        } else {
            let result = deliver_tx_result
                .events
                .iter()
                .flat_map(|e| ibc_event_try_from_abci_event(e).ok().into_iter())
                .filter(|e| matches!(e, IbcEvent::CreateClient(_)))
                .collect::<Vec<_>>();
            log::info!(target: "demo-relayer", "Result: {:?}", result);
            if result.clone().len() != 1 {
                Err(Error::from(format!(
                    "Expected exactly one CreateClient event, found {}",
                    result.len()
                )))
            } else {
                Ok(match result[0] {
                    IbcEvent::CreateClient(ref e) => e.client_id().clone(),
                    _ => unreachable!(),
                })
            }
        }
    }
}
