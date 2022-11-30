use super::client::CosmosClient;
use crate::core::primitives::TestProvider;
use core::pin::Pin;
use futures::Stream;
use ibc_relayer_types::{
    applications::transfer::{msgs::transfer::MsgTransfer, PrefixedCoin},
    core::ics24_host::identifier::{ChannelId, PortId},
};

#[async_trait::async_trait]

impl<H> TestProvider for CosmosClient<H>
where
    H: Clone + Send + Sync + 'static,
{
    /// Initiate an ibc transfer on chain.
    async fn send_transfer(&self, params: MsgTransfer<PrefixedCoin>) -> Result<(), Self::Error> {
        todo!()
    }

    /// Send a packet on an ordered channel
    async fn send_ordered_packet(
        &self,
        channel_id: ChannelId,
        timeout: u64,
    ) -> Result<(), Self::Error> {
        todo!()
    }

    /// Returns a stream that yields chain Block number
    async fn subscribe_blocks(&self) -> Pin<Box<dyn Stream<Item = u64> + Send + Sync>> {
        todo!()
    }

    /// Set the channel whitelist for the relayer task.
    fn set_channel_whitelist(&mut self, channel_whitelist: Vec<(ChannelId, PortId)>) {
        todo!()
    }
}
