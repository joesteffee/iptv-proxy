package server

import (
	"encoding/json"
	"os"
	"testing"
	"time"
)

// setupTestRedis creates a test Redis cache instance
// Returns nil if REDIS_URL is not set (skips tests)
func setupTestRedis(t *testing.T) *RedisCache {
	redisURL := os.Getenv("REDIS_URL")
	if redisURL == "" {
		// Use default test Redis URL
		redisURL = "redis://localhost:6379/1" // Use database 1 for tests
	}

	cache, err := NewRedisCache(redisURL)
	if err != nil {
		t.Skipf("Skipping test: Redis not available: %v", err)
		return nil
	}

	// Clean up test keys
	keys, _ := cache.GetAllCacheKeys("test:*")
	for _, key := range keys {
		cache.DeleteCacheKey(key)
	}

	return cache
}

func TestNewRedisCache(t *testing.T) {
	t.Run("empty URL returns nil", func(t *testing.T) {
		cache, err := NewRedisCache("")
		if err != nil {
			t.Errorf("Expected no error for empty URL, got: %v", err)
		}
		if cache != nil {
			t.Error("Expected nil cache for empty URL")
		}
	})

	t.Run("invalid URL returns error", func(t *testing.T) {
		cache, err := NewRedisCache("invalid://url")
		if err == nil {
			t.Error("Expected error for invalid URL")
		}
		if cache != nil {
			t.Error("Expected nil cache for invalid URL")
		}
	})

	t.Run("valid URL creates cache", func(t *testing.T) {
		cache := setupTestRedis(t)
		if cache == nil {
			return
		}
		defer cache.Close()
	})
}

func TestRedisCache_GetSet(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	key := "test:getset"
	value := []byte("test value")
	ttl := 1 * time.Hour

	t.Run("set and get", func(t *testing.T) {
		err := cache.Set(key, value, ttl)
		if err != nil {
			t.Fatalf("Set failed: %v", err)
		}

		result, err := cache.Get(key)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}

		if string(result) != string(value) {
			t.Errorf("Expected %q, got %q", string(value), string(result))
		}
	})

	t.Run("get non-existent key", func(t *testing.T) {
		result, err := cache.Get("test:nonexistent")
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}
		if result != nil {
			t.Errorf("Expected nil for non-existent key, got %q", string(result))
		}
	})

	t.Run("set overwrites existing", func(t *testing.T) {
		newValue := []byte("new value")
		err := cache.Set(key, newValue, ttl)
		if err != nil {
			t.Fatalf("Set failed: %v", err)
		}

		result, err := cache.Get(key)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}

		if string(result) != string(newValue) {
			t.Errorf("Expected %q, got %q", string(newValue), string(result))
		}
	})
}

func TestRedisCache_GetJSONSetJSON(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	type TestStruct struct {
		Name  string `json:"name"`
		Value int    `json:"value"`
	}

	key := "test:json"
	ttl := 1 * time.Hour

	t.Run("set and get JSON", func(t *testing.T) {
		original := TestStruct{Name: "test", Value: 42}

		err := cache.SetJSON(key, original, ttl)
		if err != nil {
			t.Fatalf("SetJSON failed: %v", err)
		}

		var result TestStruct
		found, err := cache.GetJSON(key, &result)
		if err != nil {
			t.Fatalf("GetJSON failed: %v", err)
		}
		if !found {
			t.Error("Expected to find cached JSON")
		}

		if result.Name != original.Name || result.Value != original.Value {
			t.Errorf("Expected %+v, got %+v", original, result)
		}
	})

	t.Run("get JSON non-existent", func(t *testing.T) {
		var result TestStruct
		found, err := cache.GetJSON("test:nonexistent", &result)
		if err != nil {
			t.Fatalf("GetJSON failed: %v", err)
		}
		if found {
			t.Error("Expected not to find non-existent JSON")
		}
	})
}

func TestRedisCache_RefreshMetadata(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	key := "test:metadata"
	ttl := 1 * time.Hour

	t.Run("get non-existent metadata", func(t *testing.T) {
		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta != nil {
			t.Error("Expected nil metadata for non-existent key")
		}
	})

	t.Run("set and get metadata", func(t *testing.T) {
		original := &RefreshMetadata{
			RetryCount:  2,
			LastAttempt: time.Now(),
			NextRefresh: time.Now().Add(1 * time.Hour),
			LastSuccess: time.Now().Add(-2 * time.Hour),
		}

		err := cache.SetRefreshMetadata(key, original, ttl)
		if err != nil {
			t.Fatalf("SetRefreshMetadata failed: %v", err)
		}

		result, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if result == nil {
			t.Fatal("Expected metadata, got nil")
		}

		if result.RetryCount != original.RetryCount {
			t.Errorf("Expected RetryCount %d, got %d", original.RetryCount, result.RetryCount)
		}
	})
}

func TestRedisCache_IncrementRetryCount(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	key := "test:retry"

	t.Run("increment from zero", func(t *testing.T) {
		count, err := cache.IncrementRetryCount(key)
		if err != nil {
			t.Fatalf("IncrementRetryCount failed: %v", err)
		}
		if count != 1 {
			t.Errorf("Expected retry count 1, got %d", count)
		}

		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta == nil {
			t.Fatal("Expected metadata after increment")
		}
		if meta.RetryCount != 1 {
			t.Errorf("Expected RetryCount 1, got %d", meta.RetryCount)
		}
	})

	t.Run("increment multiple times", func(t *testing.T) {
		count1, err := cache.IncrementRetryCount(key)
		if err != nil {
			t.Fatalf("IncrementRetryCount failed: %v", err)
		}

		count2, err := cache.IncrementRetryCount(key)
		if err != nil {
			t.Fatalf("IncrementRetryCount failed: %v", err)
		}

		if count2 != count1+1 {
			t.Errorf("Expected count to increment, got %d -> %d", count1, count2)
		}
	})
}

func TestRedisCache_ResetRetryCount(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	key := "test:reset"

	// Set up initial retry count
	_, err := cache.IncrementRetryCount(key)
	if err != nil {
		t.Fatalf("IncrementRetryCount failed: %v", err)
	}

	_, err = cache.IncrementRetryCount(key)
	if err != nil {
		t.Fatalf("IncrementRetryCount failed: %v", err)
	}

	t.Run("reset retry count", func(t *testing.T) {
		err := cache.ResetRetryCount(key)
		if err != nil {
			t.Fatalf("ResetRetryCount failed: %v", err)
		}

		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta == nil {
			t.Fatal("Expected metadata after reset")
		}

		if meta.RetryCount != 0 {
			t.Errorf("Expected RetryCount 0 after reset, got %d", meta.RetryCount)
		}

		if meta.LastSuccess.IsZero() {
			t.Error("Expected LastSuccess to be set after reset")
		}
	})
}

func TestRedisCache_DeleteCacheKey(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	key := "test:delete"
	ttl := 1 * time.Hour

	// Set up cache key and metadata
	err := cache.Set(key, []byte("value"), ttl)
	if err != nil {
		t.Fatalf("Set failed: %v", err)
	}

	meta := &RefreshMetadata{RetryCount: 1}
	err = cache.SetRefreshMetadata(key, meta, ttl)
	if err != nil {
		t.Fatalf("SetRefreshMetadata failed: %v", err)
	}

	t.Run("delete removes key and metadata", func(t *testing.T) {
		err := cache.DeleteCacheKey(key)
		if err != nil {
			t.Fatalf("DeleteCacheKey failed: %v", err)
		}

		// Check key is deleted
		result, err := cache.Get(key)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}
		if result != nil {
			t.Error("Expected key to be deleted")
		}

		// Check metadata is deleted
		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta != nil {
			t.Error("Expected metadata to be deleted")
		}
	})
}

func TestRedisCache_GetAllCacheKeys(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}
	defer cache.Close()

	// Clean up any existing test keys
	existing, _ := cache.GetAllCacheKeys("test:keys:*")
	for _, key := range existing {
		cache.DeleteCacheKey(key)
	}

	ttl := 1 * time.Hour

	// Create test keys
	keys := []string{"test:keys:1", "test:keys:2", "test:keys:3"}
	for _, key := range keys {
		err := cache.Set(key, []byte("value"), ttl)
		if err != nil {
			t.Fatalf("Set failed for %s: %v", key, err)
		}
	}

	t.Run("get all keys with pattern", func(t *testing.T) {
		result, err := cache.GetAllCacheKeys("test:keys:*")
		if err != nil {
			t.Fatalf("GetAllCacheKeys failed: %v", err)
		}

		if len(result) != len(keys) {
			t.Errorf("Expected %d keys, got %d", len(keys), len(result))
		}

		// Check all keys are present
		found := make(map[string]bool)
		for _, key := range result {
			found[key] = true
		}
		for _, key := range keys {
			if !found[key] {
				t.Errorf("Expected to find key %s", key)
			}
		}
	})

	t.Run("pattern excludes metadata keys", func(t *testing.T) {
		// Add metadata for one key
		meta := &RefreshMetadata{RetryCount: 1}
		err := cache.SetRefreshMetadata("test:keys:1", meta, ttl)
		if err != nil {
			t.Fatalf("SetRefreshMetadata failed: %v", err)
		}

		result, err := cache.GetAllCacheKeys("test:keys:*")
		if err != nil {
			t.Fatalf("GetAllCacheKeys failed: %v", err)
		}

		// Should still only return the 3 keys, not metadata
		if len(result) != len(keys) {
			t.Errorf("Expected %d keys (excluding metadata), got %d", len(keys), len(result))
		}

		// Verify no metadata keys
		for _, key := range result {
			if key == "test:keys:1:meta" {
				t.Error("Expected metadata keys to be excluded")
			}
		}
	})

	// Cleanup
	for _, key := range keys {
		cache.DeleteCacheKey(key)
	}
}

func TestRedisCache_Close(t *testing.T) {
	cache := setupTestRedis(t)
	if cache == nil {
		return
	}

	err := cache.Close()
	if err != nil {
		t.Errorf("Close failed: %v", err)
	}

	// Close again should not error
	err = cache.Close()
	if err != nil {
		t.Errorf("Second Close failed: %v", err)
	}
}

func TestRedisCache_NilSafety(t *testing.T) {
	var cache *RedisCache = nil

	t.Run("nil cache Get", func(t *testing.T) {
		result, err := cache.Get("test")
		if err != nil {
			t.Errorf("Expected no error for nil cache, got: %v", err)
		}
		if result != nil {
			t.Error("Expected nil result for nil cache")
		}
	})

	t.Run("nil cache Set", func(t *testing.T) {
		err := cache.Set("test", []byte("value"), time.Hour)
		if err != nil {
			t.Errorf("Expected no error for nil cache, got: %v", err)
		}
	})

	t.Run("nil cache GetJSON", func(t *testing.T) {
		var dest interface{}
		found, err := cache.GetJSON("test", &dest)
		if err != nil {
			t.Errorf("Expected no error for nil cache, got: %v", err)
		}
		if found {
			t.Error("Expected not found for nil cache")
		}
	})

	t.Run("nil cache SetJSON", func(t *testing.T) {
		err := cache.SetJSON("test", map[string]string{"key": "value"}, time.Hour)
		if err != nil {
			t.Errorf("Expected no error for nil cache, got: %v", err)
		}
	})

	t.Run("nil cache Close", func(t *testing.T) {
		err := cache.Close()
		if err != nil {
			t.Errorf("Expected no error for nil cache Close, got: %v", err)
		}
	})
}

func TestRefreshMetadata_JSON(t *testing.T) {
	// Test that RefreshMetadata can be marshaled/unmarshaled
	original := &RefreshMetadata{
		RetryCount:  3,
		LastAttempt: time.Now(),
		NextRefresh: time.Now().Add(1 * time.Hour),
		LastSuccess: time.Now().Add(-2 * time.Hour),
	}

	data, err := json.Marshal(original)
	if err != nil {
		t.Fatalf("Marshal failed: %v", err)
	}

	var result RefreshMetadata
	err = json.Unmarshal(data, &result)
	if err != nil {
		t.Fatalf("Unmarshal failed: %v", err)
	}

	if result.RetryCount != original.RetryCount {
		t.Errorf("Expected RetryCount %d, got %d", original.RetryCount, result.RetryCount)
	}
}

