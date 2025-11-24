/*
 * Iptv-Proxy is a project to proxyfie an m3u file and to proxyfie an Xtream iptv service (client API).
 * Copyright (C) 2020  Pierre-Emmanuel Jacquier
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY OR FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

package server

import (
	"crypto/sha256"
	"log"
	"strings"
	"sync"
	"time"
)

const (
	// Default cache TTL (1 week)
	defaultCacheTTL = 7 * 24 * time.Hour
	// Refresh items when they reach 80% of TTL (distribute refreshes over the last 20% of TTL)
	refreshThreshold = 0.8
	// Maximum number of retry attempts before removing from cache
	maxRetryAttempts = 5
	// Minimum time between retry attempts
	minRetryInterval = 1 * time.Hour
	// How often to check for items that need refreshing
	refreshCheckInterval = 15 * time.Minute
)

// RefreshCallback is a function that refreshes a cache key
type RefreshCallback func(key string) error

// CacheRefreshWorker manages background refresh of cache items
type CacheRefreshWorker struct {
	cache     *RedisCache
	config    *Config
	callbacks map[string]RefreshCallback
	stopCh    chan struct{}
	wg        sync.WaitGroup
	mu        sync.RWMutex
}

// NewCacheRefreshWorker creates a new cache refresh worker
func NewCacheRefreshWorker(cache *RedisCache, config *Config) *CacheRefreshWorker {
	if cache == nil {
		return nil
	}

	return &CacheRefreshWorker{
		cache:     cache,
		config:    config,
		callbacks: make(map[string]RefreshCallback),
		stopCh:    make(chan struct{}),
	}
}

// RegisterCallback registers a refresh callback for a cache key pattern
func (w *CacheRefreshWorker) RegisterCallback(pattern string, callback RefreshCallback) {
	if w == nil {
		return
	}

	w.mu.Lock()
	defer w.mu.Unlock()
	w.callbacks[pattern] = callback
}

// Start begins the background refresh worker
func (w *CacheRefreshWorker) Start() {
	if w == nil {
		return
	}

	w.wg.Add(1)
	go w.run()
	log.Printf("[iptv-proxy] Cache refresh worker started")
}

// Stop stops the background refresh worker
func (w *CacheRefreshWorker) Stop() {
	if w == nil {
		return
	}

	close(w.stopCh)
	w.wg.Wait()
	log.Printf("[iptv-proxy] Cache refresh worker stopped")
}

// run is the main refresh loop
func (w *CacheRefreshWorker) run() {
	defer w.wg.Done()

	ticker := time.NewTicker(refreshCheckInterval)
	defer ticker.Stop()

	// Run immediately on startup
	w.refreshExpiringItems()

	for {
		select {
		case <-w.stopCh:
			return
		case <-ticker.C:
			w.refreshExpiringItems()
		}
	}
}

// refreshExpiringItems checks all cache keys and refreshes those approaching expiration
func (w *CacheRefreshWorker) refreshExpiringItems() {
	w.mu.RLock()
	callbacks := make(map[string]RefreshCallback)
	for k, v := range w.callbacks {
		callbacks[k] = v
	}
	w.mu.RUnlock()

	// Check each registered pattern
	for pattern, callback := range callbacks {
		keys, err := w.cache.GetAllCacheKeys(pattern)
		if err != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to get cache keys for pattern %s: %v", pattern, err)
			continue
		}

		for _, key := range keys {
			// Check if key matches pattern (handle wildcards)
			if w.keyMatchesPattern(key, pattern) {
				w.checkAndRefreshKey(key, callback)
			}
		}
	}
}

// keyMatchesPattern checks if a key matches a pattern (supports * wildcard)
func (w *CacheRefreshWorker) keyMatchesPattern(key, pattern string) bool {
	// Convert pattern to regex-like matching
	patternParts := strings.Split(pattern, "*")
	if len(patternParts) == 1 {
		// No wildcard, exact match
		return key == pattern
	}

	// Has wildcard - check if key starts with first part and ends with last part
	if !strings.HasPrefix(key, patternParts[0]) {
		return false
	}

	// For patterns with *, check each segment
	remaining := key[len(patternParts[0]):]
	for i := 1; i < len(patternParts); i++ {
		if patternParts[i] == "" {
			// Trailing * - matches anything
			return true
		}
		idx := strings.Index(remaining, patternParts[i])
		if idx == -1 {
			return false
		}
		remaining = remaining[idx+len(patternParts[i]):]
	}

	return remaining == "" || len(patternParts) > 2
}

// checkAndRefreshKey checks if a key needs refreshing and refreshes it if needed
func (w *CacheRefreshWorker) checkAndRefreshKey(key string, callback RefreshCallback) {
	// Get TTL for the key
	ttl, err := w.cache.client.TTL(w.cache.ctx, key).Result()
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to get TTL for key %s: %v", key, err)
		return
	}

	// If key doesn't exist or has no TTL, skip
	if ttl < 0 {
		return
	}

	// Get refresh metadata
	meta, err := w.cache.GetRefreshMetadata(key)
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to get refresh metadata for key %s: %v", key, err)
		return
	}

	now := time.Now()

	// Check if we should refresh now
	shouldRefresh := false
	if meta == nil {
		// First time - calculate and store refresh time
		refreshTime := w.calculateRefreshTime(key, defaultCacheTTL)
		meta = &RefreshMetadata{
			NextRefresh: refreshTime,
			LastSuccess: now,
		}
		if err := w.cache.SetRefreshMetadata(key, meta, defaultCacheTTL); err != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to set refresh metadata for key %s: %v", key, err)
		}
		shouldRefresh = now.After(refreshTime)
	} else {
		// Check if we've exceeded max retries
		if meta.RetryCount >= maxRetryAttempts {
			log.Printf("[iptv-proxy] Cache key %s exceeded max retries (%d), removing from cache", key, maxRetryAttempts)
			if err := w.cache.DeleteCacheKey(key); err != nil {
				log.Printf("[iptv-proxy] WARNING: Failed to delete expired cache key %s: %v", key, err)
			}
			return
		}

		// Check if enough time has passed since last attempt
		if meta.LastAttempt.IsZero() || now.Sub(meta.LastAttempt) >= minRetryInterval {
			// Refresh if we're past the refresh time, or if previous attempt failed
			if meta.NextRefresh.IsZero() {
				// No refresh time set, calculate it now
				meta.NextRefresh = w.calculateRefreshTime(key, defaultCacheTTL)
				w.cache.SetRefreshMetadata(key, meta, defaultCacheTTL)
			}
			if now.After(meta.NextRefresh) || (meta.RetryCount > 0 && now.After(meta.LastAttempt.Add(minRetryInterval))) {
				shouldRefresh = true
			}
		}
	}

	if !shouldRefresh {
		return
	}

	// Refresh the key
	log.Printf("[iptv-proxy] Refreshing cache key: %s", key)
	if err := callback(key); err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to refresh cache key %s: %v", key, err)
		
		// Increment retry count
		retryCount, retryErr := w.cache.IncrementRetryCount(key)
		if retryErr != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to increment retry count for key %s: %v", key, retryErr)
		} else {
			log.Printf("[iptv-proxy] Cache key %s refresh failed, retry count: %d/%d", key, retryCount, maxRetryAttempts)
		}
	} else {
		// Success - reset retry count and update metadata
		if err := w.cache.ResetRetryCount(key); err != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to reset retry count for key %s: %v", key, err)
		}

		// Update next refresh time for next cycle
		meta, err := w.cache.GetRefreshMetadata(key)
		if err == nil && meta != nil {
			meta.NextRefresh = w.calculateRefreshTime(key, defaultCacheTTL)
			w.cache.SetRefreshMetadata(key, meta, defaultCacheTTL)
		}

		log.Printf("[iptv-proxy] Successfully refreshed cache key: %s", key)
	}
}

// calculateRefreshTime calculates when a cache key should be refreshed
// Uses a hash of the key to distribute refreshes evenly over the refresh window
func (w *CacheRefreshWorker) calculateRefreshTime(key string, ttl time.Duration) time.Time {
	// Calculate refresh window (last 20% of TTL)
	refreshWindow := time.Duration(float64(ttl) * (1 - refreshThreshold))
	
	// Use hash of key to determine position in refresh window
	hash := sha256.Sum256([]byte(key))
	// Use first 8 bytes as a uint64 to get a value between 0 and refreshWindow
	hashValue := uint64(hash[0]) | uint64(hash[1])<<8 | uint64(hash[2])<<16 | uint64(hash[3])<<24 |
		uint64(hash[4])<<32 | uint64(hash[5])<<40 | uint64(hash[6])<<48 | uint64(hash[7])<<56
	
	// Calculate offset in refresh window (0 to refreshWindow)
	offset := time.Duration(hashValue % uint64(refreshWindow))
	
	// Refresh time is when TTL reaches refreshThreshold (80% remaining)
	refreshTime := time.Now().Add(time.Duration(float64(ttl) * refreshThreshold)).Add(offset)
	
	return refreshTime
}

