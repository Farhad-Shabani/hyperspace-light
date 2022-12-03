use super::provider::TransactionId;
use crate::core::error::Error;
use core::time::Duration;
use ibc_proto::cosmos::tx::v1beta1::service_client::ServiceClient;
use ibc_proto::cosmos::tx::v1beta1::{SimulateRequest, SimulateResponse, Tx};
use tendermint::abci::transaction::Hash;
use tendermint_rpc::endpoint::tx::Response as TxResponse;
use tendermint_rpc::query::Query;
use tendermint_rpc::{Client, HttpClient, Order, Url};

pub async fn simulate_tx(
    grpc_url: Url,
    tx: Tx,
    tx_bytes: Vec<u8>,
) -> Result<SimulateResponse, Error> {
    #[allow(deprecated)]
    let req = SimulateRequest {
        tx: Some(tx), // needed for simulation to go through with Cosmos SDK <  0.43
        tx_bytes,     // needed for simulation to go through with Cosmos SDk >= 0.43
    };

    let mut client = ServiceClient::connect(grpc_url.clone().to_string())
        .await
        .map_err(|e| Error::from(e.to_string()))?;
    let request = tonic::Request::new(req);
    let response = client
        .simulate(request)
        .await
        .map_err(|e| Error::from(e.to_string()))?
        .into_inner();

    Ok(response)
}

pub async fn broadcast_tx(
    rpc_client: &HttpClient,
    tx_bytes: Vec<u8>,
) -> Result<TransactionId<Hash>, Error> {
    let response = rpc_client
        .broadcast_tx_sync(tx_bytes.into())
        .await
        .map_err(|e| Error::from(format!("failed to broadcast transaction {:?}", e)))?;
    Ok(TransactionId {
        hash: response.hash,
    })
}

pub async fn confirm_tx(
    rpc_client: &HttpClient,
    tx_hash: Hash,
) -> Result<TransactionId<Hash>, Error> {
    let start_time = tokio::time::Instant::now();
    let timeout = Duration::from_millis(30000);
    const WAIT_BACKOFF: Duration = Duration::from_millis(300);
    let response: TxResponse = loop {
        let response = rpc_client
            .tx_search(
                Query::eq("tx.hash", tx_hash.to_string()),
                false,
                1,
                1, // get only the first Tx matching the query
                Order::Ascending,
            )
            .await
            .map_err(|e| Error::from(format!("failed to search for transaction {:?}", e)))?;
        match response.txs.into_iter().next() {
            None => {
                let elapsed = start_time.elapsed();
                if &elapsed > &timeout {
                    return Err(Error::from(format!(
                        "transaction {} not found after {} seconds",
                        tx_hash,
                        elapsed.as_secs()
                    )));
                } else {
                    tokio::time::sleep(WAIT_BACKOFF).await;
                }
            }
            Some(response) => {
                break response;
            }
        }
    };

    let response_code = response.tx_result.code;
    if response_code.is_err() {
        return Err(Error::from(format!(
            "transaction {} failed with code {:?}",
            tx_hash, response_code
        )));
    }

    Ok(TransactionId {
        hash: response.hash,
    })
}
