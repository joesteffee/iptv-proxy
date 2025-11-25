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
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

package server

import (
	"context"
	"log"
	"strings"
	"time"

	"github.com/redis/go-redis/v9"
	jsoniter "github.com/json-iterator/go"
)

// RedisCache represents a Redis-based cache
type RedisCache struct {
	client *redis.Client
	ctx    context.Context
}

// NewRedisCache creates a new Redis cache instance
// Returns nil if url is empty (Redis disabled)
func NewRedisCache(url string) (*RedisCache, error) {
	if url == "" {
		return nil, nil
	}

	opts, err := redis.ParseURL(url)
	if err != nil {
		return nil, err
	}

	client := redis.NewClient(opts)
	ctx := context.Background()

	// Test connection
	if err := client.Ping(ctx).Err(); err != nil {
		return nil, err
	}

	log.Printf("[iptv-proxy] Redis cache initialized successfully")
	return &RedisCache{
		client: client,
		ctx:    ctx,
	}, nil
}

// Get retrieves cached data by key
// Returns nil, nil if key doesn't exist (cache miss)
// Returns error if Redis operation fails
func (r *RedisCache) Get(key string) ([]byte, error) {
	if r == nil {
		return nil, nil
	}

	val, err := r.client.Get(r.ctx, key).Bytes()
	if err == redis.Nil {
		return nil, nil // Cache miss
	}
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Redis Get error for key %s: %v", key, err)
		return nil, err
	}

	return val, nil
}

// Set stores data in cache with TTL
func (r *RedisCache) Set(key string, data []byte, ttl time.Duration) error {
	if r == nil {
		return nil
	}

	err := r.client.Set(r.ctx, key, data, ttl).Err()
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Redis Set error for key %s: %v", key, err)
		return err
	}

	return nil
}

// GetJSON retrieves and unmarshals JSON data from cache
// Uses json-iterator which handles ,string struct tags more flexibly than standard library
func (r *RedisCache) GetJSON(key string, dest interface{}) (bool, error) {
	if r == nil {
		return false, nil
	}

	data, err := r.Get(key)
	if err != nil {
		return false, err
	}
	if data == nil {
		return false, nil // Cache miss
	}

	// Use json-iterator which handles ,string tags more flexibly
	// It can unmarshal unquoted numbers into fields with ,string tags
	var json = jsoniter.ConfigCompatibleWithStandardLibrary
	if err := json.Unmarshal(data, dest); err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to unmarshal cached data for key %s: %v", key, err)
		return false, err
	}

	return true, nil
}

// SetJSON marshals and stores JSON data in cache
// Uses json-iterator for consistency with GetJSON
func (r *RedisCache) SetJSON(key string, data interface{}, ttl time.Duration) error {
	if r == nil {
		return nil
	}

	// Use json-iterator for consistency with GetJSON
	var json = jsoniter.ConfigCompatibleWithStandardLibrary
	jsonData, err := json.Marshal(data)
	if err != nil {
		return err
	}

	return r.Set(key, jsonData, ttl)
}

// Close closes the Redis connection
func (r *RedisCache) Close() error {
	if r == nil || r.client == nil {
		return nil
	}

	return r.client.Close()
}

// RefreshMetadata tracks refresh attempts and retry counts
type RefreshMetadata struct {
	RetryCount    int       `json:"retry_count"`
	LastAttempt   time.Time `json:"last_attempt"`
	NextRefresh   time.Time `json:"next_refresh"`
	LastSuccess   time.Time `json:"last_success"`
}

// GetRefreshMetadata retrieves refresh metadata for a cache key
func (r *RedisCache) GetRefreshMetadata(key string) (*RefreshMetadata, error) {
	if r == nil {
		return nil, nil
	}

	metaKey := r.metaKey(key)
	var meta RefreshMetadata
	found, err := r.GetJSON(metaKey, &meta)
	if err != nil {
		return nil, err
	}
	if !found {
		return nil, nil
	}
	return &meta, nil
}

// SetRefreshMetadata stores refresh metadata for a cache key
func (r *RedisCache) SetRefreshMetadata(key string, meta *RefreshMetadata, ttl time.Duration) error {
	if r == nil {
		return nil
	}

	metaKey := r.metaKey(key)
	return r.SetJSON(metaKey, meta, ttl)
}

// IncrementRetryCount increments the retry count for a cache key
func (r *RedisCache) IncrementRetryCount(key string) (int, error) {
	if r == nil {
		return 0, nil
	}

	meta, err := r.GetRefreshMetadata(key)
	if err != nil {
		return 0, err
	}
	if meta == nil {
		meta = &RefreshMetadata{
			LastAttempt: time.Now(),
		}
	}

	meta.RetryCount++
	meta.LastAttempt = time.Now()

	// Store with same TTL as the cache item (1 week)
	ttl := 7 * 24 * time.Hour
	if err := r.SetRefreshMetadata(key, meta, ttl); err != nil {
		return meta.RetryCount, err
	}

	return meta.RetryCount, nil
}

// ResetRetryCount resets the retry count and updates last success time
func (r *RedisCache) ResetRetryCount(key string) error {
	if r == nil {
		return nil
	}

	meta, err := r.GetRefreshMetadata(key)
	if err != nil {
		return err
	}
	if meta == nil {
		meta = &RefreshMetadata{}
	}

	meta.RetryCount = 0
	meta.LastSuccess = time.Now()
	meta.LastAttempt = time.Now()

	ttl := 7 * 24 * time.Hour
	return r.SetRefreshMetadata(key, meta, ttl)
}

// DeleteCacheKey removes both the cache key and its metadata
func (r *RedisCache) DeleteCacheKey(key string) error {
	if r == nil {
		return nil
	}

	metaKey := r.metaKey(key)
	if err := r.client.Del(r.ctx, key, metaKey).Err(); err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to delete cache key %s: %v", key, err)
		return err
	}

	return nil
}

// GetAllCacheKeys returns all cache keys matching a pattern
func (r *RedisCache) GetAllCacheKeys(pattern string) ([]string, error) {
	if r == nil {
		return nil, nil
	}

	keys, err := r.client.Keys(r.ctx, pattern).Result()
	if err != nil {
		return nil, err
	}

	// Filter out metadata keys
	result := make([]string, 0, len(keys))
	for _, key := range keys {
		if !strings.HasSuffix(key, ":meta") {
			result = append(result, key)
		}
	}

	return result, nil
}

// metaKey returns the metadata key for a cache key
func (r *RedisCache) metaKey(key string) string {
	return key + ":meta"
}

