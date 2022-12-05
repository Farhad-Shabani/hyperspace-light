#![allow(clippy::all)]
use super::create::timeout_future;
use super::setup::setup_connection_and_channel;
use crate::core::primitives::TestProvider;
use crate::core::relay::relay;
use futures::{future, StreamExt};
use ibc_proto::cosmos::base::v1beta1::Coin;
use ibc_relayer_types::{
    applications::transfer::msgs::transfer::MsgTransfer,
    core::{
        ics02_client::height::Height,
        ics04_channel::timeout::TimeoutHeight,
        ics24_host::identifier::{ChannelId, PortId},
    },
    events::IbcEvent,
    timestamp::Timestamp,
};
use std::time::Duration;

/// Send a packet over a connection with a connection delay and assert the sending chain only sees
/// the packet after the delay has elapsed.
pub async fn ibc_messaging_with_connection_delay<A, B>(chain_a: &mut A, chain_b: &mut B)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
    A::Error: From<B::Error>,
    B: TestProvider,
    B::FinalityEvent: Send + Sync,
    B::Error: From<A::Error>,
{
    let (handle, channel_id, channel_b, _connection_id) =
        setup_connection_and_channel(chain_a, chain_b, Duration::from_secs(60 * 5)).await; // 5 mins delay
    handle.abort();
    log::info!(target: "hyperspace-light", "🧹 Relay thread aborted");
    // Set channel whitelist and restart relayer loop
    chain_a.set_channel_whitelist(vec![(channel_id.clone(), PortId::transfer())]);
    chain_b.set_channel_whitelist(vec![(channel_b, PortId::transfer())]);
    let client_a_clone = chain_a.clone();
    let client_b_clone = chain_b.clone();
    let handle =
        tokio::task::spawn(async move { relay(client_a_clone, client_b_clone).await.unwrap() });
    send_packet_with_connection_delay(chain_a, chain_b, channel_id).await;
    log::info!(target: "hyperspace-light", "🧹 Relay thread aborted");
    handle.abort()
}

/// Simply send a packet and check that it was acknowledged after the connection delay.
async fn send_packet_with_connection_delay<A, B>(chain_a: &A, chain_b: &B, channel_id: ChannelId)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
    A::Error: From<B::Error>,
    B: TestProvider,
    B::FinalityEvent: Send + Sync,
    B::Error: From<A::Error>,
{
    let (previous_balance, ..) = send_transfer(chain_a, chain_b, channel_id.clone()).await;
    log::info!(target: "hyperspace-light", "Previous balance: {}", previous_balance);
    assert_send_transfer(chain_a, previous_balance, 20 * 60).await;
    // now send from chain b.
    let (previous_balance, ..) = send_transfer(chain_b, chain_a, channel_id).await;
    assert_send_transfer(chain_b, previous_balance, 20 * 60).await;
    log::info!(target: "hyperspace-light", "🙌🙌🙌 Token Transfer successful with connection delay");
}

/// Attempts to send 20% of funds of chain_a's signer to chain b's signer.
async fn send_transfer<A, B>(chain_a: &A, chain_b: &B, channel_id: ChannelId) -> (u128, MsgTransfer)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
    A::Error: From<B::Error>,
    B: TestProvider,
    B::FinalityEvent: Send + Sync,
    B::Error: From<A::Error>,
{
    let balance = chain_a
        .query_ibc_balance()
        .await
        .expect("Can't query ibc balance")
        .pop()
        .expect("No Ibc balances");
    let amount = parse_amount(balance.amount.to_string()) / 10;
    let coin = Coin {
        denom: balance.denom.to_string(),
        amount: amount.to_string(),
    };

    log::info!(target: "hyperspace-light", "📡 Sending {} {} from {} to {}", amount, coin.denom, chain_a.name(), chain_b.name());
    let (height_offset, time_offset) = (2000, 600 * 60);

    let (timeout_height, timestamp) = chain_b
        .latest_height_and_timestamp()
        .await
        .expect("Couldn't fetch latest_height_and_timestamp");

    let revision_height = timeout_height.revision_height() + height_offset;
    let timeout_timestamp =
        (timestamp + Duration::from_secs(time_offset)).expect("Overflow evaluating timeout");

    let msg = MsgTransfer {
        source_port: PortId::transfer(),
        source_channel: channel_id,
        token: coin,
        sender: chain_a.account_id(),
        receiver: chain_b.account_id(),
        timeout_height: TimeoutHeight::At(
            Height::new(timeout_height.revision_number(), revision_height).unwrap(),
        ),
        timeout_timestamp: Timestamp::from(timeout_timestamp),
    };
    chain_a
        .send_transfer(msg.clone())
        .await
        .expect("Failed to send transfer: ");
    (amount, msg)
}

async fn assert_send_transfer<A>(chain: &A, previous_balance: u128, wait_time: u64)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
{
    // wait for the acknowledgment
    let future = chain
        .ibc_events()
        .await
        .skip_while(|ev| future::ready(!matches!(ev.event, IbcEvent::AcknowledgePacket(_))))
        .take(1)
        .collect::<Vec<_>>();
    timeout_future(
        future,
        wait_time,
        format!("Didn't see AcknowledgePacket on {}", chain.name()),
    )
    .await;

    let balance = chain
        .query_ibc_balance()
        .await
        .expect("Can't query ibc balance")
        .pop()
        .expect("No Ibc balances");

    let new_amount = parse_amount(balance.amount.to_string());
    log::info!(target: "hyperspace-light", "🧾 Balance on {} is {}", chain.name(), new_amount);
    assert!(new_amount <= (previous_balance * 90) / 100);
}

/// Send a packet using a height timeout that has already passed
/// and assert the sending chain sees the timeout packet.
async fn send_packet_and_assert_height_timeout<A, B>(
    chain_a: &A,
    chain_b: &B,
    channel_id: ChannelId,
) where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
    A::Error: From<B::Error>,
    B: TestProvider,
    B::FinalityEvent: Send + Sync,
    B::Error: From<A::Error>,
{
    log::info!(target: "hyperspace-light", "Suspending send packet relay");

    let (.., msg) = send_transfer(chain_a, chain_b, channel_id).await;

    // Wait for timeout height to elapse then resume packet relay
    let future = chain_b
        .subscribe_blocks()
        .await
        .skip_while(|block_number| {
            future::ready(*block_number < msg.timeout_height.commitment_revision_height())
        })
        .take(1)
        .collect::<Vec<_>>();

    log::info!(target: "hyperspace-light", "Waiting for packet timeout to elapse on counterparty");
    timeout_future(
        future,
        10 * 60,
        format!("Timeout height was not reached on {}", chain_b.name()),
    )
    .await;

    log::info!(target: "hyperspace-light", "Resuming send packet relay");
    assert_timeout_packet(chain_a).await;
    log::info!(target: "hyperspace-light", "🚀🚀 Timeout packet successfully processed for height timeout");
}

pub fn parse_amount(amount: String) -> u128 {
    str::parse::<u128>(&amount).expect("Failed to parse as u128")
}

pub async fn assert_timeout_packet<A>(chain: &A)
where
    A: TestProvider,
    A::FinalityEvent: Send + Sync,
{
    // wait for the timeout packet
    let future = chain
        .ibc_events()
        .await
        .skip_while(|ev| {
            future::ready(!matches!(
                ev.event,
                IbcEvent::TimeoutPacket(_) | IbcEvent::TimeoutOnClosePacket(_)
            ))
        })
        .take(1)
        .collect::<Vec<_>>();
    timeout_future(
        future,
        20 * 60,
        format!("Didn't see Timeout packet on {}", chain.name()),
    )
    .await;
}
