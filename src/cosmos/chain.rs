use super::client::CosmosClient;
use super::tx::{broadcast_tx, confirm_tx, sign_tx, simulate_tx};
use crate::core::error::Error;
use crate::core::primitives::{Chain, IbcProvider};
use crate::cosmos::provider::FinalityEvent;
use futures::{Stream, StreamExt};
use ibc_proto::cosmos::base::v1beta1::Coin;
use ibc_proto::{cosmos::tx::v1beta1::Fee, google::protobuf::Any};
use prost::Message;
use std::pin::Pin;
use tendermint_rpc::{
    event::Event,
    event::EventData,
    query::{EventType, Query},
    SubscriptionClient, WebSocketClient,
};

#[async_trait::async_trait]
impl<H> Chain for CosmosClient<H>
where
    H: Clone + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        &*self.name
    }

    fn block_max_weight(&self) -> u64 {
        self.max_tx_size as u64
    }

    async fn estimate_weight(&self, messages: Vec<Any>) -> Result<u64, Self::Error> {
        let account_info = self.query_account().await?;
        let fee = Fee {
            amount: vec![Coin {
                denom: "stake".to_string(),
                amount: "4000".to_string(),
            }],
            gas_limit: 400000_u64,
            payer: "".to_string(),
            granter: "".to_string(),
        };
        let (_, tx_raw, _) = sign_tx(
            self.keybase.clone(),
            self.chain_id.clone(),
            &account_info,
            vec![],
            fee,
        )?;

        let total_len = tx_raw.encoded_len();
        let body_bytes_len = tx_raw.body_bytes.len();
        let envelope_len = if body_bytes_len == 0 {
            total_len
        } else {
            total_len - 1 - prost::length_delimiter_len(body_bytes_len) - body_bytes_len
        };
        // Full length of the transaction can then be derived from the length of the invariable
        // envelope and the length of the body field, taking into account the varint encoding
        // of the body field's length delimiter.
        fn tx_len(envelope_len: usize, body_len: usize) -> usize {
            // The caller has at least one message field length added to the body's
            debug_assert!(body_len != 0);
            envelope_len + 1 + prost::length_delimiter_len(body_len) + body_len
        }

        let mut current_count = 0;
        let mut current_len = body_bytes_len;

        for message in messages {
            let message_len = message.encoded_len();

            // The total length the message adds to the encoding includes the
            // field tag (small varint) and the length delimiter.
            let tagged_len = 1 + prost::length_delimiter_len(message_len) + message_len;
            if current_count >= 30
                || tx_len(envelope_len, current_len + tagged_len) > self.max_tx_size
            {
                return Err(Error::Custom("Too many messages".to_string()));
            }
            current_count += 1;
            current_len += tagged_len;
            log::info!(
                "Message {} has length {} ({} + {})",
                current_count,
                tagged_len,
                message_len,
                1 + prost::length_delimiter_len(message_len)
            );
        }
        Ok(current_len as u64)
    }

    async fn finality_notifications(
        &self,
    ) -> Pin<Box<dyn Stream<Item = <Self as IbcProvider>::FinalityEvent> + Send + Sync>> {
        let (ws_client, ws_driver) = WebSocketClient::new(self.websocket_url.clone())
            .await
            .map_err(|e| Error::from(format!("Web Socket Client Error {:?}", e)))
            .unwrap();
        tokio::spawn(ws_driver.run());
        let subscription = ws_client
            .subscribe(Query::from(EventType::NewBlock))
            .await
            .map_err(|e| Error::from(format!("failed to subscribe to new blocks {:?}", e)))
            .unwrap();
        log::info!(target: "hyperspace-light", "🛰️ Subscribed to {} listening to finality notifications", self.name);
        let stream = subscription.filter_map(|event| {
            let Event {
                data,
                events,
                query,
            } = event
                .map_err(|e| Error::from(format!("failed to get event {:?}", e)))
                .unwrap();
            let header = match data {
                EventData::NewBlock { block, .. } => block.unwrap().header,
                _ => unreachable!(),
            };
            futures::future::ready(Some(FinalityEvent::Tendermint(header)))
        });

        Box::pin(stream)
    }

    async fn submit(&self, messages: Vec<Any>) -> Result<Self::TransactionId, Error> {
        let account_info = self.query_account().await?;

        // Sign transaction
        let (tx, _, tx_bytes) = sign_tx(
            self.keybase.clone(),
            self.chain_id.clone(),
            &account_info,
            messages,
            Fee {
                amount: vec![Coin {
                    denom: "stake".to_string(), //TODO: This could be added to the config
                    amount: "4000".to_string(), //TODO: This could be added to the config
                }],
                gas_limit: 400000_u64, //TODO: This could be added to the config
                payer: "".to_string(),
                granter: "".to_string(),
            },
        )?;

        // Simulate transaction
        let _ = simulate_tx(self.grpc_url.clone(), tx, tx_bytes.clone()).await?;

        // Broadcast transaction
        let tx_id = broadcast_tx(&self.rpc_client, tx_bytes).await?;

        // wait for confirmation
        confirm_tx(&self.rpc_client, tx_id.hash).await
    }
}
