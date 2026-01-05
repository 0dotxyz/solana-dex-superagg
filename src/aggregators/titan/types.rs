// Titan API type definitions
// All types follow MessagePack encoding as specified in Titan API docs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ══════════════════════════════════════════════════════════════════════════════
// Common Types
// ══════════════════════════════════════════════════════════════════════════════

/// Solana public key encoded as 32-byte binary data (MessagePack bin format)
pub type Pubkey = [u8; 32];

/// Account metadata for Solana instructions
/// Uses short field names to save space in MessagePack encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountMeta {
    pub p: Pubkey, // public key
    pub s: bool,   // is_signer
    pub w: bool,   // is_writable
}

/// Solana instruction
/// Uses short field names to save space in MessagePack encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instruction {
    pub p: Pubkey,           // program_id
    pub a: Vec<AccountMeta>, // accounts
    pub d: Vec<u8>,          // data
}

/// Swap mode for interpreting amounts
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SwapMode {
    ExactIn,
    ExactOut,
}

// ══════════════════════════════════════════════════════════════════════════════
// Request Types
// ══════════════════════════════════════════════════════════════════════════════

/// Client request wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    pub id: u32,
    pub data: RequestData,
}

/// Request data variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RequestData {
    GetInfo(GetInfoRequest),
    NewSwapQuoteStream(SwapQuoteRequest),
    StopStream(StopStreamRequest),
    GetVenues(GetVenuesRequest),
    ListProviders(ListProvidersRequest),
}

/// Get server info request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetInfoRequest {}

/// Swap quote request parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapQuoteRequest {
    pub swap: SwapParams,
    pub transaction: TransactionParams,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<QuoteUpdateParams>,
}

/// Swap parameters
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dexes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_dexes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only_direct_routes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_size_constraint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_constraint: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts_limit_total: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts_limit_writable: Option<u16>,
}

/// Transaction generation parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionParams {
    pub user_public_key: Pubkey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_input_token_account: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_output_token_account: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_account: Option<Pubkey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_bps: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_from_input_mint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_account: Option<Pubkey>,
}

/// Quote update parameters for streaming
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteUpdateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_quotes: Option<u32>,
}

/// Stop stream request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopStreamRequest {
    pub id: u32,
}

/// Get venues request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVenuesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_program_ids: Option<bool>,
}

/// List providers request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProvidersRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_icons: Option<bool>,
}

// ══════════════════════════════════════════════════════════════════════════════
// Response Types
// ══════════════════════════════════════════════════════════════════════════════

/// Server message variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ServerMessage {
    Response(ResponseSuccess),
    Error(ResponseError),
    StreamData(StreamData),
    StreamEnd(StreamEnd),
}

/// Successful response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseSuccess {
    pub request_id: u32,
    pub data: ResponseData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamStart>,
}

/// Error response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseError {
    pub request_id: u32,
    pub code: u32,
    pub message: String,
}

/// Response data variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ResponseData {
    GetInfo(ServerInfo),
    NewSwapQuoteStream(QuoteSwapStreamResponse),
    StreamStopped(StopStreamResponse),
    GetVenues(VenueInfo),
    ListProviders(Vec<ProviderInfo>),
}

/// Server information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub protocol_version: VersionInfo,
    pub settings: ServerSettings,
}

/// Version information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

/// Server settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    pub quote_update: QuoteUpdateSettings,
    pub swap: SwapSettings,
    pub transaction: TransactionSettings,
    pub connection: ConnectionSettings,
}

/// Quote update settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteUpdateSettings {
    pub interval_ms: BoundedValueWithDefault<u64>,
    pub num_quotes: BoundedValueWithDefault<u32>,
}

/// Swap settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapSettings {
    pub slippage_bps: BoundedValueWithDefault<u16>,
    pub only_direct_routes: bool,
    pub add_size_constraint: bool,
}

/// Transaction settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSettings {
    pub close_input_token_account: bool,
    pub create_output_token_account: bool,
}

/// Connection settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSettings {
    pub concurrent_streams: u32,
}

/// Bounded value with default
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundedValueWithDefault<T> {
    pub min: T,
    pub max: T,
    pub default: T,
}

/// Quote swap stream response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteSwapStreamResponse {
    pub interval_ms: u64,
}

/// Stop stream response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopStreamResponse {
    pub id: u32,
}

/// Venue information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueInfo {
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_ids: Option<Vec<Pubkey>>,
}

/// Provider information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_uri_48: Option<String>,
}

/// Provider kind
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProviderKind {
    DexAggregator,
    RFQ,
}

// ══════════════════════════════════════════════════════════════════════════════
// Stream Types
// ══════════════════════════════════════════════════════════════════════════════

/// Stream data type indicator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamDataType {
    SwapQuotes,
}

/// Stream start notification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStart {
    pub id: u32,
    pub data_type: StreamDataType,
}

/// Stream data packet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamData {
    pub id: u32,
    pub seq: u32,
    pub payload: StreamDataPayload,
}

/// Stream data payload variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum StreamDataPayload {
    SwapQuotes(SwapQuotes),
}

/// Stream end notification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEnd {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

// ══════════════════════════════════════════════════════════════════════════════
// Swap Quote Types
// ══════════════════════════════════════════════════════════════════════════════

/// Swap quotes from multiple providers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapQuotes {
    pub id: String,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub swap_mode: SwapMode,
    pub amount: u64,
    pub quotes: HashMap<String, SwapRoute>,
}

/// Swap route from a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapRoute {
    pub in_amount: u64,
    pub out_amount: u64,
    pub slippage_bps: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_fee: Option<PlatformFee>,
    pub steps: Vec<RoutePlanStep>,
    pub instructions: Vec<Instruction>,
    pub address_lookup_tables: Vec<Pubkey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_slot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_taken_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_after_slot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_units: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_units_safe: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_id: Option<String>,
}

/// Route plan step
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub amm_key: Pubkey,
    pub label: String,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub in_amount: u64,
    pub out_amount: u64,
    pub alloc_ppb: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_mint: Option<Pubkey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_amount: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_slot: Option<u64>,
}

/// Platform fee information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformFee {
    pub amount: u64,
    pub fee_bps: u8,
}
