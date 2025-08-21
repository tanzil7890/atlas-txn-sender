use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use cadence_macros::statsd_count;
use solana_client::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Manages recent blockhashes and their validity
/// Solana blockhashes are valid for approximately 150 slots (~2-3 minutes)
pub struct BlockhashManager {
    rpc_client: Arc<RpcClient>,
    recent_blockhashes: Arc<RwLock<HashMap<Hash, BlockhashInfo>>>,
    latest_blockhash: Arc<RwLock<Option<Hash>>>,
}

#[derive(Clone, Debug)]
struct BlockhashInfo {
    /// When this blockhash was first seen
    first_seen: Instant,
    /// Whether this blockhash is still valid for new transactions
    is_valid: bool,
    /// The slot when this blockhash was created (reserved for future use)
    #[allow(dead_code)]
    slot: Option<u64>,
}

impl BlockhashManager {
    /// Maximum age for a blockhash before it's considered expired (140 slots = ~2.5 minutes)
    const MAX_BLOCKHASH_AGE: Duration = Duration::from_secs(150); // Conservative estimate
    
    /// How often to poll for new blockhashes
    const POLL_INTERVAL: Duration = Duration::from_secs(30);
    
    /// How often to clean up expired blockhashes
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        let manager = Self {
            rpc_client,
            recent_blockhashes: Arc::new(RwLock::new(HashMap::new())),
            latest_blockhash: Arc::new(RwLock::new(None)),
        };
        
        manager.start_blockhash_polling();
        manager.start_blockhash_cleanup();
        manager
    }

    /// Check if a transaction's blockhash is still valid
    pub fn is_blockhash_valid(&self, blockhash: &Hash) -> bool {
        let blockhashes = match self.recent_blockhashes.read() {
            Ok(guard) => guard,
            Err(_) => {
                warn!("Failed to acquire read lock for blockhashes");
                return true; // Conservative: assume valid if we can't check
            }
        };

        match blockhashes.get(blockhash) {
            Some(info) => {
                let is_valid = info.is_valid && info.first_seen.elapsed() < Self::MAX_BLOCKHASH_AGE;
                // #[cfg(not(test))]
                // {
                //     if !is_valid {
                //         statsd_count!("blockhash_validation_expired", 1);
                //     } else {
                //         statsd_count!("blockhash_validation_valid", 1);
                //     }
                // }
                is_valid
            }
            None => {
                // Unknown blockhash - could be very new or very old
                // Conservative approach: assume it's valid but log it
                // #[cfg(not(test))]
                // statsd_count!("blockhash_validation_unknown", 1);
                true
            }
        }
    }

    /// Get the latest known valid blockhash
    #[allow(dead_code)]
    pub fn get_latest_blockhash(&self) -> Option<Hash> {
        match self.latest_blockhash.read() {
            Ok(guard) => *guard,
            Err(_) => {
                warn!("Failed to acquire read lock for latest blockhash");
                None
            }
        }
    }

    /// Start background task to poll for new blockhashes
    fn start_blockhash_polling(&self) {
        let rpc_client = self.rpc_client.clone();
        let recent_blockhashes = self.recent_blockhashes.clone();
        let latest_blockhash = self.latest_blockhash.clone();

        tokio::spawn(async move {
            loop {
                match rpc_client.get_latest_blockhash() {
                    Ok(new_blockhash) => {
                        let now = Instant::now();
                        
                        // Update latest blockhash
                        if let Ok(mut latest) = latest_blockhash.write() {
                            *latest = Some(new_blockhash);
                        }

                        // Add to recent blockhashes if not already present
                        if let Ok(mut blockhashes) = recent_blockhashes.write() {
                            blockhashes.entry(new_blockhash).or_insert_with(|| {
                                info!("New blockhash discovered: {}", new_blockhash);
                                // #[cfg(not(test))]
                                // statsd_count!("blockhash_discovered", 1);
                                BlockhashInfo {
                                    first_seen: now,
                                    is_valid: true,
                                    slot: None, // We could fetch this if needed
                                }
                            });
                        }
                    }
                    Err(e) => {
                        error!("Failed to fetch latest blockhash: {}", e);
                        // #[cfg(not(test))]
                        // statsd_count!("blockhash_fetch_error", 1);
                    }
                }

                sleep(Self::POLL_INTERVAL).await;
            }
        });
    }

    /// Start background task to clean up expired blockhashes
    fn start_blockhash_cleanup(&self) {
        let recent_blockhashes = self.recent_blockhashes.clone();

        tokio::spawn(async move {
            loop {
                sleep(Self::CLEANUP_INTERVAL).await;

                if let Ok(mut blockhashes) = recent_blockhashes.write() {
                    let before_count = blockhashes.len();
                    
                    // Remove expired blockhashes
                    blockhashes.retain(|_, info| {
                        info.first_seen.elapsed() < Self::MAX_BLOCKHASH_AGE
                    });
                    
                    let after_count = blockhashes.len();
                    let cleaned_count = before_count - after_count;
                    
                    if cleaned_count > 0 {
                        info!("Cleaned up {} expired blockhashes", cleaned_count);
                        // #[cfg(not(test))]
                        // statsd_count!("blockhashes_cleaned_up", cleaned_count as i64);
                    }
                }
            }
        });
    }

    /// Mark a blockhash as potentially invalid (e.g., transaction using it failed)
    #[allow(dead_code)]
    pub fn mark_blockhash_invalid(&self, blockhash: &Hash) {
        if let Ok(mut blockhashes) = self.recent_blockhashes.write() {
            if let Some(info) = blockhashes.get_mut(blockhash) {
                info.is_valid = false;
                info!("Marked blockhash as invalid: {}", blockhash);
                // #[cfg(not(test))]
                // statsd_count!("blockhash_marked_invalid", 1);
            }
        }
    }

    /// Get statistics about managed blockhashes
    #[allow(dead_code)]
    pub fn get_stats(&self) -> BlockhashStats {
        if let Ok(blockhashes) = self.recent_blockhashes.read() {
            let total = blockhashes.len();
            let valid = blockhashes.values().filter(|info| info.is_valid).count();
            let has_latest = self.latest_blockhash.read()
                .map(|guard| guard.is_some())
                .unwrap_or(false);

            BlockhashStats {
                total_blockhashes: total,
                valid_blockhashes: valid,
                has_latest_blockhash: has_latest,
            }
        } else {
            BlockhashStats::default()
        }
    }

    /// Internal method for testing - manually add a blockhash
    #[cfg(any(test, feature = "test-utils"))]
    pub fn add_test_blockhash(&self, blockhash: Hash, age: Duration) {
        if let Ok(mut blockhashes) = self.recent_blockhashes.write() {
            blockhashes.insert(blockhash, BlockhashInfo {
                first_seen: Instant::now() - age,
                is_valid: true,
                slot: None,
            });
        }
    }
}

#[derive(Debug, Default)]
pub struct BlockhashStats {
    pub total_blockhashes: usize,
    pub valid_blockhashes: usize,
    pub has_latest_blockhash: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use std::time::Duration;


    fn create_test_rpc_client() -> Arc<RpcClient> {
        // Create a test RPC client that won't actually connect
        Arc::new(RpcClient::new("http://localhost:8899".to_string()))
    }

    #[tokio::test]
    async fn test_blockhash_validation_valid() {
        let manager = BlockhashManager::new(create_test_rpc_client());
        let test_hash = Hash::new_unique();
        
        // Add a recent blockhash
        manager.add_test_blockhash(test_hash, Duration::from_secs(0));
        
        // Should be valid
        assert!(manager.is_blockhash_valid(&test_hash));
    }

    #[tokio::test]
    async fn test_blockhash_validation_expired() {
        let manager = BlockhashManager::new(create_test_rpc_client());
        let test_hash = Hash::new_unique();
        
        // Add an old blockhash (older than MAX_BLOCKHASH_AGE)
        manager.add_test_blockhash(test_hash, Duration::from_secs(200));
        
        // Should be invalid due to age
        assert!(!manager.is_blockhash_valid(&test_hash));
    }

    #[tokio::test]
    async fn test_blockhash_validation_unknown() {
        let manager = BlockhashManager::new(create_test_rpc_client());
        let unknown_hash = Hash::new_unique();
        
        // Unknown blockhash should be considered valid (conservative approach)
        assert!(manager.is_blockhash_valid(&unknown_hash));
    }

    #[tokio::test]
    async fn test_mark_blockhash_invalid() {
        let manager = BlockhashManager::new(create_test_rpc_client());
        let test_hash = Hash::new_unique();
        
        // Add a recent blockhash
        manager.add_test_blockhash(test_hash, Duration::from_secs(0));
        
        // Should be valid initially
        assert!(manager.is_blockhash_valid(&test_hash));
        
        // Mark as invalid
        manager.mark_blockhash_invalid(&test_hash);
        
        // Should now be invalid
        assert!(!manager.is_blockhash_valid(&test_hash));
    }

    #[tokio::test]
    async fn test_get_stats() {
        let manager = BlockhashManager::new(create_test_rpc_client());
        let test_hash1 = Hash::new_unique();
        let test_hash2 = Hash::new_unique();
        
        // Add two valid blockhashes
        manager.add_test_blockhash(test_hash1, Duration::from_secs(0));
        manager.add_test_blockhash(test_hash2, Duration::from_secs(10));
        
        let stats = manager.get_stats();
        assert_eq!(stats.total_blockhashes, 2);
        assert_eq!(stats.valid_blockhashes, 2);
        
        // Mark one as invalid
        manager.mark_blockhash_invalid(&test_hash1);
        
        let stats = manager.get_stats();
        assert_eq!(stats.total_blockhashes, 2);
        assert_eq!(stats.valid_blockhashes, 1);
    }
}