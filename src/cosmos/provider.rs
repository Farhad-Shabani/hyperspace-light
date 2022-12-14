use super::client::CosmosClient;
use super::events::{
    event_is_type_channel, event_is_type_client, event_is_type_connection,
    ibc_event_try_from_abci_event, IbcEventWithHeight,
};
use super::utils::incerement_proof_height;
use crate::core::error::Error;
use crate::core::packets::types::PacketInfo;
use crate::primitives::traits::{Chain, IbcProvider, UpdateType};
use core::time::Duration;
use futures::{
    stream::{self, select_all},
    Stream, StreamExt,
};
use ibc_proto::cosmos::bank::v1beta1::QueryBalanceRequest;
use ibc_proto::ibc::core::channel::v1::{
    QueryChannelRequest, QueryNextSequenceReceiveRequest, QueryPacketAcknowledgementRequest,
    QueryPacketAcknowledgementsRequest, QueryPacketCommitmentRequest,
    QueryPacketCommitmentsRequest, QueryPacketReceiptRequest, QueryUnreceivedAcksRequest,
    QueryUnreceivedPacketsRequest,
};
use ibc_proto::ibc::core::client::v1::QueryConsensusStateRequest;
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
            QueryClientStateRequest, QueryClientStateResponse, QueryClientStatesRequest,
            QueryConsensusStateResponse,
        },
        connection::v1::{
            IdentifiedConnection, QueryConnectionRequest, QueryConnectionResponse,
            QueryConnectionsRequest,
        },
    },
    protobuf::Protobuf,
};
use ibc_relayer_types::{
    applications::transfer::{Amount, PrefixedCoin, PrefixedDenom},
    clients::ics07_tendermint::{
        client_state::{AllowUpdate, ClientState},
        consensus_state::ConsensusState,
    },
    core::{
        ics02_client::{
            client_type::ClientType, events as ClientEvents, msgs::update_client::MsgUpdateClient,
            trust_threshold::TrustThreshold,
        },
        ics04_channel::packet::Sequence,
        ics23_commitment::{commitment::CommitmentPrefix, specs::ProofSpecs},
        ics24_host::{
            identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId},
            path::{
                AcksPath, ChannelEndsPath, ClientConsensusStatePath, ClientStatePath,
                CommitmentsPath, ConnectionsPath, Path, ReceiptsPath, SeqRecvsPath,
            },
        },
    },
    events::IbcEvent,
    timestamp::Timestamp,
    tx_msg::Msg,
    Height,
};
use std::pin::Pin;
use std::str::FromStr;
use tendermint::{block::Height as TmHeight, Time};
use tendermint_rpc::{
    endpoint::tx::Response,
    event::{Event, EventData},
    query::{EventType, Query},
    Client, Error as RpcError, Order, SubscriptionClient, WebSocketClient,
};
use tonic::IntoRequest;

#[derive(Clone, Debug)]
pub enum FinalityEvent {
    Tendermint(TmHeight),
}

#[derive(Clone, Debug)]
pub struct TransactionId<Hash> {
    pub hash: Hash,
}

#[async_trait::async_trait]
impl<H> IbcProvider for CosmosClient<H>
where
    H: Clone + Send + Sync + 'static,
{
    type FinalityEvent = FinalityEvent;
    type TransactionId = TransactionId<tendermint::Hash>;
    type Error = Error;

    async fn query_latest_ibc_events<C>(
        &mut self,
        _finality_event: Self::FinalityEvent,
        counterparty: &C,
    ) -> Result<(Any, Vec<IbcEventWithHeight>, UpdateType), anyhow::Error>
    where
        C: Chain,
    {
        let client_id = counterparty.client_id();
        let latest_cp_height = counterparty.latest_height_and_timestamp().await?.0;
        let latest_cp_client_state = counterparty
            .query_client_state(latest_cp_height, client_id)
            .await?;
        let client_state_response = latest_cp_client_state
            .client_state
            .ok_or_else(|| Error::Custom("counterparty returned empty client state".to_string()))?;
        let client_state = ClientState::try_from(client_state_response)
            .map_err(|_| Error::Custom("failed to decode client state response".to_string()))?;
        let latest_cp_client_height = client_state.latest_height().revision_height();
        let latest_height = self.latest_height_and_timestamp().await?.0;

        let mut ibc_events: Vec<IbcEventWithHeight> = vec![];
        for height in latest_cp_client_height + 1..latest_height.revision_height() + 1 {
            let block_results = self
                .rpc_client
                .block_results(TmHeight::try_from(height).unwrap())
                .await
                .map_err(|e| {
                    Error::from(format!(
                        "Failed to query block result for height {:?}: {:?}",
                        height, e
                    ))
                })?;
            let tx_results = match block_results.txs_results {
                Some(tx) => tx,
                None => continue,
            };
            for tx in tx_results.iter() {
                for event in tx.clone().events {
                    let ibc_event = ibc_event_try_from_abci_event(&event).ok();
                    match ibc_event {
                        Some(ev) => {
                            ibc_events.push(IbcEventWithHeight::new(
                                ev,
                                Height::new(latest_height.revision_number(), height).unwrap(),
                            ));
                        }
                        None => continue,
                    }
                }
            }
        }
        let update_header = self
            .msg_update_client_header(client_state.latest_height)
            .await?;
        let update_client_header = {
            let msg = MsgUpdateClient {
                client_id: self.client_id(),
                header: update_header.0.into(),
                signer: counterparty.account_id(),
            };
            let value = msg.encode_vec().map_err(|e| {
                Error::from(format!(
                    "Failed to encode MsgUpdateClient {:?}: {:?}",
                    msg, e
                ))
            })?;
            Any {
                value,
                type_url: msg.type_url(),
            }
        };
        Ok((update_client_header, ibc_events, update_header.1))
    }

    // Changed result: `Item =` from `IbcEvent` to `IbcEventWithHeight` to include the necessary height field,
    // as `height` is removed from `Attribute` from ibc-rs v0.22.0
    async fn ibc_events(&self) -> Pin<Box<dyn Stream<Item = IbcEventWithHeight>>> {
        // Create websocket client. Like what `EventMonitor::subscribe()` does in `hermes`
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
        // Collect IBC events from each RPC event, Like what `stream_batches()` does in `hermes`
        let all_subs: Box<
            dyn Stream<Item = core::result::Result<Event, RpcError>> + Send + Sync + Unpin,
        > = Box::new(select_all(subscriptions));
        let chain_id = self.chain_id.clone();
        let events = all_subs
            .map(move |event| {
                // Like what `get_all_events()` does in `hermes`
                let mut events_with_height: Vec<IbcEventWithHeight> = vec![];
                let Event {
                    data,
                    events: _,
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
                        events_with_height.push(IbcEventWithHeight::new(
                            ClientEvents::NewBlock::new(height).into(),
                            height,
                        ));
                        // events_with_height.append(&mut extract_block_events(height, &events));
                    }
                    EventData::Tx { tx_result } => {
                        let height = Height::new(
                            ChainId::chain_version(chain_id.to_string().as_str()),
                            tx_result.height as u64,
                        )
                        .map_err(|_| {
                            Error::from(format!("tx_result.height: invalid header height of 0"))
                        })
                        .unwrap();
                        for abci_event in &tx_result.result.events {
                            if let Ok(ibc_event) = ibc_event_try_from_abci_event(abci_event) {
                                if query == Query::eq("message.module", "ibc_client").to_string()
                                    && event_is_type_client(&ibc_event)
                                {
                                    events_with_height
                                        .push(IbcEventWithHeight::new(ibc_event, height));
                                } else if query
                                    == Query::eq("message.module", "ibc_connection").to_string()
                                    && event_is_type_connection(&ibc_event)
                                {
                                    events_with_height
                                        .push(IbcEventWithHeight::new(ibc_event, height));
                                } else if query
                                    == Query::eq("message.module", "ibc_channel").to_string()
                                    && event_is_type_channel(&ibc_event)
                                {
                                    events_with_height
                                        .push(IbcEventWithHeight::new(ibc_event, height));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                stream::iter(events_with_height)
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
        let mut grpc_client = ibc_proto::ibc::core::client::v1::query_client::QueryClient::connect(
            self.grpc_url.clone().to_string(),
        )
        .await
        .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryConsensusStateRequest {
            client_id: client_id.to_string(),
            revision_number: consensus_height.revision_number(),
            revision_height: consensus_height.revision_height(),
            latest_height: false,
        };

        let response = grpc_client
            .consensus_state(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::ClientConsensusState(ClientConsensusStatePath {
            client_id: client_id.clone(),
            epoch: consensus_height.revision_number(),
            height: consensus_height.revision_height(),
        })
        .to_string()
        .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;
        Ok(QueryConsensusStateResponse {
            consensus_state: response.consensus_state,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_client_state(
        &self,
        at: Height,
        client_id: ClientId,
    ) -> Result<QueryClientStateResponse, Self::Error> {
        let mut grpc_client = ibc_proto::ibc::core::client::v1::query_client::QueryClient::connect(
            self.grpc_url.clone().to_string(),
        )
        .await
        .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryClientStateRequest {
            client_id: client_id.to_string(),
        };

        let response = grpc_client
            .client_state(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::ClientState(ClientStatePath(client_id.clone()))
            .to_string()
            .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;
        Ok(QueryClientStateResponse {
            client_state: response.client_state,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
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

        let request = QueryConnectionRequest {
            connection_id: connection_id.to_string(),
        }
        .into_request();

        let response = grpc_client
            .connection(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let path_bytes = Path::Connections(ConnectionsPath(connection_id.clone()))
            .to_string()
            .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;
        Ok(QueryConnectionResponse {
            connection: response.connection,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_channel_end(
        &self,
        at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<QueryChannelResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryChannelRequest {
            channel_id: channel_id.to_string(),
            port_id: port_id.to_string(),
        }
        .into_request();

        let response = grpc_client
            .channel(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::ChannelEnds(ChannelEndsPath(port_id.clone(), channel_id.clone()))
            .to_string()
            .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;

        Ok(QueryChannelResponse {
            channel: response.channel,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_proof(&self, at: Height, keys: Vec<Vec<u8>>) -> Result<Vec<u8>, Self::Error> {
        let (_, proof) = self.query_path(keys[0].clone(), at, true).await?;
        Ok(proof)
    }

    async fn query_packet_commitment(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketCommitmentResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryPacketCommitmentRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            sequence: seq,
        }
        .into_request();

        let response = grpc_client
            .packet_commitment(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::Commitments(CommitmentsPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence: Sequence::from(seq),
        })
        .to_string()
        .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;
        Ok(QueryPacketCommitmentResponse {
            commitment: response.commitment,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_packet_acknowledgement(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketAcknowledgementResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryPacketAcknowledgementRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            sequence: seq,
        }
        .into_request();

        let response = grpc_client
            .packet_acknowledgement(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::Acks(AcksPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence: Sequence::from(seq),
        })
        .to_string()
        .into_bytes();
        let (res, proof) = self.query_path(path_bytes, at, true).await?;
        Ok(QueryPacketAcknowledgementResponse {
            acknowledgement: res.value,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_next_sequence_recv(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
    ) -> Result<QueryNextSequenceReceiveResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryNextSequenceReceiveRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .into_request();

        let response = grpc_client
            .next_sequence_receive(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        let path_bytes = Path::SeqRecvs(SeqRecvsPath(port_id.clone(), channel_id.clone()))
            .to_string()
            .into_bytes();
        let proof = self.query_proof(at, vec![path_bytes]).await?;
        Ok(QueryNextSequenceReceiveResponse {
            next_sequence_receive: response.next_sequence_receive,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn query_packet_receipt(
        &self,
        at: Height,
        port_id: &PortId,
        channel_id: &ChannelId,
        seq: u64,
    ) -> Result<QueryPacketReceiptResponse, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryPacketReceiptRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            sequence: seq,
        }
        .into_request();

        let response = grpc_client
            .packet_receipt(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let path_bytes = Path::Receipts(ReceiptsPath {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
            sequence: Sequence::from(seq),
        })
        .to_string()
        .into_bytes();
        let (res, proof) = self.query_path(path_bytes, at, true).await?;
        Ok(QueryPacketReceiptResponse {
            received: res.value[0] == 1,
            proof,
            proof_height: incerement_proof_height(response.proof_height),
        })
    }

    async fn latest_height_and_timestamp(&self) -> Result<(Height, Timestamp), Self::Error> {
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
        _at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryPacketCommitmentsRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            pagination: None,
        };
        let request = tonic::Request::new(request);
        let response = grpc_client
            .packet_commitments(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let commitment_sequences: Vec<u64> = response
            .commitments
            .into_iter()
            .map(|v| v.sequence.into())
            .collect();

        Ok(commitment_sequences)
    }

    async fn query_packet_acknowledgements(
        &self,
        _at: Height,
        channel_id: ChannelId,
        port_id: PortId,
    ) -> Result<Vec<u64>, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryPacketAcknowledgementsRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            packet_commitment_sequences: vec![],
            pagination: None,
        };
        let request = tonic::Request::new(request);
        let response = grpc_client
            .packet_acknowledgements(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let commitment_sequences: Vec<u64> = response
            .acknowledgements
            .into_iter()
            .map(|v| v.sequence.into())
            .collect();

        Ok(commitment_sequences)
    }

    async fn query_unreceived_packets(
        &self,
        _at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryUnreceivedPacketsRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            packet_commitment_sequences: seqs.into(),
        };
        let request = tonic::Request::new(request);
        let response = grpc_client
            .unreceived_packets(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let commitment_sequences: Vec<u64> =
            response.sequences.into_iter().map(|v| v.into()).collect();

        Ok(commitment_sequences)
    }

    async fn query_unreceived_acknowledgements(
        &self,
        _at: Height,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<u64>, Self::Error> {
        let mut grpc_client =
            ibc_proto::ibc::core::channel::v1::query_client::QueryClient::connect(
                self.grpc_url.clone().to_string(),
            )
            .await
            .map_err(|e| Error::from(e.to_string()))?;

        let request = QueryUnreceivedAcksRequest {
            port_id: port_id.to_string(),
            channel_id: channel_id.to_string(),
            packet_ack_sequences: seqs.into(),
        };
        let request = tonic::Request::new(request);
        let response = grpc_client
            .unreceived_acks(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();

        let commitment_sequences: Vec<u64> =
            response.sequences.into_iter().map(|v| v.into()).collect();

        Ok(commitment_sequences)
    }

    fn channel_whitelist(&self) -> Vec<(ChannelId, PortId)> {
        self.channel_whitelist.clone()
    }

    async fn query_connection_channels(
        &self,
        _at: Height,
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
    ) -> Result<Vec<PacketInfo>, Self::Error> {
        // let mut block_events = vec![];

        for seq in seqs {
            let query_str = Query::eq(
                format!("{}.packet_src_channel", "send_packet"),
                channel_id.to_string(),
            )
            .and_eq(
                format!("{}.packet_src_port", "send_packet"),
                port_id.to_string(),
            )
            .and_eq(
                format!("{}.packet_sequence", "send_packet"),
                seq.to_string(),
            );

            let mut response = self
                .rpc_client
                .block_search(
                    query_str,
                    1,
                    1, // get only the first Tx matching the query
                    Order::Ascending,
                )
                .await
                .map_err(|e| Error::RpcError(format!("{:?}", e)))?;

            // if let Some(block) = response.blocks.first().map(|first| &first.block) {
            //     let tm_height =
            //         tendermint::block::Height::try_from(block.header.height.value()).unwrap();
            //     let response = self
            //         .rpc_client
            //         .block_results(tm_height)
            //         .await
            //         .map_err(|e| Error::RpcError(format!("{:?}", e)))?;

            //     block_events.append(
            //         &mut response
            //             .begin_block_events
            //             .unwrap_or_default()
            //             .into_iter()
            //             .filter_map(|ev| filter_matching_event(ev, request, seqs))
            //             .map(|ev| IbcEventWithHeight::new(ev, response_height))
            //             .collect(),
            //     );
            // }
        }
        todo!()
    }

    async fn query_recv_packets(
        &self,
        channel_id: ChannelId,
        port_id: PortId,
        seqs: Vec<u64>,
    ) -> Result<Vec<PacketInfo>, Self::Error> {
        todo!()
    }

    fn expected_block_time(&self) -> Duration {
        Duration::from_secs(30)
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
        let denom = "samoleans";
        let mut grpc_client = ibc_proto::cosmos::bank::v1beta1::query_client::QueryClient::connect(
            self.grpc_url.clone().to_string(),
        )
        .await
        .map_err(|e| Error::from(format!("{:?}", e)))?;

        let request = tonic::Request::new(QueryBalanceRequest {
            address: self.keybase.clone().account,
            denom: denom.to_string(),
        });

        let response = grpc_client
            .balance(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| Error::from(format!("{:?}", e)))?;

        // Querying for a balance might fail, i.e. if the account doesn't actually exist
        let balance = response
            .balance
            .ok_or_else(|| Error::from(format!("No balance for denom {}", denom)))?;

        Ok(vec![PrefixedCoin {
            denom: PrefixedDenom::from_str(balance.denom.as_str()).unwrap(),
            amount: Amount::from_str(balance.amount.as_str()).unwrap(),
        }])
    }

    fn connection_prefix(&self) -> CommitmentPrefix {
        CommitmentPrefix::try_from(self.commitment_prefix.clone()).expect("Should not fail")
    }

    fn client_id(&self) -> ClientId {
        self.client_id()
    }

    fn client_type(&self) -> ClientType {
        ClientType::from_str("07-tendermint")
            .map_err(|e| Error::from(format!("{:?}", e)))
            .unwrap()
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection_id
            .as_ref()
            .expect("Connection id should be defined")
            .clone()
    }

    async fn query_timestamp_at(&self, block_number: u64) -> Result<u64, Self::Error> {
        let height = TmHeight::try_from(block_number)
            .map_err(|e| Error::from(format!("Invalid block number: {}", e)))?;
        let response = self
            .rpc_client
            .block(height)
            .await
            .map_err(|e| Error::RpcError(e.to_string()))?;
        let time: Timestamp = response.block.header.time.into();
        Ok(time.nanoseconds())
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
            Duration::from_secs(64000),
            Duration::from_secs(1814400),
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
