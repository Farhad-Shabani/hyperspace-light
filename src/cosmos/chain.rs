use super::client::CosmosClient;
use super::provider::TransactionId;
use crate::core::error::Error;
use crate::core::primitives::{Chain, IbcProvider};
use crate::cosmos::provider::FinalityEvent;
use futures::{Stream, StreamExt};
use ibc_proto::cosmos::base::v1beta1::Coin;
use ibc_proto::{
    cosmos::tx::v1beta1::{
        mode_info::{Single, Sum},
        AuthInfo, Fee, ModeInfo, SignDoc, SignerInfo, TxBody, TxRaw,
    },
    google::protobuf::Any,
};
use k256::ecdsa::{signature::Signer as _, Signature, SigningKey};
use prost::Message;
use std::pin::Pin;
use tendermint_rpc::{
    event::Event,
    event::EventData,
    query::{EventType, Query},
    Client, Error as RpcError, SubscriptionClient, WebSocketClient,
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
        todo!()
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
        // Create SignerInfo by encoding the keybase
        let account_info = self.query_account().await?;

        let mut pk_buf = Vec::new();
        Message::encode(&self.keybase.public_key.to_pub().to_bytes(), &mut pk_buf)
            .map_err(|e| Error::from(e.to_string()))?;

        let pk_type = "/cosmos.crypto.secp256k1.PubKey".to_string();
        let pk_any = Any {
            type_url: pk_type,
            value: pk_buf,
        };
        let single = Single { mode: 1 };
        let sum_single = Some(Sum::Single(single));
        let mode = Some(ModeInfo { sum: sum_single });
        let signer_info = SignerInfo {
            public_key: Some(pk_any),
            mode_info: mode,
            sequence: account_info.sequence,
        };

        // Create and Encode TxBody
        let body = TxBody {
            messages,
            memo: "ibc".to_string(), //TODO: Check if this is correct
            timeout_height: 0_u64,
            extension_options: Vec::<Any>::default(), //TODO: Check if this is correct
            non_critical_extension_options: Vec::<Any>::default(),
        };
        let mut body_bytes = Vec::new();
        Message::encode(&body, &mut body_bytes).map_err(|e| Error::from(e.to_string()))?;

        // Create and Encode AuthInfo
        let auth_info = AuthInfo {
            signer_infos: vec![signer_info],
            fee: Some(Fee {
                amount: vec![Coin {
                    denom: "stake".to_string(),
                    amount: "4000".to_string(),
                }],
                gas_limit: 400000_u64,
                payer: "".to_string(),
                granter: "".to_string(),
            }),
            tip: None,
        };
        let mut auth_info_bytes = Vec::new();
        Message::encode(&auth_info, &mut auth_info_bytes)
            .map_err(|e| Error::from(e.to_string()))?;

        // Create and Encode SignDoc
        let sign_doc = SignDoc {
            body_bytes: body_bytes.clone(),
            auth_info_bytes: auth_info_bytes.clone(),
            chain_id: self.chain_id.to_string(),
            account_number: account_info.account_number,
        };
        let mut signdoc_buf = Vec::new();
        Message::encode(&sign_doc, &mut signdoc_buf).unwrap();

        // Create signature
        let private_key_bytes = self.keybase.private_key.to_priv().to_bytes();
        let signing_key = SigningKey::from_bytes(private_key_bytes.as_slice())
            .map_err(|e| Error::from(e.to_string()))?;
        let signature: Signature = signing_key.sign(&signdoc_buf);
        let signature_bytes = signature.as_ref().to_vec();

        // Create and Encode TxRaw
        let tx_raw = TxRaw {
            body_bytes,
            auth_info_bytes,
            signatures: vec![signature_bytes.clone()],
        };
        let mut tx_bytes = Vec::new();
        Message::encode(&tx_raw, &mut tx_bytes).map_err(|e| Error::from(e.to_string()))?;

        // ------------------ Simulate transaction ------------------
        use ibc_proto::cosmos::tx::v1beta1::service_client::ServiceClient;
        use ibc_proto::cosmos::tx::v1beta1::{SimulateRequest, Tx};
        use ibc_proto::google::protobuf::Any;

        use std::convert::{From, Into};
        let tx = Tx {
            body: Some(body),
            auth_info: Some(auth_info),
            signatures: vec![signature_bytes],
        };

        #[allow(deprecated)]
        let req = SimulateRequest {
            tx: Some(tx), // needed for simulation to go through with Cosmos SDK <  0.43
            tx_bytes: tx_bytes.clone(), // needed for simulation to go through with Cosmos SDk >= 0.43
        };

        let mut client = ServiceClient::connect(self.grpc_url.clone().to_string())
            .await
            .map_err(|e| Error::from(e.to_string()))?;
        let request = tonic::Request::new(req);
        let response = client
            .simulate(request)
            .await
            .map_err(|e| Error::from(e.to_string()))?
            .into_inner();
        log::info!(target: "demo-relayer", "Simulate response: {:?}", response);
        // -----------------------------------------------------------

        // Submit transaction
        let response = self
            .rpc_client
            .broadcast_tx_sync(tx_bytes.into())
            .await
            .map_err(|e| Error::from(format!("failed to broadcast transaction {:?}", e)))?;
        log::info!(target:"demo-relayer", "Broadcast response: {:?}", response);
        Ok(TransactionId {
            hash: response.hash,
        })
    }
}
