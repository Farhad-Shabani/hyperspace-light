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
#![allow(clippy::all)]
use super::key_provider::KeyEntry;
use super::light_client::LightClient;
use super::tx::{broadcast_tx, confirm_tx, sign_tx, simulate_tx};
use crate::core::error::Error;
use crate::core::primitives::{IbcProvider, KeyProvider, UpdateType};
use core::convert::{From, Into, TryFrom};
use ibc_proto::cosmos::auth::v1beta1::{
    query_client::QueryClient, BaseAccount, QueryAccountRequest,
};
use ibc_proto::cosmos::base::v1beta1::Coin;
use ibc_proto::cosmos::tx::v1beta1::Fee;
use ibc_proto::google::protobuf::Any;
use ibc_relayer_types::{
    clients::ics07_tendermint::{
        client_state::ClientState, consensus_state::ConsensusState, header::Header,
    },
    core::{
        ics02_client::{client_type::ClientType, height::Height},
        ics23_commitment::{
            commitment::{CommitmentPrefix, CommitmentProofBytes},
            merkle::convert_tm_to_ics_merkle_proof,
        },
        ics24_host::{
            identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId},
            IBC_QUERY_PATH,
        },
    },
    keys::STORE_KEY,
};
use prost::Message;
use serde::Deserialize;
use tendermint::Hash;
use tendermint::{abci::Code, block::Height as TmHeight};
use tendermint_light_client::components::io::{AtHeight, Io};
use tendermint_rpc::{endpoint::abci_query::AbciQuery, Client, HttpClient, Url};

// Implements the [`crate::Chain`] trait for cosmos.
/// This is responsible for:
/// 1. Tracking a cosmos light client on a counter-party chain, advancing this light
/// client state  as new finality proofs are observed.
/// 2. Submiting new IBC messages to this cosmos.
#[derive(Clone)]
pub struct CosmosClient<H> {
    /// Chain name
    pub name: String,
    /// Chain rpc client
    pub rpc_client: HttpClient,
    /// Chain grpc address
    pub grpc_url: Url,
    /// Websocket chain ws client
    pub websocket_url: Url,
    /// Chain Id
    pub chain_id: ChainId,
    /// Light client id on counterparty chain
    pub client_id: Option<ClientId>,
    /// Connection Id
    pub connection_id: Option<ConnectionId>,
    /// Light Client instance
    pub light_client: LightClient,
    /// The key that signs transactions
    pub keybase: KeyEntry,
    /// Account prefix
    pub account_prefix: String,
    /// Reference to commitment
    pub commitment_prefix: CommitmentPrefix,
    /// Maximun transaction size
    pub max_tx_size: usize,
    /// Channels cleared for packet relay
    pub channel_whitelist: Vec<(ChannelId, PortId)>,
    /// Finality protocol to use, eg Tenderminet
    pub _phantom: std::marker::PhantomData<H>,
}
/// config options for [`ParachainClient`]
#[derive(Debug, Deserialize)]
pub struct CosmosClientConfig {
    /// Chain name
    pub name: String,
    /// rpc url for cosmos
    pub rpc_url: Url,
    /// grpc url for cosmos
    pub grpc_url: Url,
    /// websocket url for cosmos
    pub websocket_url: Url,
    /// Cosmos chain Id
    pub chain_id: String,
    /// Light client id on counterparty chain
    pub client_id: Option<String>,
    /// Connection Id
    pub connection_id: Option<String>,
    /// Account prefix
    pub account_prefix: String,
    /// Store prefix
    pub store_prefix: String,
    /// Maximun transaction size
    pub max_tx_size: usize,
    /// The key that signs transactions
    pub keybase: KeyEntry,
    /*
    Here is a list of dropped configuration parameters from Hermes Config.toml
    that could be set to default values or removed for the MVP phase:

    ub key_store_type: Store,					//TODO: Could be set to any of SyncCryptoStorePtr or KeyStore or KeyEntry types, but not sure yet
    pub rpc_timeout: Duration,				    //TODO: Could be set to '15s' by default
    pub default_gas: Option<u64>,	  			//TODO: Could be set to `0` by default
    pub max_gas: Option<u64>,                   //TODO: DEFAULT_MAX_GAS: u64 = 400_000
    pub gas_multiplier: Option<GasMultiplier>,  //TODO: Could be set to `1.1` by default
    pub fee_granter: Option<String>,            //TODO: DEFAULT_FEE_GRANTER: &str = ""
    pub max_msg_num: MaxMsgNum,                 //TODO: Default is 30, Could be set usize = 1 for test
                                                //TODO: Could be set to const MAX_LEN: usize = 50;
    pub proof_specs: Option<ProofSpecs>,        //TODO: Could be set to None
    pub sequential_batch_tx: bool,			    //TODO: sequential_send_batched_messages_and_wait_commit() or send_batched_messages_and_wait_commit() ?
    pub trust_threshold: TrustThreshold,
    pub gas_price: GasPrice,   				    //TODO: Could be set to `0`
    pub packet_filter: PacketFilter,            //TODO: AllowAll
    pub address_type: AddressType,			    //TODO: Type = cosmos
    pub extension_options: Vec<ExtensionOption>,//TODO: Could be set to None
    */
}

impl<H> CosmosClient<H>
where
    Self: KeyProvider,
    H: Clone + Send + Sync + 'static,
{
    /// Initializes a [`CosmosClient`] given a [`CosmosClientConfig`]
    pub async fn new(config: CosmosClientConfig) -> Result<Self, Error> {
        let rpc_client = HttpClient::new(config.rpc_url.clone())
            .map_err(|e| Error::RpcError(format!("{:?}", e)))?;
        let chain_id = ChainId::from(config.chain_id);
        let client_id = Some(
            ClientId::new(ClientType::Tendermint, 0)
                .map_err(|e| Error::from(format!("Invalid client id {:?}", e)))?,
        );
        let light_client = LightClient::init_light_client(config.rpc_url.clone()).await?;
        let commitment_prefix = CommitmentPrefix::try_from(config.store_prefix.as_bytes().to_vec())
            .map_err(|e| Error::from(format!("Invalid store prefix {:?}", e)))?;

        Ok(Self {
            name: config.name,
            chain_id,
            rpc_client,
            grpc_url: config.grpc_url,
            websocket_url: config.websocket_url,
            client_id,
            connection_id: None,
            light_client,
            account_prefix: config.account_prefix,
            commitment_prefix,
            max_tx_size: config.max_tx_size,
            keybase: config.keybase,
            channel_whitelist: vec![],
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id.as_ref().unwrap().clone()
    }

    pub fn set_client_id(&mut self, client_id: ClientId) {
        self.client_id = Some(client_id)
    }

    /// Construct a tendermint client state to be submitted to the counterparty chain
    pub async fn construct_tendermint_client_state(
        &self,
    ) -> Result<(ClientState, ConsensusState), Error>
    where
        Self: KeyProvider + IbcProvider,
        H: Clone + Send + Sync + 'static,
    {
        self.initialize_client_state().await.map_err(|e| {
            Error::from(format!(
                "Failed to initialize client state for chain {:?} with error {:?}",
                self.name, e
            ))
        })
    }

    pub async fn submit_create_client_msg(&self, _msg: String) -> Result<ClientId, Error> {
        todo!()
    }

    pub async fn transfer_tokens(&self, _asset_id: u128, _amount: u128) -> Result<(), Error> {
        todo!()
    }

    pub async fn submit_call(&self, messages: Vec<Any>) -> Result<Hash, Error> {
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
        // let res = simulate_tx(self.grpc_url.clone(), tx, tx_bytes.clone()).await?;
        // Broadcast transaction
        let hash = broadcast_tx(&self.rpc_client, tx_bytes).await?;
        log::info!("Transaction sent with hash: {:?}", hash);

        // wait for confirmation
        confirm_tx(&self.rpc_client, hash).await
    }

    pub async fn msg_update_client_header(
        &self,
        trusted_height: Height,
    ) -> Result<(Header, UpdateType), Error> {
        let latest_light_block = self
            .light_client
            .io
            .fetch_light_block(AtHeight::Highest)
            .map_err(|e| {
                Error::from(format!(
                    "Failed to fetch light block for chain {:?} with error {:?}",
                    self.name, e
                ))
            })?;
        let height = TmHeight::try_from(trusted_height.revision_height()).map_err(|e| {
            Error::from(format!(
                "Failed to convert height for chain {:?} with error {:?}",
                self.name, e
            ))
        })?;
        let trusted_light_block = self
            .light_client
            .io
            .fetch_light_block(AtHeight::At(height.increment()))
            .map_err(|e| {
                Error::from(format!(
                    "Failed to fetch light block for chain {:?} with error {:?}",
                    self.name, e
                ))
            })?;

        let update_type = match latest_light_block.validators == latest_light_block.next_validators
        {
            true => UpdateType::Optional,
            false => UpdateType::Optional,
        };
        Ok((
            Header {
                signed_header: latest_light_block.signed_header,
                validator_set: latest_light_block.validators,
                trusted_height,
                trusted_validator_set: trusted_light_block.validators,
            },
            update_type,
        ))
    }

    /// Uses the GRPC client to retrieve the account sequence
    pub async fn query_account(&self) -> Result<BaseAccount, Error> {
        let mut client = QueryClient::connect(self.grpc_url.clone().to_string())
            .await
            .map_err(|e| Error::from(format!("GRPC client error: {:?}", e)))?;

        let request = tonic::Request::new(QueryAccountRequest {
            address: self.keybase.account.to_string(),
        });

        let response = client.account(request).await;

        // Querying for an account might fail, i.e. if the account doesn't actually exist
        let resp_account = match response
            .map_err(|e| Error::from(format!("{:?}", e)))?
            .into_inner()
            .account
        {
            Some(account) => account,
            None => return Err(Error::from(format!("Account not found"))),
        };

        Ok(BaseAccount::decode(resp_account.value.as_slice())
            .map_err(|e| Error::from(format!("Failed to decode account {}", e)))?)
    }

    pub async fn query_path(
        &self,
        data: Vec<u8>,
        height_query: Height,
        prove: bool,
    ) -> Result<(AbciQuery, Vec<u8>), Error> {
        // SAFETY: Creating a Path from a constant; this should never fail
        let path = IBC_QUERY_PATH.into();
        let height = TmHeight::try_from(height_query.revision_height())
            .map_err(|e| Error::from(format!("Invalid height {}", e)))?;

        let height = match height.value() {
            0 => None,
            _ => Some(height),
        };

        // Use the Tendermint-rs RPC client to do the query.
        let response = self
            .rpc_client
            .abci_query(Some(path), data, height, prove)
            .await
            .map_err(|e| {
                Error::from(format!(
                    "Failed to query chain {} with error {:?}",
                    self.name, e
                ))
            })?;

        if !response.code.is_ok() {
            // Fail with response log.
            return Err(Error::from(format!(
                "Query failed with code {:?} and log {:?}",
                response.code, response.log
            )));
        }

        if prove && response.proof.is_none() {
            // Fail due to empty proof
            return Err(Error::from(format!(
                "Query failed due to empty proof for chain {}",
                self.name
            )));
        }

        let merkle_proof = response
            .clone()
            .proof
            .map(|p| convert_tm_to_ics_merkle_proof(&p))
            .transpose()
            .map_err(|_| Error::Custom(format!("bad client state proof")))?;
        // log::info!("Merkle proof: {:?}", merkle_proof);
        let proof = CommitmentProofBytes::try_from(merkle_proof.unwrap())
            .map_err(|err| Error::Custom(format!("bad client state proof: {}", err)))?;
        Ok((response, proof.into()))
    }
}
