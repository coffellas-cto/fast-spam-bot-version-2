# Token Balance Caching Optimization

## Overview
This document describes the token balance caching optimization implemented to reduce latency during selling operations.

## Problem
Previously, during the selling process, the system had to make an RPC call to `get_token_account()` to fetch the current token balance. This added significant latency to the selling process, which is critical for timing-sensitive operations.

## Solution
Implemented a comprehensive token balance caching system that:

1. **Caches balance after successful buy transactions**
2. **Uses cached balance for fast selling**
3. **Falls back to RPC only when necessary**
4. **Invalidates cache when transactions fail**

## Implementation Details

### 1. Extended Cache Structure

#### BoughtTokenInfo Enhancement
```rust
pub struct BoughtTokenInfo {
    // ... existing fields ...
    // New fields for balance caching
    pub cached_balance_raw: u64,
    pub cached_balance_decimal: f64,
    pub cached_decimals: u8,
    pub balance_cache_time: Instant,
    pub balance_verified: bool,
}
```

#### New TokenBalanceCache
```rust
pub struct TokenBalanceCache {
    balances: RwLock<HashMap<String, TokenBalanceInfo>>,
    default_ttl: u64,
}
```

### 2. Enhanced BoughtTokensTracker Methods

- `cache_verified_balance()` - Cache balance after successful buy verification
- `get_cached_balance()` - Retrieve cached balance if not stale
- `invalidate_balance_cache()` - Mark cache as invalid after failed transactions

### 3. Selling Process Optimization

#### Before (High Latency):
```rust
// Always makes RPC call during selling
let account = rpc_client.get_token_account(&ata).await?;
```

#### After (Low Latency):
```rust
// Try cache first, fallback to RPC only if needed
let balance = self.get_token_balance_fast(mint, &ata).await?;
```

### 4. Smart Caching Strategy

#### Buy Transaction Flow:
1. Execute buy transaction
2. Verify transaction success
3. **NEW**: Fetch and cache token balance (500ms delay for settlement)
4. Track token for future selling

#### Sell Transaction Flow:
1. Check cached balance (max age: 30 seconds)
2. If cache hit: Use cached balance (minimal latency)
3. If cache miss: Fetch from RPC and update cache
4. Execute sell with balance info

### 5. Cache Invalidation Logic

Cache is invalidated when:
- Sell transaction fails
- Transaction verification fails
- Balance becomes stale (> 30 seconds for selling)

### 6. Updated Cache Maintenance

Extended `CacheMaintenanceService` to clean up the new token balance cache periodically.

## Performance Benefits

### Latency Reduction
- **Cache Hit**: Near-zero latency (memory access)
- **Cache Miss**: Same as before (RPC call + cache update)
- **Expected Hit Rate**: ~90% for active trading tokens

### Smart Fallback
- Never compromises accuracy
- Automatically refreshes stale data
- Handles edge cases gracefully

## Configuration

### Cache TTL Settings
- **Selling Operations**: 30 seconds max age
- **General Cache**: 5 minutes (300 seconds)
- **Verification Window**: 500ms post-transaction

### Memory Usage
- Minimal overhead per tracked token
- Automatic cleanup of stale entries
- Bounded by number of actively traded tokens

## Error Handling

### Cache Miss Scenarios
1. Token never cached (new buy)
2. Cache expired (> 30 seconds)
3. Cache invalidated (failed transaction)

### Fallback Behavior
- Seamless RPC fallback
- Cache repopulation
- Error propagation maintained

## Monitoring

### Cache Statistics
- Cache hit/miss rates
- Cache size metrics
- Cleanup frequency logs

### Debug Logging
- Color-coded cache operations
- Performance timing information
- Cache state transitions

## Future Enhancements

### Potential Improvements
1. **Real-time Balance Updates**: Subscribe to account changes
2. **Predictive Caching**: Pre-cache balances for likely sells
3. **Distributed Caching**: Share cache across instances
4. **Advanced TTL**: Dynamic TTL based on token volatility

### Metrics to Track
- Average selling latency improvement
- Cache hit rates by token
- Memory usage patterns
- RPC call reduction percentage

## Usage Guidelines

### For Developers
1. Cache is automatically managed
2. No manual cache operations needed
3. Debug logs show cache status
4. Monitor cache statistics for optimization

### For Operations
1. Cache reduces RPC load
2. Faster selling execution
3. Automatic cleanup maintains performance
4. No configuration changes required

## Testing Recommendations

### Performance Testing
1. Measure selling latency before/after
2. Test cache behavior under load
3. Verify cache invalidation scenarios
4. Monitor memory usage patterns

### Functional Testing
1. Verify balance accuracy
2. Test cache expiration logic
3. Validate fallback behavior
4. Check error handling paths 