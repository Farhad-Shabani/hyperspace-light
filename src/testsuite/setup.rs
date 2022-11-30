use super::create::{create_channel, create_clients, create_connection};
use crate::core::primitives::{IbcProvider, TestProvider};
// use crate::core::relay::relay;
use crate::cosmos::{client::CosmosClient, client::CosmosClientConfig, key_provider::KeyEntry};
use futures::{future, StreamExt};
use ibc_relayer_types::events::IbcEvent;
use std::{path::PathBuf, str::FromStr};
use tendermint_rpc::Url;

use ibc_relayer_types::{
    applications::transfer::VERSION,
    core::{
        ics04_channel::channel::{ChannelEnd, Order, State},
        ics24_host::identifier::{ChannelId, ConnectionId, PortId},
    },
};
use std::time::Duration;
use tokio::task::JoinHandle;

pub async fn setup_clients<H: Clone + Send + Sync + 'static>() -> (CosmosClient<H>, CosmosClient<H>)
{
    log::info!(target: "hyperspace-light", "============================== Starting Test ==============================");

    // Create client configurations
    // Parameters have been set up to work with local nodes according to https://hermes.informal.systems/tutorials
    let config_a = CosmosClientConfig {
        name: "chain_a".to_string(),
        rpc_url: Url::from_str("http://127.0.0.1:27030").unwrap(),
        grpc_url: Url::from_str("http://127.0.0.1:27032").unwrap(),
        websocket_url: Url::from_str("ws://127.0.0.1:27030/websocket").unwrap(),
        chain_id: "ibc-0".to_string(),
        client_id: Some("7-tendermint".to_string()),
        connection_id: None,
        account_prefix: "cosmos".to_string(),
        store_prefix: "ibc".to_string(),
        keybase: KeyEntry::from_file(
            PathBuf::from_str("/Users/farhad/.hermes/keys/ibc-0/keyring-test/wallet.json").unwrap(),
        )
        .unwrap(),
    };

    let config_b = CosmosClientConfig {
        name: "chain_b".to_string(),
        rpc_url: Url::from_str("http://127.0.0.1:27040").unwrap(),
        grpc_url: Url::from_str("http://127.0.0.1:27042").unwrap(),
        websocket_url: Url::from_str("ws://127.0.0.1:27040/websocket").unwrap(),
        chain_id: "ibc-1".to_string(),
        client_id: Some("7-tendermint".to_string()),
        connection_id: None,
        account_prefix: "cosmos".to_string(),
        store_prefix: "ibc".to_string(),
        keybase: KeyEntry::from_file(
            PathBuf::from_str("/Users/farhad/.hermes/keys/ibc-1/keyring-test/wallet.json").unwrap(),
        )
        .unwrap(),
    };

    let mut chain_a = CosmosClient::<H>::new(config_a).await.unwrap();
    let mut chain_b = CosmosClient::<H>::new(config_b).await.unwrap();

    // Wait until for cosmos to start producing blocks
    log::info!(target: "hyperspace-light", "Waiting for block production from Cosmos chains");
    let new_block_chain_a = chain_a
        .ibc_events()
        .await
        .skip_while(|ev| future::ready(!matches!(ev.event, IbcEvent::NewBlock(_))))
        .take(1)
        .collect::<Vec<_>>()
        .await;
    log::info!(target: "hyperspace-light", "Received {:?} events", new_block_chain_a);

    let new_block_chain_b = chain_b
        .ibc_events()
        .await
        .skip_while(|ev| future::ready(!matches!(ev.event, IbcEvent::NewBlock(_))))
        .take(1)
        .collect::<Vec<_>>()
        .await;
    log::info!(target: "hyperspace-light", "Received {:?} events", new_block_chain_b);
    log::info!(target: "hyperspace-light", "Cosmos chains are ready to go!");

    // Check if the clients are already created
    let clients_on_a = chain_a.query_clients().await.unwrap();
    log::info!(target: "hyperspace-light", "Clients on chain_a: {:?}", clients_on_a);
    let clients_on_b = chain_b.query_clients().await.unwrap();
    log::info!(target: "hyperspace-light", "Clients on chain_b: {:?}", clients_on_b);

    if !clients_on_a.is_empty() && !clients_on_b.is_empty() {
        chain_a.set_client_id(clients_on_b[0].clone());
        chain_b.set_client_id(clients_on_b[0].clone());
        return (chain_a, chain_b);
    }

    let height = chain_a.latest_height_and_timestamp().await.unwrap();
    log::trace!(target: "hyperspace-light", "Latest height on chain_a: {:?}", height);
    let time = chain_a.query_timestamp_at(10).await.unwrap();
    log::trace!(target: "hyperspace-light", "Timestamp at height 10 on chain_a: {:?}", time);
    let channels = chain_a.query_channels().await.unwrap();
    log::trace!(target: "hyperspace-light", "Channels on chain_a: {:?}", channels);

    let height = chain_b.latest_height_and_timestamp().await.unwrap();
    log::trace!(target: "hyperspace-light", "Latest height on chain_b: {:?}", height);
    let time = chain_b.query_timestamp_at(10).await.unwrap();
    log::trace!(target: "hyperspace-light", "Timestamp at height 10 on chain_b: {:?}", time);
    let channels = chain_b.query_channels().await.unwrap();
    log::trace!(target: "hyperspace-light", "Channels on chain_b: {:?}", channels);
    let (client_a, client_b) = create_clients(&chain_a, &chain_b).await.unwrap();
    
    chain_a.set_client_id(client_a);
    chain_b.set_client_id(client_b);
    (chain_a, chain_b)
}

/// This will set up a connection and ics20 channel in-between the two chains.
/// `connection_delay` should be in seconds.
pub async fn setup_connection_and_channel<A, B>(
    chain_a: &A,
    chain_b: &B,
    connection_delay: Duration,
) -> (JoinHandle<()>, ChannelId, ChannelId, ConnectionId)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
    A::Error: From<B::Error>,
    B: TestProvider,
    B::FinalityEvent: Send + Sync,
    B::Error: From<A::Error>,
{
    let client_a_clone = chain_a.clone();
    let client_b_clone = chain_b.clone();
    // Start relayer loop
    // let handle =
    //     tokio::task::spawn(async move { relay(client_a_clone, client_b_clone).await.unwrap() });

    // check if an open transfer channel exists
    let (latest_height, ..) = chain_a.latest_height_and_timestamp().await.unwrap();
    let connections = chain_a
        .query_connection_using_client(
            latest_height.revision_height() as u32,
            chain_b.client_id().to_string(),
        )
        .await
        .unwrap();
    log::info!(target: "hyperspace-light", "Connections: {:?}", connections);
    for connection in connections {
        let connection_id = ConnectionId::from_str(&connection.id).unwrap();
        let connection_end = chain_a
            .query_connection_end(latest_height, connection_id.clone())
            .await
            .unwrap()
            .connection
            .unwrap();

        let delay_period = Duration::from_nanos(connection_end.delay_period);
        if delay_period != connection_delay {
            continue;
        }

        let channels = chain_a
            .query_connection_channels(latest_height, &connection_id)
            .await
            .unwrap()
            .channels;
        log::info!(target: "hyperspace-light", "Channels: {:?}", channels);
        for channel in channels {
            let channel_id = ChannelId::from_str(&channel.channel_id).unwrap();
            let channel_end = chain_a
                .query_channel_end(latest_height, channel_id.clone(), PortId::transfer())
                .await
                .unwrap()
                .channel
                .unwrap();
            let channel_end = ChannelEnd::try_from(channel_end.clone()).unwrap();
            if channel_end.state == State::Open && channel.port_id == PortId::transfer().to_string()
            {
                todo!()
                // return (
                //     handle,
                //     channel_id,
                //     channel_end.counterparty().channel_id.as_ref().unwrap().clone(),
                //     channel_end.connection_hops[0].clone(),
                // );
            }
        }
    }
    let (connection_id, ..) = create_connection(chain_a, chain_b, connection_delay)
        .await
        .unwrap();

    log::info!(target: "hyperspace-light", "============ Connection handshake completed: ConnectionId({connection_id}) ============");
    log::info!(target: "hyperspace-light", "=========================== Starting channel handshake ===========================");

    let (channel_id_a, channel_id_b) = create_channel(
        chain_a,
        chain_b,
        connection_id.clone(),
        PortId::transfer(),
        VERSION.to_string(),
        Order::Unordered,
    )
    .await
    .unwrap();
    // channel handshake completed
    log::info!(target: "hyperspace-light", "============ Channel handshake completed: ChannelId({channel_id_a}) ============");
    todo!()
    // (handle, channel_id_a, channel_id_b, connection_id)
}
