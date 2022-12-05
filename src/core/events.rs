// Copyright 2022 ComposableFi
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::packets::query_ready_and_timed_out_packets;
use crate::{
    core::{error::Error, primitives::Chain},
    cosmos::events::IbcEventWithHeight,
};
use codec::Encode;
use ibc_proto::protobuf::Protobuf;
use ibc_proto::{google::protobuf::Any, ibc::core::client::v1::QueryConsensusStateResponse};
use ibc_relayer_types::{
    clients::ics07_tendermint::client_state::ClientState,
    core::{
        ics03_connection::{
            connection::{ConnectionEnd, Counterparty},
            msgs::{
                conn_open_ack::MsgConnectionOpenAck, conn_open_confirm::MsgConnectionOpenConfirm,
                conn_open_try::MsgConnectionOpenTry,
            },
        },
        ics04_channel::{
            channel::{ChannelEnd, Counterparty as ChannelCounterparty},
            msgs::{
                acknowledgement::MsgAcknowledgement, chan_close_confirm::MsgChannelCloseConfirm,
                chan_open_ack::MsgChannelOpenAck, chan_open_confirm::MsgChannelOpenConfirm,
                chan_open_try::MsgChannelOpenTry, recv_packet::MsgRecvPacket,
            },
        },
        ics23_commitment::commitment::{CommitmentPrefix, CommitmentProofBytes},
    },
    events::{IbcEvent, IbcEventType},
    proofs::{ConsensusProof, Proofs},
    tx_msg::Msg,
    Height,
};

/// Connection proof type
#[derive(Encode)]
pub struct ConnectionProof {
    pub host_proof: Vec<u8>,
    pub connection_proof: Vec<u8>,
}

/// This parses events coming from a source chain
/// Returns a tuple of messages, with the first item being packets that are ready to be sent to the
/// sink chain. And the second item being packet timeouts that should be sent to the source.
pub async fn parse_events(
    source: &mut impl Chain,
    sink: &mut impl Chain,
    events: Vec<IbcEventWithHeight>,
) -> Result<(Vec<Any>, Vec<Any>), anyhow::Error> {
    let mut messages = vec![];
    // 1. translate events to messages
    for ev in events {
        log::info!(
            target: "hyperspace-light",
            "🔍 Processing event: {:?} at height: {}",
            ev.event,
            ev.height
        );
        match ev.event {
            IbcEvent::OpenInitConnection(open_init) => {
                if let Some(connection_id) = open_init.connection_id() {
                    let connection_id = connection_id.clone();
                    // Get connection end with proof
                    let connection_response = source
                        .query_connection_end(ev.height, connection_id.clone())
                        .await?;
                    let connection_end = ConnectionEnd::try_from(
                        connection_response.connection.ok_or_else(|| {
                            Error::Custom(format!(
								"[get_messages_for_events - open_conn_init] Connection end not found for {:?}",
								open_init.attributes().connection_id
							))
                        })?,
                    )?;
                    let counterparty = connection_end.counterparty();
                    let connection_proof =
                        CommitmentProofBytes::try_from(connection_response.proof)?;
                    let prefix: CommitmentPrefix = source.connection_prefix();
                    let client_state_response = source
                        .query_client_state(ev.height, open_init.attributes().client_id.clone())
                        .await?;
                    let proof_height = connection_response.proof_height.ok_or_else(|| Error::Custom(format!("[get_messages_for_events - open_conn_init] Proof height not found in response")))?;
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_conn_init] Invalid proof height: {:?}",
                            e
                        ))
                            })?;
                    let client_state_proof =
                        CommitmentProofBytes::try_from(client_state_response.proof).ok();

                    let client_state = client_state_response
                        .client_state
                        .map(ClientState::try_from)
                        .ok_or_else(|| Error::Custom(format!("Client state is empty")))??;
                    let consensus_proof = source
                        .query_client_consensus(
                            ev.height,
                            open_init.attributes().client_id.clone(),
                            client_state.latest_height(),
                        )
                        .await?;
                    let consensus_proof =
                        query_consensus_proof(sink, client_state.clone(), consensus_proof).await?;
                    // Construct OpenTry
                    let msg = MsgConnectionOpenTry {
                        previous_connection_id: counterparty.connection_id.clone(),
                        client_id: counterparty.client_id().clone(),
                        // client state proof is mandatory in conn_open_try
                        client_state: Some(client_state.clone().into()),
                        counterparty: Counterparty::new(
                            open_init.attributes().client_id.clone(),
                            Some(connection_id),
                            prefix,
                        ),
                        counterparty_versions: connection_end.versions().to_vec(),
                        proofs: Proofs::new(
                            connection_proof,
                            client_state_proof,
                            Some(ConsensusProof::new(
                                CommitmentProofBytes::try_from(consensus_proof)?,
                                client_state.latest_height(),
                            )?),
                            None,
                            proof_height.increment(),
                        )?,
                        delay_period: connection_end.delay_period(),
                        signer: sink.account_id(),
                    };
                    log::info!("MsgConnectionOpenTry: {:?}", msg);
                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_conn_init] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::OpenTryConnection(open_try) => {
                if let Some(connection_id) = open_try.connection_id() {
                    let connection_id = connection_id.clone();
                    // Get connection end with proof
                    let connection_response = source
                        .query_connection_end(ev.height, connection_id.clone())
                        .await?;
                    let connection_end = ConnectionEnd::try_from(
                        connection_response.connection.ok_or_else(|| {
                            Error::Custom(format!(
								"[get_messages_for_events - open_conn_try] Connection end not found for {:?}",
								open_try.attributes().connection_id
							))
                        })?,
                    )?;
                    let counterparty = connection_end.counterparty();

                    let connection_proof =
                        CommitmentProofBytes::try_from(connection_response.proof)?;
                    let client_state_response = source
                        .query_client_state(ev.height, open_try.attributes().client_id.clone())
                        .await?;

                    let proof_height = connection_response.proof_height.ok_or_else(|| Error::Custom(format!("[get_messages_for_events - open_conn_try] Proof height not found in response")))?;
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_conn_try] Invalid proof height: {:?}",
                            e
                        ))
                            })?;
                    let client_state_proof =
                        CommitmentProofBytes::try_from(client_state_response.proof).ok();
                    let client_state = client_state_response
                        .client_state
                        .map(ClientState::try_from)
                        .ok_or_else(|| Error::Custom(format!("Client state is empty")))??;
                    let consensus_proof = source
                        .query_client_consensus(
                            ev.height,
                            open_try.attributes().client_id.clone(),
                            client_state.latest_height(),
                        )
                        .await?;
                    let consensus_proof =
                        query_consensus_proof(sink, client_state.clone(), consensus_proof).await?;
                    // Construct OpenAck
                    let msg = MsgConnectionOpenAck {
						connection_id: counterparty
							.connection_id()
							.ok_or_else(|| {
								Error::Custom(format!("[get_messages_for_events - open_conn_try] Connection Id not found"))
							})?
							.clone(),
						counterparty_connection_id: connection_id,
						client_state: Some(client_state.clone().into()),
						proofs: Proofs::new(
							connection_proof,
							client_state_proof,
							Some(ConsensusProof::new(
								CommitmentProofBytes::try_from(consensus_proof)?,
								client_state.latest_height(),
							)?),
							None,
							proof_height,
						)?,
						version: connection_end
							.versions()
							.get(0)
							.ok_or_else(|| {
								Error::Custom(format!(
									"[get_messages_for_events - open_conn_try] Connection version is missing for  {:?}",
									open_try.attributes().connection_id
								))
							})?
							.clone(),
						signer: sink.account_id(),
					};
                    log::info!("MsgConnectionOpenAck: {:?}", msg);
                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_conn_try] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::OpenAckConnection(open_ack) => {
                if let Some(connection_id) = open_ack.connection_id() {
                    let connection_id = connection_id.clone();
                    // Get connection end with proof
                    let connection_response = source
                        .query_connection_end(ev.height, connection_id.clone())
                        .await?;
                    let connection_end = ConnectionEnd::try_from(
                        connection_response.connection.ok_or_else(|| {
                            Error::Custom(format!(
								"[get_messages_for_events - open_conn_ack] Connection end not found for {:?}",
								open_ack.attributes().connection_id
							))
                        })?,
                    )?;
                    let counterparty = connection_end.counterparty();

                    let connection_proof =
                        CommitmentProofBytes::try_from(connection_response.proof)?;

                    let proof_height = connection_response.proof_height.ok_or_else(|| {
						Error::Custom(format!("[get_messages_for_events - open_conn_ack] Proof height not found in response"))
					})?;
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_conn_ack] Invalid proof height: {:?}",
                            e
                        ))
                            })?;

                    // Construct OpenConfirm
                    let msg = MsgConnectionOpenConfirm {
						connection_id: counterparty
							.connection_id()
							.ok_or_else(|| {
								Error::Custom(format!("[get_messages_for_events - open_conn_ack] Connection Id not found"))
							})?
							.clone(),
						proofs: Proofs::new(connection_proof, None, None, None, proof_height)?,

						signer: sink.account_id(),
					};
                    log::info!("MsgConnectionOpenConfirm: {:?}", msg);
                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_conn_ack] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::OpenInitChannel(open_init) => {
                if let Some(channel_id) = open_init.channel_id {
                    let channel_response = source
                        .query_channel_end(ev.height, channel_id.clone(), open_init.port_id.clone())
                        .await?;
                    let channel_end =
                        ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                            Error::Custom(format!(
                                "[get_messages_for_events - open_chan_init] ChannelEnd not found"
                            ))
                        })?)
                        .expect("Channel end decoding should not fail");
                    let counterparty = channel_end.counterparty();
                    // Construct the channel end as we expect it to be constructed on the
                    // receiving chain
                    let channel = ChannelEnd::new(
                        channel_end.state,
                        channel_end.ordering,
                        ChannelCounterparty::new(open_init.port_id, Some(channel_id)),
                        channel_end.connection_hops.clone(),
                        channel_end.version.clone(),
                    );

                    let channel_proof = CommitmentProofBytes::try_from(channel_response.proof)?;

                    let proof_height = channel_response.proof_height.expect(
                        "[get_messages_for_events - open_chan_init]Proof height should be present",
                    );
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_chan_init] Invalid proof height: {:?}",
                            e
                        ))
                            })?;

                    let msg = MsgChannelOpenTry {
                        previous_channel_id: None,
                        port_id: counterparty.port_id.clone(),
                        channel,
                        counterparty_version: channel_end.version,
                        proofs: Proofs::new(channel_proof, None, None, None, proof_height)?,

                        signer: sink.account_id(),
                    };
                    log::info!("MsgChannelOpenTry: {:?}", msg);
                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_chan_init] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::OpenTryChannel(open_try) => {
                if let Some(channel_id) = open_try.channel_id {
                    let channel_response = source
                        .query_channel_end(ev.height, channel_id.clone(), open_try.port_id.clone())
                        .await?;
                    let channel_end =
                        ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                            Error::Custom(format!(
                                "[get_messages_for_events - open_chan_try] ChannelEnd not found"
                            ))
                        })?)
                        .expect("Channel end decoding should not fail");
                    let counterparty = channel_end.counterparty().clone();
                    let channel_proof = CommitmentProofBytes::try_from(channel_response.proof)?;

                    let proof_height = channel_response.proof_height.expect(
                        "[get_messages_for_events - open_chan_try] Proof height should be present",
                    );
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_chan_try] Invalid proof height: {:?}",
                            e
                        ))
                            })?;

                    let msg = MsgChannelOpenAck {
                        port_id: counterparty.port_id.clone(),
                        counterparty_version: channel_end.version.clone(),
                        proofs: Proofs::new(channel_proof, None, None, None, proof_height)?,
                        channel_id: counterparty
                            .channel_id
                            .expect("Expect channel id to be set"),
                        counterparty_channel_id: channel_id,

                        signer: sink.account_id(),
                    };

                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_chan_try] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::OpenAckChannel(open_ack) => {
                if let Some(channel_id) = open_ack.channel_id {
                    let channel_response = source
                        .query_channel_end(ev.height, channel_id, open_ack.port_id.clone())
                        .await?;
                    let channel_end =
                        ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                            Error::Custom(format!(
                                "[get_messages_for_events - open_chan_ack] ChannelEnd not found"
                            ))
                        })?)?;
                    let counterparty = channel_end.counterparty().clone();
                    let channel_proof = CommitmentProofBytes::try_from(channel_response.proof)?;

                    let proof_height = channel_response
                        .proof_height
                        .expect("Proof height should be present");
                    let proof_height =
                        Height::new(proof_height.revision_number, proof_height.revision_height)
                            .map_err(|e| {
                                Error::Custom(format!(
                            "[get_messages_for_events - open_chan_ack] Invalid proof height: {:?}",
                            e
                        ))
                            })?;

                    let msg = MsgChannelOpenConfirm {
                        port_id: counterparty.port_id.clone(),
                        proofs: Proofs::new(channel_proof, None, None, None, proof_height)?,
                        channel_id: counterparty
                            .channel_id
                            .expect("Expect channel id to be set"),

                        signer: sink.account_id(),
                    };

                    let value = msg.encode_vec().map_err(|e| {
                        Error::Custom(format!(
                            "[get_messages_for_events - open_chan_ack] Error encoding message: {:?}",
                            e
                        ))
                    })?;
                    let msg = Any {
                        value,
                        type_url: msg.type_url(),
                    };
                    messages.push(msg)
                }
            }
            IbcEvent::CloseInitChannel(close_init) => {
                let channel_id = close_init.channel_id;
                let channel_response = source
                    .query_channel_end(ev.height, channel_id, close_init.port_id.clone())
                    .await?;
                let channel_end =
                    ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                        Error::Custom(format!(
                            "[get_messages_for_events - close_chan_init] ChannelEnd not found"
                        ))
                    })?)?;
                let counterparty = channel_end.counterparty().clone();
                let channel_proof = CommitmentProofBytes::try_from(channel_response.proof)?;

                let proof_height = channel_response
                    .proof_height
                    .expect("Proof height should be present");
                let proof_height =
                    Height::new(proof_height.revision_number, proof_height.revision_height)
                        .map_err(|e| {
                            Error::Custom(format!(
                        "[get_messages_for_events - close_chan_init] Invalid proof height: {:?}",
                        e
                    ))
                        })?;

                let msg = MsgChannelCloseConfirm {
                    port_id: counterparty.port_id.clone(),
                    proofs: Proofs::new(channel_proof, None, None, None, proof_height)?,
                    channel_id: counterparty
                        .channel_id
                        .expect("Expect channel id to be set"),

                    signer: sink.account_id(),
                };

                let value = msg.encode_vec().map_err(|e| {
                    Error::Custom(format!(
                        "[get_messages_for_events - close_chan_init] Error encoding message: {:?}",
                        e
                    ))
                })?;
                let msg = Any {
                    value,
                    type_url: msg.type_url(),
                };
                messages.push(msg)
            }
            IbcEvent::SendPacket(send_packet) => {
                // can we send this packet?
                // 1. query the connection and get the connection delay.
                // 2. if none, send message immediately
                // 3. otherwise skip.
                let port_id = send_packet.packet.source_port.clone();
                let channel_id = send_packet.packet.source_channel.clone();
                let channel_response = source
                    .query_channel_end(ev.height, channel_id.clone(), port_id.clone())
                    .await?;
                let channel_end =
                    ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                        Error::Custom(format!(
                            "Failed to convert to concrete channel end from raw channel end",
                        ))
                    })?)?;
                let connection_id = channel_end
                    .connection_hops
                    .get(0)
                    .ok_or_else(|| Error::Custom("Channel end missing connection id".to_string()))?
                    .clone();
                let connection_response = source
                    .query_connection_end(ev.height, connection_id.clone())
                    .await?;
                let connection_end =
                    ConnectionEnd::try_from(connection_response.connection.ok_or_else(|| {
                        Error::Custom(format!("ConnectionEnd not found for {:?}", connection_id))
                    })?)?;
                if !connection_end.delay_period().is_zero() {
                    // We can't send this packet immediately because of connection delays
                    continue;
                }
                let seq = u64::from(send_packet.packet.sequence);
                let packet = send_packet.packet;
                let packet_commitment_response = source
                    .query_packet_commitment(ev.height, &port_id, &channel_id, seq)
                    .await?;
                let commitment_proof =
                    CommitmentProofBytes::try_from(packet_commitment_response.proof)?;
                let proof_height = packet_commitment_response
                    .proof_height
                    .expect("Proof height should be present");
                let proof_height =
                    Height::new(proof_height.revision_number, proof_height.revision_height)
                        .map_err(|e| {
                            Error::Custom(format!(
                            "[get_messages_for_events - send_packet] Invalid proof height: {:?}",
                            e
                        ))
                        })?;
                let msg = MsgRecvPacket {
                    packet: packet.clone(),
                    proofs: Proofs::new(
                        commitment_proof,
                        None,
                        None,
                        None,
                        proof_height.increment(),
                    )?,
                    signer: sink.account_id(),
                };
                let value = msg.encode_vec().map_err(|e| {
                    Error::Custom(format!(
                        "[get_messages_for_events - send_packet] Error encoding message: {:?}",
                        e
                    ))
                })?;
                let msg = Any {
                    value,
                    type_url: msg.type_url(),
                };
                messages.push(msg);
            }
            IbcEvent::WriteAcknowledgement(write_ack) => {
                let port_id = &write_ack.packet.source_port.clone();
                let channel_id = &write_ack.packet.source_channel.clone();
                let channel_response = source
                    .query_channel_end(ev.height, channel_id.clone(), port_id.clone())
                    .await?;
                let channel_end =
                    ChannelEnd::try_from(channel_response.channel.ok_or_else(|| {
                        Error::Custom(format!(
                            "Failed to convert to concrete channel end from raw channel end",
                        ))
                    })?)?;
                let connection_id = channel_end
                    .connection_hops
                    .get(0)
                    .ok_or_else(|| Error::Custom("Channel end missing connection id".to_string()))?
                    .clone();
                let connection_response = source
                    .query_connection_end(ev.height, connection_id.clone())
                    .await?;
                let connection_end =
                    ConnectionEnd::try_from(connection_response.connection.ok_or_else(|| {
                        Error::Custom(format!("ConnectionEnd not found for {:?}", connection_id))
                    })?)?;
                if !connection_end.delay_period().is_zero() {
                    // We can't send this packet immediately because of connection delays
                    continue;
                }
                let seq = u64::from(write_ack.packet.sequence);
                let packet = write_ack.packet;
                let packet_acknowledgement_response = source
                    .query_packet_acknowledgement(ev.height, &port_id, &channel_id, seq)
                    .await?;
                let acknowledgement = write_ack.ack;
                let commitment_proof =
                    CommitmentProofBytes::try_from(packet_acknowledgement_response.proof)?;

                let proof_height = packet_acknowledgement_response
                    .proof_height
                    .expect("Proof height should be present");
                let proof_height =
                    Height::new(proof_height.revision_number, proof_height.revision_height)
                        .map_err(|e| {
                            Error::Custom(format!(
                                "[get_messages_for_events - write_ack] Invalid proof height: {:?}",
                                e
                            ))
                        })?;
                let msg = MsgAcknowledgement {
                    packet,
                    acknowledgement: acknowledgement.into(),
                    proofs: Proofs::new(commitment_proof, None, None, None, proof_height).map_err(
                        |e| {
                            Error::Custom(format!(
                                "[get_messages_for_events - write_ack] Invalid proof height: {:?}",
                                e
                            ))
                        },
                    )?,

                    signer: sink.account_id(),
                };

                let value = msg.encode_vec().map_err(|e| {
                    Error::Custom(format!(
                        "[get_messages_for_events - write_ack] Error encoding message: {:?}",
                        e
                    ))
                })?;
                let msg = Any {
                    value,
                    type_url: msg.type_url(),
                };
                messages.push(msg)
            }
            _ => continue,
        }
    }

    // 2. query packets that can now be sent, at this sink height because of connection delay.
    // let (ready_packets, timed_out_packets) =
    //     query_ready_and_timed_out_packets(source, sink).await?;
    // messages.extend(ready_packets);
    Ok((messages, vec![]))
}

/// Fetch the connection proof for the sink chain.
async fn query_consensus_proof(
    sink: &impl Chain,
    _client_state: ClientState,
    consensus_proof: QueryConsensusStateResponse,
) -> Result<Vec<u8>, anyhow::Error> {
    let _client_type = sink.client_type();
    let consensus_proof_bytes = consensus_proof.proof;
    Ok(consensus_proof_bytes)
}

pub fn has_packet_events(event_types: &[IbcEventType]) -> bool {
    event_types.into_iter().any(|event_type| {
        matches!(
            event_type,
            &IbcEventType::SendPacket | &IbcEventType::WriteAck
        )
    })
}
