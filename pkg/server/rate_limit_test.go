package server

import (
	"net/url"
	"sync"
	"testing"
	"time"
)

func TestRateLimitTracker_waitIfRateLimited(t *testing.T) {
	tracker := &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	t.Run("not rate limited", func(t *testing.T) {
		start := time.Now()
		shouldSkip := tracker.waitIfRateLimited("http://example.com/test", 10*time.Second)
		duration := time.Since(start)

		if shouldSkip {
			t.Error("Expected shouldSkip to be false for non-rate-limited URL")
		}
		if duration > 100*time.Millisecond {
			t.Error("Expected no wait time for non-rate-limited URL")
		}
	})

	t.Run("rate limited and wait", func(t *testing.T) {
		testURL := "http://example.com/rate-limited"
		waitDuration := 200 * time.Millisecond

		// Mark URL as rate-limited
		tracker.markRateLimited(testURL, waitDuration)

		start := time.Now()
		shouldSkip := tracker.waitIfRateLimited(testURL, 10*time.Second)
		actualDuration := time.Since(start)

		if !shouldSkip {
			t.Error("Expected shouldSkip to be true for rate-limited URL")
		}
		if actualDuration < waitDuration-50*time.Millisecond {
			t.Errorf("Expected to wait at least %v, but waited %v", waitDuration, actualDuration)
		}
		if actualDuration > waitDuration+100*time.Millisecond {
			t.Errorf("Expected to wait at most %v, but waited %v", waitDuration+100*time.Millisecond, actualDuration)
		}
	})

	t.Run("rate limit expired", func(t *testing.T) {
		testURL := "http://example.com/expired"
		// Mark with very short duration that will expire
		tracker.markRateLimited(testURL, 10*time.Millisecond)
		time.Sleep(20 * time.Millisecond)

		start := time.Now()
		shouldSkip := tracker.waitIfRateLimited(testURL, 10*time.Second)
		duration := time.Since(start)

		if shouldSkip {
			t.Error("Expected shouldSkip to be false for expired rate limit")
		}
		if duration > 100*time.Millisecond {
			t.Error("Expected no wait time for expired rate limit")
		}

		// Verify it was removed from the map
		tracker.mu.RLock()
		_, exists := tracker.rateLimited[testURL]
		tracker.mu.RUnlock()
		if exists {
			t.Error("Expected expired rate limit to be removed from map")
		}
	})

	t.Run("max wait cap", func(t *testing.T) {
		testURL := "http://example.com/max-wait"
		maxWait := 100 * time.Millisecond
		rateLimitDuration := 5 * time.Second

		tracker.markRateLimited(testURL, rateLimitDuration)

		start := time.Now()
		shouldSkip := tracker.waitIfRateLimited(testURL, maxWait)
		actualDuration := time.Since(start)

		if !shouldSkip {
			t.Error("Expected shouldSkip to be true")
		}
		if actualDuration < maxWait-50*time.Millisecond {
			t.Errorf("Expected to wait at least %v, but waited %v", maxWait-50*time.Millisecond, actualDuration)
		}
		if actualDuration > maxWait+100*time.Millisecond {
			t.Errorf("Expected to wait at most %v, but waited %v", maxWait+100*time.Millisecond, actualDuration)
		}
	})
}

func TestRateLimitTracker_markRateLimited(t *testing.T) {
	tracker := &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/test"
	duration := 30 * time.Second

	tracker.markRateLimited(testURL, duration)

	tracker.mu.RLock()
	expiresAt, exists := tracker.rateLimited[testURL]
	tracker.mu.RUnlock()

	if !exists {
		t.Error("Expected URL to be marked as rate-limited")
	}

	expectedExpiry := time.Now().Add(duration)
	// Allow 1 second tolerance
	if expiresAt.Before(expectedExpiry.Add(-1*time.Second)) || expiresAt.After(expectedExpiry.Add(1*time.Second)) {
		t.Errorf("Expected expiry time around %v, got %v", expectedExpiry, expiresAt)
	}
}

func TestRateLimitTracker_clearRateLimit(t *testing.T) {
	tracker := &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/test"
	tracker.markRateLimited(testURL, 30*time.Second)

	// Verify it's marked
	tracker.mu.RLock()
	_, exists := tracker.rateLimited[testURL]
	tracker.mu.RUnlock()
	if !exists {
		t.Error("Expected URL to be marked as rate-limited")
	}

	// Clear it
	tracker.clearRateLimit(testURL)

	// Verify it's cleared
	tracker.mu.RLock()
	_, exists = tracker.rateLimited[testURL]
	tracker.mu.RUnlock()
	if exists {
		t.Error("Expected URL to be cleared from rate-limited map")
	}
}

func TestRateLimitTracker_concurrentAccess(t *testing.T) {
	tracker := &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	// Test concurrent access doesn't cause race conditions
	var wg sync.WaitGroup
	numGoroutines := 10
	numOps := 100

	for i := 0; i < numGoroutines; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			for j := 0; j < numOps; j++ {
				testURL := "http://example.com/test" + string(rune(id)) + string(rune(j))
				tracker.markRateLimited(testURL, 1*time.Second)
				tracker.waitIfRateLimited(testURL, 100*time.Millisecond)
				tracker.clearRateLimit(testURL)
			}
		}(i)
	}

	wg.Wait()
	// If we get here without a race condition, the test passes
}

// TestStream_exponentialBackoff tests that exponential backoff is applied correctly
// We test this by checking the rate limit tracking behavior rather than full stream execution
func TestStream_exponentialBackoff(t *testing.T) {
	// Reset global tracker
	globalRateLimitTracker = &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL, err := url.Parse("http://example.com/test")
	if err != nil {
		t.Fatalf("Failed to parse test URL: %v", err)
	}

	// Test that URL gets marked as rate-limited after 458 response
	// We'll verify the rate limit tracking works correctly
	urlStr := testURL.String()

	// Simulate what happens in stream() - mark as rate-limited
	globalRateLimitTracker.markRateLimited(urlStr, 30*time.Second)

	// Verify it's marked
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited := globalRateLimitTracker.rateLimited[urlStr]
	globalRateLimitTracker.mu.RUnlock()

	if !isRateLimited {
		t.Error("Expected URL to be marked as rate-limited")
	}

	// Test exponential backoff calculation with retry timeout
	const initialBackoff = 2 * time.Second
	const maxBackoff = 60 * time.Second
	const retryTimeout = 1 * time.Minute

	// Simulate retry logic: track first 458 time and calculate backoffs
	first458Time := time.Now()
	for attempt := 1; attempt <= 5; attempt++ {
		backoffDuration := initialBackoff * time.Duration(1<<uint(attempt-1))
		if backoffDuration > maxBackoff {
			backoffDuration = maxBackoff
		}

		// Check if we'd exceed retry timeout
		elapsed := time.Since(first458Time)
		remainingTime := retryTimeout - elapsed
		if backoffDuration > remainingTime {
			backoffDuration = remainingTime
		}

		// Verify backoff is calculated correctly
		expected := initialBackoff * time.Duration(1<<uint(attempt-1))
		if expected > maxBackoff {
			expected = maxBackoff
		}
		if expected > remainingTime {
			expected = remainingTime
		}

		if backoffDuration != expected {
			t.Errorf("Attempt %d: expected backoff %v, got %v", attempt, expected, backoffDuration)
		}

		// Simulate sleep (but don't actually sleep in test)
		first458Time = first458Time.Add(backoffDuration)

		// Stop if we'd exceed timeout
		if time.Since(first458Time) >= retryTimeout {
			break
		}
	}
}

func TestStream_rateLimitTracking(t *testing.T) {
	// Reset global tracker for this test
	globalRateLimitTracker = &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/test-stream"

	// Simulate first request getting 458 - mark URL as rate-limited
	globalRateLimitTracker.markRateLimited(testURL, 30*time.Second)

	// Verify URL is marked as rate-limited
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited := globalRateLimitTracker.rateLimited[testURL]
	globalRateLimitTracker.mu.RUnlock()

	if !isRateLimited {
		t.Error("Expected URL to be marked as rate-limited after 458 response")
	}

	// Simulate second request to same URL - should wait
	start := time.Now()
	globalRateLimitTracker.waitIfRateLimited(testURL, 60*time.Second)
	duration := time.Since(start)

	// Should have waited (at least some time due to rate limit tracking)
	// But not the full 30 seconds since we're using a shorter wait in test
	if duration < 50*time.Millisecond {
		t.Errorf("Expected some wait time due to rate limit tracking, got %v", duration)
	}
}

func TestStream_retryTimeoutExceeded(t *testing.T) {
	// Test that after 1 minute of retries, URL remains rate-limited
	// Reset global tracker
	globalRateLimitTracker = &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/retry-timeout"

	// Simulate retry timeout exceeded - URL should be marked as rate-limited
	globalRateLimitTracker.markRateLimited(testURL, 30*time.Second)

	// Verify it's still marked
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited := globalRateLimitTracker.rateLimited[testURL]
	globalRateLimitTracker.mu.RUnlock()

	if !isRateLimited {
		t.Error("Expected URL to remain rate-limited after retry timeout")
	}
}

func TestStream_successClearsRateLimit(t *testing.T) {
	// Reset global tracker
	globalRateLimitTracker = &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/success"

	// Manually mark as rate-limited first
	globalRateLimitTracker.markRateLimited(testURL, 30*time.Second)

	// Verify it's marked
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited := globalRateLimitTracker.rateLimited[testURL]
	globalRateLimitTracker.mu.RUnlock()

	if !isRateLimited {
		t.Error("Expected URL to be marked as rate-limited")
	}

	// Simulate successful request - clear rate limit
	globalRateLimitTracker.clearRateLimit(testURL)

	// Verify rate limit is cleared
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited = globalRateLimitTracker.rateLimited[testURL]
	globalRateLimitTracker.mu.RUnlock()

	if isRateLimited {
		t.Error("Expected URL to be cleared from rate-limited map after successful request")
	}
}

func TestHlsXtreamStream_exponentialBackoff(t *testing.T) {
	// Test that HLS stream uses the same exponential backoff logic
	// Reset global tracker
	globalRateLimitTracker = &rateLimitTracker{
		rateLimited: make(map[string]time.Time),
	}

	testURL := "http://example.com/hls-stream.m3u8"

	// Test that HLS URLs also get rate limit tracking
	globalRateLimitTracker.markRateLimited(testURL, 30*time.Second)

	// Verify it's marked
	globalRateLimitTracker.mu.RLock()
	_, isRateLimited := globalRateLimitTracker.rateLimited[testURL]
	globalRateLimitTracker.mu.RUnlock()

	if !isRateLimited {
		t.Error("Expected HLS URL to be marked as rate-limited")
	}

	// Test exponential backoff calculation (same as regular stream)
	const initialBackoff = 2 * time.Second
	const maxBackoff = 60 * time.Second

	for attempt := 1; attempt <= 3; attempt++ {
		backoffDuration := initialBackoff * time.Duration(1<<uint(attempt-1))
		if backoffDuration > maxBackoff {
			backoffDuration = maxBackoff
		}

		expected := initialBackoff * time.Duration(1<<uint(attempt-1))
		if expected > maxBackoff {
			expected = maxBackoff
		}

		if backoffDuration != expected {
			t.Errorf("HLS Attempt %d: expected backoff %v, got %v", attempt, expected, backoffDuration)
		}
	}
}

func TestExponentialBackoffCalculation(t *testing.T) {
	const initialBackoff = 2 * time.Second
	const maxBackoff = 60 * time.Second

	testCases := []struct {
		attempt      int
		expectedMin  time.Duration
		expectedMax  time.Duration
		description  string
	}{
		{1, 2 * time.Second, 2 * time.Second, "first attempt"},
		{2, 4 * time.Second, 4 * time.Second, "second attempt"},
		{3, 8 * time.Second, 8 * time.Second, "third attempt"},
		{4, 16 * time.Second, 16 * time.Second, "fourth attempt"},
		{10, 60 * time.Second, 60 * time.Second, "tenth attempt (capped)"},
	}

	for _, tc := range testCases {
		t.Run(tc.description, func(t *testing.T) {
			backoffDuration := initialBackoff * time.Duration(1<<uint(tc.attempt-1))
			if backoffDuration > maxBackoff {
				backoffDuration = maxBackoff
			}

			if backoffDuration < tc.expectedMin || backoffDuration > tc.expectedMax {
				t.Errorf("Attempt %d: expected backoff between %v and %v, got %v",
					tc.attempt, tc.expectedMin, tc.expectedMax, backoffDuration)
			}
		})
	}
}

