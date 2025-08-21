use std::{sync::Arc, time::Instant};

use atlas_txn_sender::{
    blockhash_manager::BlockhashManager,
    rpc_server::{AtlasTxnSenderImpl, AtlasTxnSenderServer, BundleConfig, RequestMetadata, TransactionBundleRequest},
    transaction_store::{TransactionStoreImpl, TransactionStore, TransactionData},
    txn_sender::TxnSender,
};
use jsonrpsee::core::async_trait;
use solana_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::{
    bs58,
    hash::Hash,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use solana_transaction_status::UiTransactionEncoding;

// Mock transaction sender for testing
struct MockTxnSender {
    sent_transactions: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockTxnSender {
    fn new() -> Self {
        Self {
            sent_transactions: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn get_sent_transactions(&self) -> Vec<String> {
        self.sent_transactions.lock().unwrap().clone()
    }
}

#[async_trait]
impl TxnSender for MockTxnSender {
    fn send_transaction(&self, txn: TransactionData) {
        let signature = txn.versioned_transaction.signatures[0].to_string();
        self.sent_transactions.lock().unwrap().push(signature);
    }
}

fn create_test_transaction(payer: &Keypair, recent_blockhash: Hash) -> String {
    let to_pubkey = Keypair::new().pubkey();
    let instruction = system_instruction::transfer(&payer.pubkey(), &to_pubkey, 1000000);
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );
    
    let versioned_transaction = VersionedTransaction::from(transaction);
    bs58::encode(bincode::serialize(&versioned_transaction).unwrap()).into_string()
}

fn create_test_rpc_client() -> Arc<RpcClient> {
    Arc::new(RpcClient::new("http://localhost:8899".to_string()))
}

#[tokio::test]
async fn test_send_transaction_bundle_empty() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store,
        5,
        blockhash_manager,
    );

    let bundle_request = TransactionBundleRequest {
        transactions: vec![], // Empty bundle
        bundle_config: None,
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        None,
    ).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().message().contains("Bundle cannot be empty"));
}

#[tokio::test]
async fn test_send_transaction_bundle_valid() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store,
        5,
        blockhash_manager.clone(),
    );

    // Create test transactions
    let payer1 = Keypair::new();
    let payer2 = Keypair::new();
    let recent_blockhash = Hash::new_unique();
    
    // Add the blockhash to manager as valid
    blockhash_manager.add_test_blockhash(recent_blockhash, std::time::Duration::from_secs(0));
    
    let tx1 = create_test_transaction(&payer1, recent_blockhash);
    let tx2 = create_test_transaction(&payer2, recent_blockhash);

    let bundle_request = TransactionBundleRequest {
        transactions: vec![tx1, tx2],
        bundle_config: Some(BundleConfig {
            max_confirmation_timeout_seconds: Some(1), // Short timeout for testing
            skip_invalid_transactions: Some(false),
        }),
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        Some(RequestMetadata {
            api_key: "test_key".to_string(),
        }),
    ).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.total_transactions, 2);
    assert!(!response.bundle_id.is_empty());
}

#[tokio::test]
async fn test_send_transaction_bundle_with_expired_blockhash() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store,
        5,
        blockhash_manager.clone(),
    );

    // Create test transaction with expired blockhash
    let payer = Keypair::new();
    let old_blockhash = Hash::new_unique();
    
    // Add an expired blockhash
    blockhash_manager.add_test_blockhash(old_blockhash, std::time::Duration::from_secs(200));
    
    let tx = create_test_transaction(&payer, old_blockhash);

    let bundle_request = TransactionBundleRequest {
        transactions: vec![tx],
        bundle_config: Some(BundleConfig {
            max_confirmation_timeout_seconds: Some(1),
            skip_invalid_transactions: Some(false), // Don't skip invalid transactions
        }),
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        None,
    ).await;

    // Should fail due to expired blockhash
    assert!(result.is_err());
    assert!(result.unwrap_err().message().contains("expired blockhash"));
}

#[tokio::test]
async fn test_send_transaction_bundle_skip_invalid() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store,
        5,
        blockhash_manager.clone(),
    );

    // Create test transactions - one valid, one with expired blockhash
    let payer1 = Keypair::new();
    let payer2 = Keypair::new();
    let valid_blockhash = Hash::new_unique();
    let expired_blockhash = Hash::new_unique();
    
    // Add blockhashes
    blockhash_manager.add_test_blockhash(valid_blockhash, std::time::Duration::from_secs(0));
    blockhash_manager.add_test_blockhash(expired_blockhash, std::time::Duration::from_secs(200));
    
    let valid_tx = create_test_transaction(&payer1, valid_blockhash);
    let invalid_tx = create_test_transaction(&payer2, expired_blockhash);

    let bundle_request = TransactionBundleRequest {
        transactions: vec![valid_tx, invalid_tx],
        bundle_config: Some(BundleConfig {
            max_confirmation_timeout_seconds: Some(1),
            skip_invalid_transactions: Some(true), // Skip invalid transactions
        }),
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        None,
    ).await;

    // Should succeed with only the valid transaction
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.total_transactions, 1); // Only the valid transaction should be processed
}

#[tokio::test]
async fn test_send_transaction_bundle_duplicate_detection() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store.clone(),
        5,
        blockhash_manager.clone(),
    );

    // Create test transaction
    let payer = Keypair::new();
    let recent_blockhash = Hash::new_unique();
    blockhash_manager.add_test_blockhash(recent_blockhash, std::time::Duration::from_secs(0));
    
    let tx = create_test_transaction(&payer, recent_blockhash);
    
    // Manually add transaction to store to simulate duplicate
    let versioned_tx = bincode::deserialize::<VersionedTransaction>(
        &bs58::decode(&tx).into_vec().unwrap()
    ).unwrap();
    
    let _signature = versioned_tx.signatures[0].to_string();
    transaction_store.add_transaction(TransactionData {
        wire_transaction: bs58::decode(&tx).into_vec().unwrap(),
        versioned_transaction: versioned_tx,
        sent_at: Instant::now(),
        retry_count: 0,
        max_retries: 5,
        request_metadata: None,
    });

    let bundle_request = TransactionBundleRequest {
        transactions: vec![tx],
        bundle_config: Some(BundleConfig {
            max_confirmation_timeout_seconds: Some(1),
            skip_invalid_transactions: Some(false), // Don't skip duplicates
        }),
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        None,
    ).await;

    // Should fail due to duplicate transaction
    assert!(result.is_err());
    assert!(result.unwrap_err().message().contains("duplicate"));
}

#[tokio::test]
async fn test_bundle_config_defaults() {
    let mock_txn_sender = Arc::new(MockTxnSender::new());
    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let rpc_client = create_test_rpc_client();
    let blockhash_manager = Arc::new(BlockhashManager::new(rpc_client));
    
    let atlas_sender = AtlasTxnSenderImpl::new(
        mock_txn_sender.clone(),
        transaction_store,
        5,
        blockhash_manager.clone(),
    );

    // Create test transaction
    let payer = Keypair::new();
    let recent_blockhash = Hash::new_unique();
    blockhash_manager.add_test_blockhash(recent_blockhash, std::time::Duration::from_secs(0));
    
    let tx = create_test_transaction(&payer, recent_blockhash);

    let bundle_request = TransactionBundleRequest {
        transactions: vec![tx],
        bundle_config: None, // No config provided - should use defaults
    };

    let params = RpcSendTransactionConfig {
        skip_preflight: true,
        encoding: Some(UiTransactionEncoding::Base58),
        ..Default::default()
    };

    let result = atlas_sender.send_transaction_bundle(
        bundle_request,
        params,
        None,
    ).await;

    // Should work with default configuration
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.total_transactions, 1);
}
