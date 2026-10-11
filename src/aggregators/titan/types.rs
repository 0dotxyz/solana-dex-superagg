//! Titan WebSocket API messages (MessagePack, structs encoded as maps). Only the fields we use;
//! the rest of a server message is ignored.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type Pubkey = [u8; 32];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountMeta {
    pub p: Pubkey, // public key
    pub s: bool,   // is_signer
    pub w: bool,   // is_writable
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instruction {
    pub p: Pubkey,           // program_id
    pub a: Vec<AccountMeta>, // accounts
    pub d: Vec<u8>,          // data
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SwapMode {
    ExactIn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    pub id: u32,
    pub data: RequestData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RequestData {
    NewSwapQuoteStream(SwapQuoteRequest),
    StopStream(StopStreamRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapQuoteRequest {
    pub swap: SwapParams,
    pub transaction: TransactionParams,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<QuoteUpdateParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapParams {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_mode: Option<SwapMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionParams {
    pub user_public_key: Pubkey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_input_token_account: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_output_token_account: Option<bool>,
    /// Leave a wSOL output as wSOL; Titan unwraps it to native SOL by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_wsol: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteUpdateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_quotes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopStreamRequest {
    pub id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ServerMessage {
    Response(ResponseSuccess),
    Error(ResponseError),
    StreamData(StreamData),
    StreamEnd(StreamEnd),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseSuccess {
    pub request_id: u32,
    /// Set when the request started a stream.
    pub stream: Option<StreamStart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseError {
    pub request_id: u32,
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStart {
    pub id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamData {
    pub id: u32,
    pub payload: StreamDataPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum StreamDataPayload {
    SwapQuotes(SwapQuotes),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEnd {
    pub id: u32,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapQuotes {
    /// Route per provider.
    pub quotes: HashMap<String, SwapRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapRoute {
    pub out_amount: u64,
    /// Empty for providers that only hand out a prebuilt transaction.
    pub instructions: Vec<Instruction>,
    pub address_lookup_tables: Vec<Pubkey>,
    pub expires_at_ms: Option<u64>,
    pub compute_units: Option<u64>,
    /// Recommended CU limit.
    pub compute_units_safe: Option<u64>,
}
