package server

import (
	"errors"
	"sync"
	"testing"
	"time"
)

func setupTestRefreshWorker(t *testing.T) (*CacheRefreshWorker, *RedisCache) {
	cache := setupTestRedis(t)
	if cache == nil {
		return nil, nil
	}

	config := &Config{
		redisCache: cache,
	}

	worker := NewCacheRefreshWorker(cache, config)
	return worker, cache
}

func TestNewCacheRefreshWorker(t *testing.T) {
	t.Run("nil cache returns nil worker", func(t *testing.T) {
		worker := NewCacheRefreshWorker(nil, nil)
		if worker != nil {
			t.Error("Expected nil worker for nil cache")
		}
	})

	t.Run("valid cache creates worker", func(t *testing.T) {
		worker, cache := setupTestRefreshWorker(t)
		if worker == nil {
			return
		}
		defer cache.Close()

		if worker.cache == nil {
			t.Error("Expected worker to have cache")
		}
		if worker.config == nil {
			t.Error("Expected worker to have config")
		}
		if worker.callbacks == nil {
			t.Error("Expected worker to have callbacks map")
		}
	})
}

func TestCacheRefreshWorker_RegisterCallback(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	callback := func(key string) error {
		return nil
	}

	worker.RegisterCallback("test:pattern:*", callback)

	worker.mu.RLock()
	registered, exists := worker.callbacks["test:pattern:*"]
	worker.mu.RUnlock()

	if !exists {
		t.Error("Expected callback to be registered")
	}
	if registered == nil {
		t.Error("Expected callback to not be nil")
	}
}

func TestCacheRefreshWorker_StartStop(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	// Start worker
	worker.Start()

	// Give it a moment to start
	time.Sleep(100 * time.Millisecond)

	// Stop worker
	worker.Stop()

	// Verify it stopped
	select {
	case <-worker.stopCh:
		// Good, channel is closed
	default:
		t.Error("Expected stopCh to be closed")
	}
}

func TestCacheRefreshWorker_calculateRefreshTime(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	key := "test:refresh:time"
	ttl := 7 * 24 * time.Hour

	// Calculate refresh time multiple times - should be consistent
	time1 := worker.calculateRefreshTime(key, ttl)
	time2 := worker.calculateRefreshTime(key, ttl)

	// Should be the same (within a small margin for time.Now() differences)
	diff := time1.Sub(time2)
	if diff < -time.Second || diff > time.Second {
		t.Errorf("Expected refresh times to be consistent, got diff: %v", diff)
	}

	// Should be in the refresh window (last 20% of TTL)
	refreshWindow := time.Duration(float64(ttl) * 0.2)
	now := time.Now()
	expectedStart := now.Add(time.Duration(float64(ttl) * 0.8))
	expectedEnd := expectedStart.Add(refreshWindow)

	if time1.Before(expectedStart) || time1.After(expectedEnd) {
		t.Errorf("Refresh time %v should be between %v and %v", time1, expectedStart, expectedEnd)
	}
}

func TestCacheRefreshWorker_keyMatchesPattern(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	tests := []struct {
		name    string
		key     string
		pattern string
		matches bool
	}{
		{"exact match", "test:key", "test:key", true},
		{"no match", "test:key", "test:other", false},
		{"wildcard match", "test:key:123", "test:key:*", true},
		{"wildcard no match", "test:other:123", "test:key:*", false},
		{"wildcard prefix", "test:key:123", "test:*:123", true},
		{"wildcard middle", "test:key:value", "test:*:value", true},
		{"trailing wildcard", "test:key:123", "test:*", true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := worker.keyMatchesPattern(tt.key, tt.pattern)
			if result != tt.matches {
				t.Errorf("keyMatchesPattern(%q, %q) = %v, expected %v", tt.key, tt.pattern, result, tt.matches)
			}
		})
	}
}

func TestCacheRefreshWorker_checkAndRefreshKey(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	key := "test:refresh:key"
	ttl := 1 * time.Hour

	// Set up a cache key
	err := cache.Set(key, []byte("test value"), ttl)
	if err != nil {
		t.Fatalf("Set failed: %v", err)
	}

	t.Run("refresh with success callback", func(t *testing.T) {
		callbackCalled := false
		callback := func(k string) error {
			if k != key {
				t.Errorf("Expected key %s, got %s", key, k)
			}
			callbackCalled = true
			return nil
		}

		worker.checkAndRefreshKey(key, callback)

		if !callbackCalled {
			t.Error("Expected callback to be called")
		}

		// Check metadata was updated
		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta == nil {
			t.Fatal("Expected metadata after refresh")
		}
		if meta.RetryCount != 0 {
			t.Errorf("Expected RetryCount 0 after successful refresh, got %d", meta.RetryCount)
		}
	})

	t.Run("refresh with failure callback", func(t *testing.T) {
		// Reset retry count
		cache.ResetRetryCount(key)

		callbackCalled := false
		callback := func(k string) error {
			callbackCalled = true
			return errors.New("refresh failed")
		}

		worker.checkAndRefreshKey(key, callback)

		if !callbackCalled {
			t.Error("Expected callback to be called")
		}

		// Check retry count was incremented
		meta, err := cache.GetRefreshMetadata(key)
		if err != nil {
			t.Fatalf("GetRefreshMetadata failed: %v", err)
		}
		if meta == nil {
			t.Fatal("Expected metadata after failed refresh")
		}
		if meta.RetryCount != 1 {
			t.Errorf("Expected RetryCount 1 after failed refresh, got %d", meta.RetryCount)
		}
	})

	t.Run("remove after max retries", func(t *testing.T) {
		// Set retry count to max
		for i := 0; i < maxRetryAttempts; i++ {
			_, err := cache.IncrementRetryCount(key)
			if err != nil {
				t.Fatalf("IncrementRetryCount failed: %v", err)
			}
		}

		callback := func(k string) error {
			return errors.New("refresh failed")
		}

		worker.checkAndRefreshKey(key, callback)

		// Check key was deleted
		result, err := cache.Get(key)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}
		if result != nil {
			t.Error("Expected key to be deleted after max retries")
		}
	})

	// Cleanup
	cache.DeleteCacheKey(key)
}

func TestCacheRefreshWorker_refreshExpiringItems(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	// Create test keys
	keys := []string{"test:refresh:1", "test:refresh:2"}
	ttl := 1 * time.Hour

	for _, key := range keys {
		err := cache.Set(key, []byte("value"), ttl)
		if err != nil {
			t.Fatalf("Set failed for %s: %v", key, err)
		}
	}

	callbackCalls := make(map[string]int)
	var mu sync.Mutex

	callback := func(key string) error {
		mu.Lock()
		callbackCalls[key]++
		mu.Unlock()
		return nil
	}

	worker.RegisterCallback("test:refresh:*", callback)

	// Run refresh check
	worker.refreshExpiringItems()

	// Give it a moment
	time.Sleep(100 * time.Millisecond)

	// Check callbacks were called
	mu.Lock()
	if len(callbackCalls) == 0 {
		t.Error("Expected callbacks to be called")
	}
	mu.Unlock()

	// Cleanup
	for _, key := range keys {
		cache.DeleteCacheKey(key)
	}
}

func TestCacheRefreshWorker_concurrentAccess(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	key := "test:concurrent"
	ttl := 1 * time.Hour

	err := cache.Set(key, []byte("value"), ttl)
	if err != nil {
		t.Fatalf("Set failed: %v", err)
	}

	var wg sync.WaitGroup
	numGoroutines := 10

	callback := func(k string) error {
		return nil
	}

	// Test concurrent access to checkAndRefreshKey
	for i := 0; i < numGoroutines; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			worker.checkAndRefreshKey(key, callback)
		}()
	}

	wg.Wait()

	// If we get here without a race condition, the test passes
	cache.DeleteCacheKey(key)
}

func TestCacheRefreshWorker_respectsMinRetryInterval(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	key := "test:retry:interval"
	ttl := 1 * time.Hour

	err := cache.Set(key, []byte("value"), ttl)
	if err != nil {
		t.Fatalf("Set failed: %v", err)
	}

	// Set metadata with recent last attempt
	meta := &RefreshMetadata{
		RetryCount:  1,
		LastAttempt: time.Now(),
		NextRefresh: time.Now().Add(-1 * time.Hour), // Past refresh time
	}
	err = cache.SetRefreshMetadata(key, meta, ttl)
	if err != nil {
		t.Fatalf("SetRefreshMetadata failed: %v", err)
	}

	callbackCalled := false
	callback := func(k string) error {
		callbackCalled = true
		return errors.New("refresh failed")
	}

	// Should not refresh because minRetryInterval hasn't passed
	worker.checkAndRefreshKey(key, callback)

	if callbackCalled {
		t.Error("Expected callback not to be called before minRetryInterval")
	}

	// Update last attempt to be past minRetryInterval
	meta.LastAttempt = time.Now().Add(-2 * time.Hour)
	err = cache.SetRefreshMetadata(key, meta, ttl)
	if err != nil {
		t.Fatalf("SetRefreshMetadata failed: %v", err)
	}

	callbackCalled = false
	worker.checkAndRefreshKey(key, callback)

	if !callbackCalled {
		t.Error("Expected callback to be called after minRetryInterval")
	}

	cache.DeleteCacheKey(key)
}

func TestCacheRefreshWorker_handlesExpiredKeys(t *testing.T) {
	worker, cache := setupTestRefreshWorker(t)
	if worker == nil {
		return
	}
	defer cache.Close()

	key := "test:expired"
	// Set key with very short TTL
	err := cache.Set(key, []byte("value"), 100*time.Millisecond)
	if err != nil {
		t.Fatalf("Set failed: %v", err)
	}

	// Wait for key to expire
	time.Sleep(200 * time.Millisecond)

	callbackCalled := false
	callback := func(k string) error {
		callbackCalled = true
		return nil
	}

	// Should skip expired keys
	worker.checkAndRefreshKey(key, callback)

	if callbackCalled {
		t.Error("Expected callback not to be called for expired key")
	}
}

func TestCacheRefreshWorker_nilSafety(t *testing.T) {
	var worker *CacheRefreshWorker = nil

	t.Run("nil worker RegisterCallback", func(t *testing.T) {
		// Should not panic
		worker.RegisterCallback("test", func(string) error { return nil })
	})

	t.Run("nil worker Start", func(t *testing.T) {
		// Should not panic
		worker.Start()
	})

	t.Run("nil worker Stop", func(t *testing.T) {
		// Should not panic
		worker.Stop()
	})
}

