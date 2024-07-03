use pallas::{
    codec::{
        minicbor::{decode, encode},
        utils::KeepRaw,
    },
    ledger::{
        addresses::Address,
        primitives::{
            babbage::{PseudoScript, PseudoTransactionOutput},
            conway::{
                NativeScript, PlutusData, PseudoDatumOption, PseudoPostAlonzoTransactionOutput,
            },
        },
        traverse::MultiEraTx,
    },
};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

use crate::{model::hydra::utxo::UTxO, SCRIPT_ADDRESS};

use hex::FromHex;

use super::{
    game_state::GameState,
    hydra::{
        hydra_message::HydraData,
        hydra_socket::HydraSocket,
        messages::{new_tx::NewTx, tx_valid::TxValid},
    },
    player::Player,
    tx_builder::TxBuilder,
};

#[derive(Clone)]
pub struct Node {
    pub connection_info: ConnectionInfo,
    pub head_id: Option<String>,
    pub socket: HydraSocket,
    pub players: Vec<Player>,
    pub stats: NodeStats,
    pub tx_builder: TxBuilder,
}

#[derive(Clone)]
pub struct ConnectionInfo {
    pub host: String,
    pub port: u32,
    pub secure: bool,
}
pub struct NodeSummary(pub Node);

#[derive(Clone)]
pub struct NodeStats {
    pub persisted: bool,
    pub transactions: u64,
    pub bytes: u64,
    pub kills: u64,
    pub items: u64,
    pub secrets: u64,
    pub play_time: u64,
    pub pending_transactions: HashMap<Vec<u8>, StateUpdate>,
}

#[derive(Clone)]
pub struct StateUpdate {
    pub bytes: u64,
    pub kills: u64,
    pub items: u64,
    pub secrets: u64,
    pub play_time: u64,
}

#[derive(Debug)]
pub enum NetworkRequestError {
    HttpError(reqwest::Error),
    DeserializationError(Box<dyn std::error::Error>),
}

impl Node {
    pub async fn try_new(
        uri: &str,
        writer: &UnboundedSender<HydraData>,
        persisted: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let connection_info: ConnectionInfo = uri.to_string().try_into()?;

        let socket = HydraSocket::new(connection_info.to_websocket_url().as_str(), writer).await?;
        let mut node = Node {
            connection_info,
            head_id: None,
            players: Vec::new(),
            socket,
            stats: NodeStats::new(persisted),
            tx_builder: TxBuilder::new(
                <[u8; 32]>::from_hex(
                    "AF9292ADA4AA01DB918BBBA7796ACF235E6D87D3EBC0D93FA44AA7E0531CF226",
                )
                .unwrap(),
            ),
        };

        node.listen();
        let utxos = node
            .fetch_utxos()
            .await
            .map_err(|_| "Failed to fetch UTxOs")?;
        let maybe_script_ref = TxBuilder::find_script_ref(utxos);
        match maybe_script_ref {
            Some(script_ref) => {
                let _ = node.tx_builder.set_script_ref(&script_ref);
                println!("Set script ref! {:?}", script_ref);
            }
            None => {
                println!("No script ref found for this node.");
            }
        }
        Ok(node)
    }

    pub async fn add_player(&mut self, player: Player) -> Result<(), Box<dyn std::error::Error>> {
        let utxos = self
            .fetch_utxos()
            .await
            .map_err(|_| "Failed to fetch utxos")?;

        let new_game_tx = self.tx_builder.build_new_game_state(&player, utxos)?;

        let message: String = NewTx::new(new_game_tx)?.into();

        self.players.push(player);
        self.send(message);

        Ok(())
    }

    pub fn listen(&self) {
        let receiver = self.socket.receiver.clone();
        let identifier = self.connection_info.to_authority();
        tokio::spawn(async move { receiver.lock().await.listen(identifier.as_str()).await });
    }

    pub fn send(&self, message: String) {
        let sender = self.socket.sender.clone();
        tokio::spawn(async move {
            let _ = sender.lock().await.send(HydraData::Send(message)).await;
        });
    }

    pub async fn fetch_utxos(&self) -> Result<Vec<UTxO>, NetworkRequestError> {
        let request_url = self.connection_info.to_http_url() + "/snapshot/utxo";
        let response = reqwest::get(&request_url)
            .await
            .map_err(NetworkRequestError::HttpError)?;

        let body = response
            .json::<HashMap<String, Value>>()
            .await
            .map_err(NetworkRequestError::HttpError)?;

        let utxos = body
            .iter()
            .map(|(key, value)| UTxO::try_from_value(key, value))
            .map(|result| result.map_err(|e| NetworkRequestError::DeserializationError(e)))
            .collect::<Result<Vec<UTxO>, NetworkRequestError>>()?;

        Ok(utxos)
    }

    pub fn add_transaction(
        &mut self,
        transaction: TxValid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bytes = transaction.cbor.as_slice();
        let tx = MultiEraTx::decode(bytes).map_err(|_| "Failed to decode transaction")?;

        let tx = tx.as_babbage().ok_or("Invalid babbage era tx")?;

        let outputs = &tx.transaction_body.outputs;
        let script_outputs = outputs
            .into_iter()
            .filter(|output| match output {
                PseudoTransactionOutput::PostAlonzo(output) => {
                    let bytes: Vec<u8> = output.address.clone().into();
                    let address = match Address::from_bytes(bytes.as_slice()) {
                        Ok(address) => address,
                        Err(_) => return false,
                    };
                    // unwrapping here because it came from hydra, so it is valid
                    let address = address.to_bech32().unwrap();

                    address.as_str() == SCRIPT_ADDRESS
                }
                _ => false,
            })
            .collect::<Vec<
                &PseudoTransactionOutput<
                    PseudoPostAlonzoTransactionOutput<
                        PseudoDatumOption<KeepRaw<PlutusData>>,
                        PseudoScript<KeepRaw<NativeScript>>,
                    >,
                >,
            >>();

        if script_outputs.len() != 1 {
            return Err("Invalid number of script outputs".into());
        }

        let script_output = script_outputs.first().unwrap();
        match script_output {
            PseudoTransactionOutput::PostAlonzo(output) => {
                if output.datum_option.is_none() {
                    return Err("No datum found".into());
                }

                let datum = match output.datum_option.as_ref().unwrap() {
                    PseudoDatumOption::Data(datum) => datum,
                    _ => return Err("No inline datum found".into()),
                }
                .0
                .raw_cbor();

                let data = match decode::<PlutusData>(datum) {
                    Ok(data) => data,
                    Err(_) => return Err("Failed to deserialize datum".into()),
                };

                let game_state: GameState = data.try_into()?;

                let player = match self
                    .players
                    .iter_mut()
                    .find(|player| player.pkh == game_state.admin)
                {
                    Some(player) => player,
                    None => return Err("No player found".into()),
                };

                let state_update =
                    player.generate_state_update(transaction.cbor.len() as u64, game_state);

                self.stats
                    .pending_transactions
                    .insert(transaction.tx_id, state_update);

                Ok(())
            }
            _ => return Err("Invalid output type".into()),
        }
    }
}

impl Serialize for Node {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("Node", 4)?;
        s.serialize_field("id", &self.head_id)?;
        s.serialize_field("total", &self.stats)?;
        // TODO: Make the active games count match the openapi schema
        s.serialize_field("active_games", &self.players.len())?;
        s.skip_field("socket")?;
        s.skip_field("ephemeral")?;
        s.skip_field("connection_info")?;
        s.end()
    }
}

impl TryFrom<String> for ConnectionInfo {
    type Error = Box<dyn std::error::Error>;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = value.split(':').collect();
        // default to secure connection if no schema provided
        match parts.len() {
            2 => {
                let host = parts[0].to_string();
                let port = parts[1].parse::<u32>()?;

                Ok(ConnectionInfo {
                    host,
                    port,
                    secure: true,
                })
            }
            3 => {
                let schema = parts[0].to_string();
                let port = parts[2].parse::<u32>()?;
                let host = parts[1]
                    .to_string()
                    .split("//")
                    .last()
                    .ok_or("Invalid host")?
                    .to_string();

                let secure = schema == "https" || schema == "wss";
                Ok(ConnectionInfo { host, port, secure })
            }
            _ => {
                return Err("Invalid uri".into());
            }
        }
    }
}

impl ConnectionInfo {
    pub fn to_websocket_url(&self) -> String {
        let schema = if self.secure { "wss" } else { "ws" };
        format!("{}://{}:{}", schema, self.host, self.port)
    }

    pub fn to_http_url(&self) -> String {
        let schema = if self.secure { "https" } else { "http" };
        format!("{}://{}:{}", schema, self.host, self.port)
    }

    pub fn to_authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
impl NodeStats {
    pub fn new(persisted: bool) -> NodeStats {
        NodeStats {
            persisted,
            transactions: 0,
            bytes: 0,
            kills: 0,
            items: 0,
            secrets: 0,
            play_time: 0,
            pending_transactions: HashMap::new(),
        }
    }

    pub fn calculate_stats(&mut self, confirmed_txs: Vec<Vec<u8>>) {
        for tx_id in confirmed_txs {
            match self.pending_transactions.remove(&tx_id) {
                Some(state_change) => self.update_stats(state_change),

                None => println!(
                    "Transaction in snapshot not found in stored transactions: {:?}",
                    tx_id
                ),
            }
        }
    }

    fn update_stats(&mut self, state_change: StateUpdate) {
        self.transactions += 1;
        self.bytes += state_change.bytes;
        self.kills += state_change.kills;
        self.items += state_change.items;
        self.secrets += state_change.secrets;
        self.play_time += state_change.play_time;
    }

    pub fn join(&self, other: NodeStats) -> NodeStats {
        let mut pending_transactions = self.pending_transactions.clone();
        pending_transactions.extend(other.pending_transactions);

        NodeStats {
            persisted: self.persisted && other.persisted,
            transactions: self.transactions + other.transactions,
            bytes: self.bytes + other.bytes,
            kills: self.kills + other.kills,
            items: self.items + other.items,
            secrets: self.secrets + other.secrets,
            play_time: self.play_time + other.play_time,
            pending_transactions,
        }
    }
}

impl Serialize for NodeStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("NodeStats", 6)?;
        s.serialize_field("transactions", &self.transactions)?;
        s.serialize_field("bytes", &self.bytes)?;
        s.serialize_field("kills", &self.kills)?;
        s.serialize_field("items", &self.items)?;
        s.serialize_field("secrets", &self.secrets)?;
        s.serialize_field("play_time", &self.play_time)?;
        s.skip_field("pending_transactions")?;
        s.end()
    }
}

impl Serialize for NodeSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("NodeSummary", 3)?;
        s.serialize_field("id", &self.0.head_id)?;
        s.serialize_field("active_games", &self.0.players.len())?;
        s.serialize_field("persisted", &self.0.stats.persisted)?;
        s.end()
    }
}
