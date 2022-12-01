use hyperspace_light::core::logging;
use hyperspace_light::testsuite::{tests::ibc_messaging_with_connection_delay, setup::setup_clients};

#[tokio::test(flavor = "multi_thread")]
async fn cosmos_cosmos() {
    logging::setup_logging();
    let (mut chain_a, mut chain_b) = setup_clients::<()>().await;
    // Run tests sequentially

    // no timeouts + connection delay
    ibc_messaging_with_connection_delay(&mut chain_a, &mut chain_b).await;

    // // timeouts + connection delay
    // ibc_messaging_packet_height_timeout_with_connection_delay(&mut chain_a, &mut chain_b).await;
    // ibc_messaging_packet_timestamp_timeout_with_connection_delay(&mut chain_a, &mut
    // chain_b).await;

    // // channel closing semantics
    // ibc_messaging_packet_timeout_on_channel_close(&mut chain_a, &mut chain_b).await;
    // ibc_channel_close(&mut chain_a, &mut chain_b).await;
}
