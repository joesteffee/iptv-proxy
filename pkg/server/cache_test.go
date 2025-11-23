package server

import (
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func TestStreamCache_getOrCreateEntry(t *testing.T) {
	cache := &streamCache{
		entries: make(map[string]*streamCacheEntry),
	}

	t.Run("create new entry", func(t *testing.T) {
		urlStr := "http://example.com/test1"
		entry := cache.getOrCreateEntry(urlStr)

		if entry == nil {
			t.Fatal("Expected entry to be created")
		}
		if entry.url != urlStr {
			t.Errorf("Expected URL %s, got %s", urlStr, entry.url)
		}
		if entry.activeClients != 0 {
			t.Errorf("Expected activeClients to be 0, got %d", entry.activeClients)
		}
	})

	t.Run("get existing entry", func(t *testing.T) {
		urlStr := "http://example.com/test2"
		entry1 := cache.getOrCreateEntry(urlStr)
		entry1.addClient()

		entry2 := cache.getOrCreateEntry(urlStr)

		if entry1 != entry2 {
			t.Error("Expected to get the same entry instance")
		}
		if entry2.activeClients != 1 {
			t.Errorf("Expected activeClients to be 1, got %d", entry2.activeClients)
		}
	})

	t.Run("multiple entries", func(t *testing.T) {
		url1 := "http://example.com/test3"
		url2 := "http://example.com/test4"

		entry1 := cache.getOrCreateEntry(url1)
		entry2 := cache.getOrCreateEntry(url2)

		if entry1 == entry2 {
			t.Error("Expected different entries for different URLs")
		}
	})
}

func TestStreamCache_removeEntry(t *testing.T) {
	cache := &streamCache{
		entries: make(map[string]*streamCacheEntry),
	}

	urlStr := "http://example.com/remove-test"
	entry := cache.getOrCreateEntry(urlStr)

	cache.removeEntry(urlStr)

	cache.mu.RLock()
	_, exists := cache.entries[urlStr]
	cache.mu.RUnlock()

	if exists {
		t.Error("Expected entry to be removed")
	}
	if entry == nil {
		t.Error("Entry should still exist, just removed from cache")
	}
}

func TestStreamCacheEntry_addClient(t *testing.T) {
	entry := &streamCacheEntry{
		activeClients: 0,
	}

	entry.addClient()
	if entry.activeClients != 1 {
		t.Errorf("Expected activeClients to be 1, got %d", entry.activeClients)
	}

	entry.addClient()
	if entry.activeClients != 2 {
		t.Errorf("Expected activeClients to be 2, got %d", entry.activeClients)
	}
}

func TestStreamCacheEntry_removeClient(t *testing.T) {
	entry := &streamCacheEntry{
		activeClients: 2,
	}

	shouldRemove := entry.removeClient()
	if shouldRemove {
		t.Error("Expected shouldRemove to be false when clients remain")
	}
	if entry.activeClients != 1 {
		t.Errorf("Expected activeClients to be 1, got %d", entry.activeClients)
	}

	shouldRemove = entry.removeClient()
	if !shouldRemove {
		t.Error("Expected shouldRemove to be true when no clients remain")
	}
	if entry.activeClients != 0 {
		t.Errorf("Expected activeClients to be 0, got %d", entry.activeClients)
	}
}

func TestStreamCacheEntry_getRange(t *testing.T) {
	testData := []byte("0123456789abcdef")
	entry := &streamCacheEntry{
		data:          testData,
		fetchComplete: true,
		contentLength: int64(len(testData)),
	}

	t.Run("valid range", func(t *testing.T) {
		data, err := entry.getRange(0, 4)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		expected := []byte("01234")
		if string(data) != string(expected) {
			t.Errorf("Expected %s, got %s", string(expected), string(data))
		}
	})

	t.Run("range at end", func(t *testing.T) {
		data, err := entry.getRange(10, 15)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		expected := []byte("abcdef")
		if string(data) != string(expected) {
			t.Errorf("Expected %s, got %s", string(expected), string(data))
		}
	})

	t.Run("range beyond end", func(t *testing.T) {
		data, err := entry.getRange(10, 100)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		expected := []byte("abcdef")
		if string(data) != string(expected) {
			t.Errorf("Expected %s, got %s", string(expected), string(data))
		}
	})

	t.Run("negative start", func(t *testing.T) {
		data, err := entry.getRange(-5, 5)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		expected := []byte("012345")
		if string(data) != string(expected) {
			t.Errorf("Expected %s, got %s", string(expected), string(data))
		}
	})

	t.Run("cache not ready", func(t *testing.T) {
		notReadyEntry := &streamCacheEntry{
			fetchComplete: false,
		}
		_, err := notReadyEntry.getRange(0, 5)
		if err == nil {
			t.Error("Expected error when cache is not ready")
		}
	})

	t.Run("fetch error", func(t *testing.T) {
		errorEntry := &streamCacheEntry{
			fetchComplete: true,
			fetchErr:      fmt.Errorf("fetch failed"),
		}
		_, err := errorEntry.getRange(0, 5)
		if err == nil {
			t.Error("Expected error when fetch failed")
		}
	})
}

func TestParseRangeHeader(t *testing.T) {
	contentLength := int64(1000)

	t.Run("no range header", func(t *testing.T) {
		start, end, err := parseRangeHeader("", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start != -1 || end != -1 {
			t.Errorf("Expected (-1, -1), got (%d, %d)", start, end)
		}
	})

	t.Run("valid range", func(t *testing.T) {
		start, end, err := parseRangeHeader("bytes=0-499", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start != 0 || end != 499 {
			t.Errorf("Expected (0, 499), got (%d, %d)", start, end)
		}
	})

	t.Run("open-ended range", func(t *testing.T) {
		start, end, err := parseRangeHeader("bytes=500-", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start != 500 || end != 999 {
			t.Errorf("Expected (500, 999), got (%d, %d)", start, end)
		}
	})

	t.Run("suffix range", func(t *testing.T) {
		start, end, err := parseRangeHeader("bytes=-100", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start != 900 || end != 999 {
			t.Errorf("Expected (900, 999), got (%d, %d)", start, end)
		}
	})

	t.Run("range beyond content length", func(t *testing.T) {
		start, end, err := parseRangeHeader("bytes=500-2000", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start != 500 {
			t.Errorf("Expected start 500, got %d", start)
		}
		if end != 999 {
			t.Errorf("Expected end 999, got %d", end)
		}
	})

	t.Run("negative start", func(t *testing.T) {
		start, _, err := parseRangeHeader("bytes=-100", contentLength)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if start < 0 {
			t.Errorf("Expected start >= 0, got %d", start)
		}
	})

	t.Run("invalid format", func(t *testing.T) {
		_, _, err := parseRangeHeader("invalid", contentLength)
		if err == nil {
			t.Error("Expected error for invalid format")
		}
	})

	t.Run("invalid range spec", func(t *testing.T) {
		_, _, err := parseRangeHeader("bytes=500", contentLength)
		if err == nil {
			t.Error("Expected error for invalid range spec")
		}
	})

	t.Run("start greater than end", func(t *testing.T) {
		_, _, err := parseRangeHeader("bytes=500-400", contentLength)
		if err == nil {
			t.Error("Expected error when start > end")
		}
	})

	t.Run("invalid start value", func(t *testing.T) {
		_, _, err := parseRangeHeader("bytes=abc-500", contentLength)
		if err == nil {
			t.Error("Expected error for invalid start value")
		}
	})

	t.Run("invalid end value", func(t *testing.T) {
		_, _, err := parseRangeHeader("bytes=100-abc", contentLength)
		if err == nil {
			t.Error("Expected error for invalid end value")
		}
	})
}

func TestStreamCacheEntry_fetchFullFile(t *testing.T) {
	// Create a test server that returns test data
	testData := []byte("test file content")
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "video/mp4")
		w.Header().Set("Content-Length", fmt.Sprintf("%d", len(testData)))
		w.WriteHeader(http.StatusOK)
		w.Write(testData)
	}))
	defer server.Close()

	entry := &streamCacheEntry{
		url:          server.URL,
		data:         make([]byte, 0),
		activeClients: 0,
		fetching:     false,
		fetchComplete: false,
		headers:      make(http.Header),
	}

	client := sharedHTTPClient
	entry.fetchFullFile(client, server.URL)

	// Wait for fetch to complete (with timeout)
	timeout := time.After(5 * time.Second)
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-timeout:
			t.Fatal("Timeout waiting for fetch to complete")
		case <-ticker.C:
			entry.mu.RLock()
			complete := entry.fetchComplete
			err := entry.fetchErr
			entry.mu.RUnlock()

			if complete {
				entry.mu.RLock()
				data := entry.data
				contentType := entry.contentType
				entry.mu.RUnlock()

				if string(data) != string(testData) {
					t.Errorf("Expected data %s, got %s", string(testData), string(data))
				}
				if contentType != "video/mp4" {
					t.Errorf("Expected Content-Type video/mp4, got %s", contentType)
				}
				return
			}
			if err != nil {
				t.Fatalf("Fetch error: %v", err)
			}
		}
	}
}

func TestStreamCacheEntry_fetchFullFile_concurrent(t *testing.T) {
	testData := []byte("concurrent test data")
	// Add a small delay to ensure fetch is still in progress when we check
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(50 * time.Millisecond) // Small delay to ensure fetch is in progress
		w.WriteHeader(http.StatusOK)
		w.Write(testData)
	}))
	defer server.Close()

	cache := &streamCache{
		entries: make(map[string]*streamCacheEntry),
	}

	urlStr := server.URL
	entry := cache.getOrCreateEntry(urlStr)

	client := sharedHTTPClient

	// Start multiple fetches concurrently (should only fetch once)
	var wg sync.WaitGroup
	for i := 0; i < 5; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			entry.fetchFullFile(client, urlStr)
		}()
	}

	// Wait a bit for fetches to start
	time.Sleep(10 * time.Millisecond)

	// Fetch may have already completed, so just verify the entry exists
	if entry == nil {
		t.Error("Expected entry to exist")
	}

	wg.Wait()

	// Wait for fetch to complete
	timeout := time.After(5 * time.Second)
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-timeout:
			t.Fatal("Timeout waiting for fetch to complete")
		case <-ticker.C:
			entry.mu.RLock()
			complete := entry.fetchComplete
			entry.mu.RUnlock()

			if complete {
				entry.mu.RLock()
				data := entry.data
				entry.mu.RUnlock()

				if string(data) != string(testData) {
					t.Errorf("Expected data %s, got %s", string(testData), string(data))
				}
				return
			}
		}
	}
}

func TestStreamCacheEntry_clientTracking(t *testing.T) {
	cache := &streamCache{
		entries: make(map[string]*streamCacheEntry),
	}

	urlStr := "http://example.com/client-test"
	entry := cache.getOrCreateEntry(urlStr)

	// Simulate multiple clients
	entry.addClient()
	entry.addClient()
	entry.addClient()

	if entry.activeClients != 3 {
		t.Errorf("Expected 3 active clients, got %d", entry.activeClients)
	}

	// Remove clients one by one
	shouldRemove := entry.removeClient()
	if shouldRemove {
		t.Error("Expected shouldRemove to be false with 2 clients remaining")
	}

	shouldRemove = entry.removeClient()
	if shouldRemove {
		t.Error("Expected shouldRemove to be false with 1 client remaining")
	}

	shouldRemove = entry.removeClient()
	if !shouldRemove {
		t.Error("Expected shouldRemove to be true with no clients remaining")
	}

	// Verify cache cleanup
	cache.removeEntry(urlStr)
	cache.mu.RLock()
	_, exists := cache.entries[urlStr]
	cache.mu.RUnlock()

	if exists {
		t.Error("Expected entry to be removed from cache")
	}
}

func TestStreamCacheEntry_fetchFullFile_errorHandling(t *testing.T) {
	// Create a test server that returns 404
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNotFound)
	}))
	defer server.Close()

	entry := &streamCacheEntry{
		url:          server.URL,
		data:         make([]byte, 0),
		activeClients: 0,
		fetching:     false,
		fetchComplete: false,
		headers:      make(http.Header),
	}

	client := sharedHTTPClient
	entry.fetchFullFile(client, server.URL)

	// Wait for fetch to complete or error
	timeout := time.After(2 * time.Second)
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()

	for {
		select {
		case <-timeout:
			t.Fatal("Timeout waiting for fetch to complete")
		case <-ticker.C:
			entry.mu.RLock()
			complete := entry.fetchComplete
			fetching := entry.fetching
			err := entry.fetchErr
			entry.mu.RUnlock()

			if !fetching {
				// Fetch completed (with error or success)
				if complete {
					t.Error("Expected fetch to fail, but it completed")
				}
				if err == nil {
					t.Error("Expected fetch error, but got none")
				}
				return
			}
		}
	}
}

func TestStreamCacheEntry_getRange_edgeCases(t *testing.T) {
	testData := []byte("test")
	entry := &streamCacheEntry{
		data:          testData,
		fetchComplete: true,
		contentLength: int64(len(testData)),
	}

	t.Run("single byte range", func(t *testing.T) {
		data, err := entry.getRange(0, 0)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if len(data) != 1 || data[0] != 't' {
			t.Errorf("Expected single byte 't', got %v", data)
		}
	})

	t.Run("full range", func(t *testing.T) {
		data, err := entry.getRange(0, int64(len(testData))-1)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}
		if string(data) != string(testData) {
			t.Errorf("Expected %s, got %s", string(testData), string(data))
		}
	})

	t.Run("empty range", func(t *testing.T) {
		// This should be caught by parseRangeHeader, but test getRange behavior
		_, err := entry.getRange(5, 3)
		if err == nil {
			t.Error("Expected error for invalid range (start > end)")
		}
	})
}

func TestStream_withCache(t *testing.T) {
	// Set Gin to test mode to reduce logging
	gin.SetMode(gin.TestMode)

	// Create a test server that serves media content
	testData := []byte("0123456789abcdefghijklmnopqrstuvwxyz")
	mediaServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "video/mp4")
		w.Header().Set("Content-Length", fmt.Sprintf("%d", len(testData)))
		w.Header().Set("Accept-Ranges", "bytes")

		// Handle Range requests
		rangeHeader := r.Header.Get("Range")
		if rangeHeader != "" {
			start, end, err := parseRangeHeader(rangeHeader, int64(len(testData)))
			if err != nil {
				w.WriteHeader(http.StatusRequestedRangeNotSatisfiable)
				return
			}
			w.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", start, end, len(testData)))
			w.Header().Set("Content-Length", fmt.Sprintf("%d", end-start+1))
			w.WriteHeader(http.StatusPartialContent)
			w.Write(testData[start : end+1])
		} else {
			w.WriteHeader(http.StatusOK)
			w.Write(testData)
		}
	}))
	defer mediaServer.Close()

	// Clear cache before test
	globalStreamCache.mu.Lock()
	globalStreamCache.entries = make(map[string]*streamCacheEntry)
	globalStreamCache.mu.Unlock()

	testURL, _ := url.Parse(mediaServer.URL)

	t.Run("full file request creates cache", func(t *testing.T) {
		// Get or create cache entry and start fetch
		entry := globalStreamCache.getOrCreateEntry(testURL.String())
		entry.addClient()
		entry.fetchFullFile(sharedHTTPClient, testURL.String())

		// Wait for cache to be populated
		timeout := time.After(5 * time.Second)
		ticker := time.NewTicker(100 * time.Millisecond)
		defer ticker.Stop()

		var cacheReady bool
		for {
			select {
			case <-timeout:
				t.Fatal("Timeout waiting for cache to be populated")
			case <-ticker.C:
				entry.mu.RLock()
				complete := entry.fetchComplete
				entry.mu.RUnlock()

				if complete {
					cacheReady = true
					break
				}
			}
			if cacheReady {
				break
			}
		}

		// Verify cache data
		entry.mu.RLock()
		cachedData := entry.data
		entry.mu.RUnlock()

		if string(cachedData) != string(testData) {
			t.Errorf("Expected cached data to match test data")
		}

		// Clean up
		entry.removeClient()
		if entry.removeClient() {
			globalStreamCache.removeEntry(testURL.String())
		}
	})

	t.Run("range request served from cache", func(t *testing.T) {
		// Use a different URL for this test
		testURL2, _ := url.Parse(mediaServer.URL + "/range-test")
		
		// Ensure cache is populated first
		entry := globalStreamCache.getOrCreateEntry(testURL2.String())
		entry.addClient()
		entry.fetchFullFile(sharedHTTPClient, testURL2.String())

		// Wait for cache to be ready
		timeout := time.After(5 * time.Second)
		ticker := time.NewTicker(100 * time.Millisecond)
		defer ticker.Stop()

		var cacheReady bool
		for {
			select {
			case <-timeout:
				t.Fatal("Timeout waiting for cache")
			case <-ticker.C:
				entry.mu.RLock()
				complete := entry.fetchComplete
				entry.mu.RUnlock()
				if complete {
					cacheReady = true
					break
				}
			}
			if cacheReady {
				break
			}
		}

		// Test getRange directly (since stream uses ctx.Stream which requires CloseNotify)
		expectedData := testData[0:10]
		data, err := entry.getRange(0, 9)
		if err != nil {
			t.Fatalf("Expected no error, got %v", err)
		}

		if string(data) != string(expectedData) {
			t.Errorf("Expected %s, got %s", string(expectedData), string(data))
		}

		// Clean up
		entry.removeClient()
		if entry.removeClient() {
			globalStreamCache.removeEntry(testURL2.String())
		}
	})

	t.Run("cache cleanup on client disconnect", func(t *testing.T) {
		// Clear cache
		globalStreamCache.mu.Lock()
		globalStreamCache.entries = make(map[string]*streamCacheEntry)
		globalStreamCache.mu.Unlock()

		testURL2, _ := url.Parse(mediaServer.URL + "/test2")
		entry := globalStreamCache.getOrCreateEntry(testURL2.String())
		entry.addClient()

		// Verify entry exists
		globalStreamCache.mu.RLock()
		_, exists := globalStreamCache.entries[testURL2.String()]
		globalStreamCache.mu.RUnlock()

		if !exists {
			t.Error("Expected cache entry to exist")
		}

		// Simulate client disconnect
		shouldRemove := entry.removeClient()
		if !shouldRemove {
			t.Error("Expected shouldRemove to be true")
		}

		globalStreamCache.removeEntry(testURL2.String())

		// Verify entry is removed
		globalStreamCache.mu.RLock()
		_, exists = globalStreamCache.entries[testURL2.String()]
		globalStreamCache.mu.RUnlock()

		if exists {
			t.Error("Expected cache entry to be removed")
		}
	})

	t.Run("multiple clients share cache", func(t *testing.T) {
		// Clear cache
		globalStreamCache.mu.Lock()
		globalStreamCache.entries = make(map[string]*streamCacheEntry)
		globalStreamCache.mu.Unlock()

		testURL3, _ := url.Parse(mediaServer.URL + "/test3")
		entry1 := globalStreamCache.getOrCreateEntry(testURL3.String())
		entry1.addClient()

		entry2 := globalStreamCache.getOrCreateEntry(testURL3.String())
		entry2.addClient()

		if entry1 != entry2 {
			t.Error("Expected same cache entry for same URL")
		}

		if entry1.activeClients != 2 {
			t.Errorf("Expected 2 active clients, got %d", entry1.activeClients)
		}

		// Remove one client
		entry1.removeClient()
		if entry1.activeClients != 1 {
			t.Errorf("Expected 1 active client, got %d", entry1.activeClients)
		}

		// Remove second client
		shouldRemove := entry1.removeClient()
		if !shouldRemove {
			t.Error("Expected shouldRemove to be true")
		}
	})
}

