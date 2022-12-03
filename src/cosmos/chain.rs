use super::client::CosmosClient;
use super::encode::{
    encode_auth_info, encode_key_bytes, encode_sign_doc, encode_signer_info, encode_tx,
    encode_tx_body,
};
use super::tx::{broadcast_tx, confirm_tx, simulate_tx};
use crate::core::error::Error;
use crate::core::primitives::{Chain, IbcProvider};
use crate::cosmos::provider::FinalityEvent;
use futures::{Stream, StreamExt};
use ibc_proto::cosmos::base::v1beta1::Coin;
use ibc_proto::{cosmos::tx::v1beta1::Fee, google::protobuf::Any};
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
        2097152
    }

    async fn estimate_weight(&self, _messages: Vec<Any>) -> Result<u64, Self::Error> {
        todo!()
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
        let pk_bytes = encode_key_bytes(&self.keybase)?;
        let signer_info = encode_signer_info(account_info.sequence, pk_bytes)?;

        // Create and Encode AuthInfo
        let (auth_info, auth_info_bytes) = encode_auth_info(
            signer_info,
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

        // Create and Encode TxBody
        let (body, body_bytes) = encode_tx_body(messages)?;

        // Create and Encode TxRaw
        let signature_bytes = encode_sign_doc(
            self.keybase.clone(),
            body_bytes.clone(),
            auth_info_bytes.clone(),
            self.chain_id.clone(),
            account_info.account_number,
        )?;

        // Encode SignDoc and Create Signature
        let (tx, tx_bytes) = encode_tx(
            body,
            body_bytes,
            auth_info,
            auth_info_bytes,
            signature_bytes,
        )?;

        // Simulate transaction
        let _ = simulate_tx(self.grpc_url.clone(), tx, tx_bytes.clone()).await?;
        
        // Broadcast transaction
        let tx_id = broadcast_tx(&self.rpc_client, tx_bytes).await?;

        // wait for confirmation
        confirm_tx(&self.rpc_client, tx_id.hash).await
    }
}
