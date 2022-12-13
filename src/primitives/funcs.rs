use super::traits::Chain;
use crate::core::error::Error;
use crate::core::packets::{types::PacketInfo, utils::calculate_block_delay};
use ibc_relayer_types::{
    clients::ics07_tendermint::{client_state::ClientState, consensus_state::ConsensusState},
    core::{
        ics04_channel::{
            channel::{ChannelEnd, Order},
            packet::Packet,
        },
        ics24_host::identifier::{ChannelId, ClientId, PortId},
    },
    timestamp::Timestamp,
    Height,
};
use std::{str::FromStr, time::Duration};

/// Returns undelivered packet sequences that have been sent out from
/// the `source` chain to the `sink` chain
/// works for both ordered and unordered channels
pub async fn query_undelivered_sequences(
    source_height: Height,
    sink_height: Height,
    channel_id: ChannelId,
    port_id: PortId,
    source: &impl Chain,
    sink: &impl Chain,
) -> Result<Vec<u64>, anyhow::Error> {
    let channel_response = source
        .query_channel_end(source_height.clone(), channel_id.clone(), port_id.clone())
        .await?;
    let channel_end = ChannelEnd::try_from(
        channel_response
            .channel
            .ok_or_else(|| Error::Custom("ChannelEnd not could not be decoded".to_string()))?,
    )
    .map_err(|e| Error::Custom(e.to_string()))?;
    // First we fetch all packet commitments from source
    let seqs = source
        .query_packet_commitments(source_height, channel_id, port_id.clone())
        .await?;
    let counterparty_channel_id = channel_end
        .counterparty()
        .clone()
        .channel_id
        .ok_or_else(|| Error::Custom("Expected counterparty channel id".to_string()))?;
    let counterparty_port_id = channel_end.counterparty().port_id.clone();

    let undelivered_sequences = if channel_end.ordering == Order::Unordered {
        sink.query_unreceived_packets(
            sink_height,
            counterparty_channel_id,
            counterparty_port_id.clone(),
            seqs,
        )
        .await?
    } else {
        let next_seq_recv = sink
            .query_next_sequence_recv(sink_height, &counterparty_port_id, &counterparty_channel_id)
            .await?
            .next_sequence_receive;
        seqs.into_iter()
            .filter(|seq| *seq > next_seq_recv)
            .collect()
    };

    Ok(undelivered_sequences)
}

/// Queries the `source` chain for packet acknowledgements that have not been seen by the `sink`
/// chain.
pub async fn query_undelivered_acks(
    source_height: Height,
    sink_height: Height,
    channel_id: ChannelId,
    port_id: PortId,
    source: &impl Chain,
    sink: &impl Chain,
) -> Result<Vec<u64>, anyhow::Error> {
    let channel_response = source
        .query_channel_end(source_height.clone(), channel_id.clone(), port_id.clone())
        .await?;
    let channel_end = ChannelEnd::try_from(
        channel_response
            .channel
            .ok_or_else(|| Error::Custom("ChannelEnd not could not be decoded".to_string()))?,
    )
    .map_err(|e| Error::Custom(e.to_string()))?;
    // First we fetch all packet acknowledgements from source
    let seqs = source
        .query_packet_acknowledgements(source_height, channel_id, port_id.clone())
        .await?;
    let counterparty_channel_id = channel_end
        .counterparty()
        .clone()
        .channel_id
        .ok_or_else(|| Error::Custom("Expected counterparty channel id".to_string()))?;
    let counterparty_port_id = channel_end.counterparty().port_id.clone();

    let undelivered_acks = sink
        .query_unreceived_acknowledgements(
            sink_height,
            counterparty_channel_id,
            counterparty_port_id.clone(),
            seqs,
        )
        .await?;

    Ok(undelivered_acks)
}

pub fn packet_info_to_packet(packet_info: &PacketInfo) -> Packet {
    Packet {
        sequence: packet_info.sequence.into(),
        source_port: PortId::from_str(&packet_info.source_port).expect("Port should be valid"),
        source_channel: ChannelId::from_str(&packet_info.source_channel)
            .expect("Channel should be valid"),
        destination_port: PortId::from_str(&packet_info.destination_port)
            .expect("Port should be valid"),
        destination_channel: ChannelId::from_str(&packet_info.destination_channel)
            .expect("Channel should be valid"),
        data: packet_info.data.clone(),
        timeout_height: packet_info
            .timeout_height
            .clone()
            .try_into()
            .expect("Height should be valid"),
        timeout_timestamp: Timestamp::from_nanoseconds(packet_info.timeout_timestamp)
            .expect("Timestamp should be valid"),
    }
}

/// Should return the first client height with a latest_height and consensus state timestamp that
/// is equal to or greater than the values provided
pub async fn find_suitable_proof_height_for_client(
    chain: &impl Chain,
    at: Height,
    client_id: ClientId,
    start_height: Height,
    timestamp_to_match: Option<Timestamp>,
    latest_client_height: Height,
) -> Option<Height> {
    // If searching for existence of just a height we use a pure linear search because there's no
    // valid comparison to be made and there might be missing values  for some heights
    if timestamp_to_match.is_none() {
        for height in start_height.revision_height()..=latest_client_height.revision_height() {
            let temp_height = Height::new(start_height.revision_number(), height)
                .map_err(|_| {
                    Error::from(format!(
                        "Could not create Height from revision number {} and height {}",
                        start_height.revision_number(),
                        height
                    ))
                })
                .unwrap();
            let consensus_state = chain
                .query_client_consensus(at.clone(), client_id.clone(), temp_height.clone())
                .await
                .ok();
            if consensus_state.is_none() {
                continue;
            }
            return Some(temp_height);
        }
    } else {
        let timestamp_to_match = timestamp_to_match.unwrap();
        let mut start = start_height.revision_height();
        let mut end = latest_client_height.revision_height();
        let mut last_known_valid_height = None;
        if start > end {
            return None;
        }
        while end - start > 1 {
            let mid = (end + start) / 2;
            let temp_height = Height::new(start_height.revision_number(), mid)
                .map_err(|_| {
                    Error::from(format!(
                        "Could not create Height from revision number {} and height {}",
                        start_height.revision_number(),
                        mid
                    ))
                })
                .unwrap();
            let consensus_state = chain
                .query_client_consensus(at.clone(), client_id.clone(), temp_height.clone())
                .await
                .ok();
            if consensus_state.is_none() {
                start += 1;
                continue;
            }

            let consensus_state =
                ConsensusState::try_from(consensus_state.unwrap().consensus_state?).ok()?;
            let consensus_time: Timestamp = consensus_state.timestamp.into();
            if consensus_time.nanoseconds() < timestamp_to_match.nanoseconds() {
                start = mid + 1;
                continue;
            } else {
                last_known_valid_height = Some(temp_height);
                end = mid;
            }
        }
        let start_height = Height::new(start_height.revision_number(), start)
            .map_err(|_| {
                Error::from(format!(
                    "Could not create Height from revision number {} and height {}",
                    start_height.revision_number(),
                    start
                ))
            })
            .unwrap();
        let consensus_state = chain
            .query_client_consensus(at, client_id.clone(), start_height.clone())
            .await
            .ok();
        if let Some(consensus_state) = consensus_state {
            let consensus_state =
                ConsensusState::try_from(consensus_state.consensus_state?).ok()?;
            let consensus_time: Timestamp = consensus_state.timestamp.into();
            if consensus_time.nanoseconds() >= timestamp_to_match.nanoseconds() {
                return Some(start_height);
            }
        }

        return last_known_valid_height;
    }
    None
}

pub async fn query_maximum_height_for_timeout_proofs(
    source: &impl Chain,
    sink: &impl Chain,
) -> Option<u64> {
    let mut min_timeout_height = None;
    let (source_height, ..) = source.latest_height_and_timestamp().await.ok()?;
    let (sink_height, ..) = sink.latest_height_and_timestamp().await.ok()?;
    for (channel, port_id) in source.channel_whitelist() {
        let undelivered_sequences = query_undelivered_sequences(
            source_height,
            sink_height,
            channel.clone(),
            port_id.clone(),
            source,
            sink,
        )
        .await
        .ok()?;
        let send_packets = source
            .query_send_packets(channel, port_id, undelivered_sequences)
            .await
            .ok()?;
        for send_packet in send_packets {
            let sink_client_state = source
                .query_client_state(source_height, sink.client_id())
                .await
                .ok()?;
            let sink_client_state = ClientState::try_from(sink_client_state.client_state?).ok()?;
            let height = sink_client_state.latest_height();
            let timestamp_at_creation = sink
                .query_timestamp_at(height.revision_height())
                .await
                .ok()?;
            let period = send_packet
                .timeout_timestamp
                .saturating_sub(timestamp_at_creation);
            if period == 0 {
                min_timeout_height =
                    min_timeout_height.max(Some(send_packet.timeout_height.revision_height));
                continue;
            }
            let period = Duration::from_nanos(period);
            let approx_height =
                calculate_block_delay(period, sink.expected_block_time()).saturating_add(1);
            let timeout_height = if send_packet.timeout_height.revision_height < approx_height {
                send_packet.timeout_height.revision_height
            } else {
                approx_height
            };

            min_timeout_height = min_timeout_height.max(Some(timeout_height))
        }
    }
    min_timeout_height
}

pub fn apply_prefix(mut commitment_prefix: Vec<u8>, path: String) -> Vec<u8> {
    let path = path.as_bytes().to_vec();
    commitment_prefix.extend_from_slice(&path);
    commitment_prefix
}
