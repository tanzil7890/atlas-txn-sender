use std::{sync::Arc, time::Instant};

use cadence_macros::{statsd_count, statsd_time};

// Helper macros that are no-ops in test mode
#[cfg(test)]
macro_rules! metrics_count {
    ($($tt:tt)*) => {};
}

#[cfg(not(test))]
macro_rules! metrics_count {
    ($($tt:tt)*) => {
        statsd_count!($($tt)*)
    };
}

#[cfg(test)]
macro_rules! metrics_time {
    ($($tt:tt)*) => {};
}

#[cfg(not(test))]
macro_rules! metrics_time {
    ($($tt:tt)*) => {
        statsd_time!($($tt)*)
    };
}
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use serde::Deserialize;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;

use crate::{
    blockhash_manager::BlockhashManager,
    errors::invalid_request,
    transaction_store::{TransactionData, TransactionStore},
    txn_sender::TxnSender,
    vendor::solana_rpc::decode_and_deserialize,
};

// jsonrpsee does not make it easy to access http data,
// so creating this optional param to pass in metadata
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct RequestMetadata {
    pub api_key: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct TransactionBundleRequest {
    pub transactions: Vec<String>,
    pub bundle_config: Option<BundleConfig>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct BundleConfig {
    pub max_confirmation_timeout_seconds: Option<u64>,
    pub skip_invalid_transactions: Option<bool>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct TransactionBundleResponse {
    pub bundle_id: String,
    pub successful_transactions: Vec<String>,
    pub failed_transaction: Option<String>,
    pub total_transactions: usize,
    pub success_count: usize,
}

#[rpc(server)]
pub trait AtlasTxnSender {
    #[method(name = "health")]
    async fn health(&self) -> String;
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<String>;
    #[method(name = "sendTransactionBundle")]
    async fn send_transaction_bundle(
        &self,
        bundle_request: TransactionBundleRequest,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<TransactionBundleResponse>;
}

pub struct AtlasTxnSenderImpl {
    txn_sender: Arc<dyn TxnSender>,
    transaction_store: Arc<dyn TransactionStore>,
    max_txn_send_retries: usize,
    blockhash_manager: Arc<BlockhashManager>,
}

impl AtlasTxnSenderImpl {
    pub fn new(
        txn_sender: Arc<dyn TxnSender>,
        transaction_store: Arc<dyn TransactionStore>,
        max_txn_send_retries: usize,
        blockhash_manager: Arc<BlockhashManager>,
    ) -> Self {
        Self {
            txn_sender,
            max_txn_send_retries,
            transaction_store,
            blockhash_manager,
        }
    }

    async fn execute_transaction_bundle(
        &self,
        transactions: Vec<TransactionData>,
        bundle_config: Option<BundleConfig>,
        api_key: String,
    ) -> Result<TransactionBundleResponse, ErrorObjectOwned> {
        let mut successful_transactions = Vec::new();
        let mut failed_transaction = None;
        let timeout_seconds = bundle_config
            .as_ref()
            .and_then(|config| config.max_confirmation_timeout_seconds)
            .unwrap_or(60); // Default 60 seconds timeout per transaction

        for (_index, transaction_data) in transactions.into_iter().enumerate() {
            let signature = transaction_data.versioned_transaction.signatures[0].to_string();
            
            // Send the transaction
            self.txn_sender.send_transaction(transaction_data.clone());
            
            // Wait for confirmation
            let confirmation_result = self.wait_for_confirmation(
                signature.clone(),
                timeout_seconds,
                api_key.clone(),
            ).await;
            
            match confirmation_result {
                Ok(confirmed) => {
                    if confirmed {
                        successful_transactions.push(signature);
                        // metrics_count!("bundle_transaction_success", 1, "api_key" => &api_key);
                    } else {
                        // Transaction failed or timed out
                        failed_transaction = Some(signature);
                        // metrics_count!("bundle_transaction_failed", 1, "api_key" => &api_key);
                        break; // Stop processing remaining transactions
                    }
                }
                Err(_) => {
                    // Error in confirmation process
                    failed_transaction = Some(signature);
                    // metrics_count!("bundle_transaction_error", 1, "api_key" => &api_key);
                    break; // Stop processing remaining transactions
                }
            }
        }

        let success_count = successful_transactions.len();
        let total_transactions = success_count + if failed_transaction.is_some() { 1 } else { 0 };
        
        Ok(TransactionBundleResponse {
            bundle_id: String::new(), // Will be set by caller
            successful_transactions,
            failed_transaction,
            total_transactions,
            success_count,
        })
    }

    async fn wait_for_confirmation(
        &self,
        signature: String,
        timeout_seconds: u64,
        _api_key: String,
    ) -> Result<bool, ErrorObjectOwned> {
        use tokio::time::{sleep, timeout, Duration};
        
        // Use a timeout to avoid waiting indefinitely
        let result = timeout(
            Duration::from_secs(timeout_seconds),
            async {
                // Poll for confirmation every 500ms
                loop {
                    // Check if transaction is still in store (means it's not confirmed yet)
                    if !self.transaction_store.has_signature(&signature) {
                        // Transaction was removed from store, which means it was confirmed
                        return true;
                    }
                    
                    sleep(Duration::from_millis(500)).await;
                }
            }
        ).await;

        match result {
            Ok(confirmed) => Ok(confirmed),
            Err(_) => {
                // Timeout occurred
                // Remove transaction from store to prevent further retries
                self.transaction_store.remove_transaction(signature);
                Ok(false)
            }
        }
    }
}

#[async_trait]
impl AtlasTxnSenderServer for AtlasTxnSenderImpl {
    async fn health(&self) -> String {
        "ok".to_string()
    }
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<String> {
        let sent_at = Instant::now();
        let api_key = request_metadata
            .clone()
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        // metrics_count!("send_transaction", 1, "api_key" => &api_key);
        validate_send_transaction_params(&params)?;
        let start = Instant::now();
        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;
        let (wire_transaction, versioned_transaction) =
            match decode_and_deserialize::<VersionedTransaction>(txn, binary_encoding) {
                Ok((wire_transaction, versioned_transaction)) => {
                    (wire_transaction, versioned_transaction)
                }
                Err(e) => {
                    return Err(invalid_request(&e.to_string()));
                }
            };
        let signature = versioned_transaction.signatures[0].to_string();
        if self.transaction_store.has_signature(&signature) {
            // metrics_count!("duplicate_transaction", 1, "api_key" => &api_key);
            return Ok(signature);
        }

        // Validate blockhash for bandwidth optimization
        let blockhash = versioned_transaction.message.recent_blockhash();
        if !self.blockhash_manager.is_blockhash_valid(&blockhash) {
            // metrics_count!("invalid_blockhash", 1, "api_key" => &api_key);
            return Err(invalid_request(&format!(
                "Transaction has expired blockhash: {}. Please use a recent blockhash.",
                blockhash
            )));
        }
        let transaction = TransactionData {
            wire_transaction,
            versioned_transaction,
            sent_at,
            retry_count: 0,
            max_retries: std::cmp::min(
                self.max_txn_send_retries,
                params.max_retries.unwrap_or(self.max_txn_send_retries),
            ),
            request_metadata,
        };
        self.txn_sender.send_transaction(transaction);
        // metrics_time!(
        //     "send_transaction_time",
        //     start.elapsed(),
        //     "api_key" => &api_key
        // );
        Ok(signature)
    }

    async fn send_transaction_bundle(
        &self,
        bundle_request: TransactionBundleRequest,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<TransactionBundleResponse> {
        let sent_at = Instant::now();
        let api_key = request_metadata
            .clone()
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        // metrics_count!("send_transaction_bundle", 1, "api_key" => &api_key);
        
        // Generate unique bundle ID
        let bundle_id = format!("bundle_{}", sent_at.elapsed().as_nanos());
        
        // Validate bundle
        if bundle_request.transactions.is_empty() {
            return Err(invalid_request("Bundle cannot be empty"));
        }
        
        validate_send_transaction_params(&params)?;
        
        let start = Instant::now();
        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;

        // Parse and validate all transactions first
        let mut transaction_data_list = Vec::new();
        for (index, txn_str) in bundle_request.transactions.iter().enumerate() {
            let (wire_transaction, versioned_transaction) =
                match decode_and_deserialize::<VersionedTransaction>(txn_str.clone(), binary_encoding) {
                    Ok((wire_transaction, versioned_transaction)) => {
                        (wire_transaction, versioned_transaction)
                    }
                    Err(e) => {
                        return Err(invalid_request(&format!(
                            "Transaction {} in bundle is invalid: {}",
                            index, e
                        )));
                    }
                };
            
            let signature = versioned_transaction.signatures[0].to_string();
            
            // Check for duplicates
            if self.transaction_store.has_signature(&signature) {
                // metrics_count!("duplicate_transaction_in_bundle", 1, "api_key" => &api_key);
                if bundle_request.bundle_config.as_ref()
                    .and_then(|config| config.skip_invalid_transactions)
                    .unwrap_or(false) {
                    continue; // Skip duplicate transactions if configured to do so
                } else {
                    return Err(invalid_request(&format!(
                        "Transaction {} in bundle is a duplicate: {}",
                        index, signature
                    )));
                }
            }

            // Validate blockhash for bandwidth optimization
            let blockhash = versioned_transaction.message.recent_blockhash();
            if !self.blockhash_manager.is_blockhash_valid(&blockhash) {
                // metrics_count!("invalid_blockhash_in_bundle", 1, "api_key" => &api_key);
                if bundle_request.bundle_config.as_ref()
                    .and_then(|config| config.skip_invalid_transactions)
                    .unwrap_or(false) {
                    continue; // Skip transactions with invalid blockhashes if configured to do so
                } else {
                    return Err(invalid_request(&format!(
                        "Transaction {} in bundle has expired blockhash: {}",
                        index, blockhash
                    )));
                }
            }

            let transaction_data = TransactionData {
                wire_transaction,
                versioned_transaction,
                sent_at,
                retry_count: 0,
                max_retries: std::cmp::min(
                    self.max_txn_send_retries,
                    params.max_retries.unwrap_or(self.max_txn_send_retries),
                ),
                request_metadata: request_metadata.clone(),
            };
            
            transaction_data_list.push(transaction_data);
        }

        // Save the count before moving the list
        let total_transactions = transaction_data_list.len();
        
        // Execute bundle serially
        let bundle_result = self.execute_transaction_bundle(
            transaction_data_list,
            bundle_request.bundle_config.clone(),
            api_key.clone(),
        ).await;

        // metrics_time!(
        //     "send_transaction_bundle_time",
        //     start.elapsed(),
        //     "api_key" => &api_key
        // );

        match bundle_result {
            Ok(response) => Ok(TransactionBundleResponse {
                bundle_id,
                successful_transactions: response.successful_transactions,
                failed_transaction: response.failed_transaction,
                total_transactions,
                success_count: response.success_count,
            }),
            Err(e) => Err(e),
        }
    }
}

fn validate_send_transaction_params(
    params: &RpcSendTransactionConfig,
) -> Result<(), ErrorObjectOwned> {
    if !params.skip_preflight {
        return Err(invalid_request("running preflight check is not supported"));
    }
    Ok(())
}
