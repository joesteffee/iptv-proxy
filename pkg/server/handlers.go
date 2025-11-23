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
	"bytes"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"net"
	"net/http"
	"net/url"
	"path"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

// sharedHTTPClient is a shared HTTP client with optimized settings for connection reuse
var sharedHTTPClient = &http.Client{
	Transport: &http.Transport{
		DialContext: (&net.Dialer{
			Timeout:   5 * time.Second,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		MaxIdleConns:          100,
		MaxIdleConnsPerHost:   10,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   5 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
		DisableKeepAlives:     false,
		ForceAttemptHTTP2:     true,
	},
	Timeout: 0, // No overall timeout - streaming can take a long time
}

// sharedHTTPClientHLS is similar but configured for HLS redirects
var sharedHTTPClientHLS = &http.Client{
	Transport: &http.Transport{
		DialContext: (&net.Dialer{
			Timeout:   5 * time.Second,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		MaxIdleConns:          100,
		MaxIdleConnsPerHost:   10,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   5 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
		DisableKeepAlives:     false,
		ForceAttemptHTTP2:     true,
	},
	Timeout: 0,
	CheckRedirect: func(req *http.Request, via []*http.Request) error {
		return http.ErrUseLastResponse
	},
}

// rateLimitTracker tracks URLs that are currently rate-limited
// to avoid making duplicate requests to the same URL
type rateLimitTracker struct {
	mu          sync.RWMutex
	rateLimited map[string]time.Time // URL -> time when rate limit expires
}

var globalRateLimitTracker = &rateLimitTracker{
	rateLimited: make(map[string]time.Time),
}

// streamCacheEntry represents a cached media file
type streamCacheEntry struct {
	mu           sync.RWMutex
	url          string
	data         []byte
	contentType  string
	contentLength int64
	activeClients int
	fetching     bool
	fetchComplete bool
	fetchErr     error
	headers      http.Header
}

// streamCache manages cached media streams
type streamCache struct {
	mu     sync.RWMutex
	entries map[string]*streamCacheEntry
}

var globalStreamCache = &streamCache{
	entries: make(map[string]*streamCacheEntry),
}

// getOrCreateEntry gets an existing cache entry or creates a new one
func (sc *streamCache) getOrCreateEntry(urlStr string) *streamCacheEntry {
	sc.mu.Lock()
	defer sc.mu.Unlock()

	entry, exists := sc.entries[urlStr]
	if !exists {
		entry = &streamCacheEntry{
			url:          urlStr,
			data:         make([]byte, 0),
			activeClients: 0,
			fetching:     false,
			fetchComplete: false,
			headers:      make(http.Header),
		}
		sc.entries[urlStr] = entry
	}
	return entry
}

// removeEntry removes a cache entry when no clients are using it
func (sc *streamCache) removeEntry(urlStr string) {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	delete(sc.entries, urlStr)
}

// addClient increments the client count for an entry
func (e *streamCacheEntry) addClient() {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.activeClients++
}

// removeClient decrements the client count and returns true if no clients remain
func (e *streamCacheEntry) removeClient() bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.activeClients--
	return e.activeClients == 0
}

// fetchFullFile fetches the entire file from the remote server in the background
func (e *streamCacheEntry) fetchFullFile(client *http.Client, urlStr string) {
	e.mu.Lock()
	if e.fetching || e.fetchComplete {
		e.mu.Unlock()
		return
	}
	e.fetching = true
	e.mu.Unlock()

	go func() {
		const rateLimitStatusCode = 458
		const retryInterval = 5 * time.Second
		retryTimeout := 10 * time.Minute // Default retry timeout for background fetch

		var resp *http.Response
		var first458Time *time.Time
		attempt := 0

		for {
			attempt++
			
			// Check if this URL is currently rate-limited
			globalRateLimitTracker.waitIfRateLimited(urlStr, retryInterval)
			
			req, err := http.NewRequest("GET", urlStr, nil)
			if err != nil {
				e.mu.Lock()
				e.fetchErr = err
				e.fetching = false
				e.mu.Unlock()
				return
			}

			// Don't forward Range header for full file fetch
			req.Header.Set("User-Agent", "iptv-proxy-cache")

			resp, err = client.Do(req)
			if err != nil {
				e.mu.Lock()
				e.fetchErr = err
				e.fetching = false
				e.mu.Unlock()
				return
			}

			// Check for rate limiting (458) response
			if resp.StatusCode == rateLimitStatusCode {
				resp.Body.Close()

				if first458Time == nil {
					now := time.Now()
					first458Time = &now
				}

				elapsed := time.Since(*first458Time)
				if elapsed >= retryTimeout {
					e.mu.Lock()
					e.fetchErr = fmt.Errorf("rate limit persisted for %v", retryTimeout)
					e.fetching = false
					e.mu.Unlock()
					return
				}

				remainingTime := retryTimeout - elapsed
				backoffDuration := retryInterval
				if backoffDuration > remainingTime {
					backoffDuration = remainingTime
				}

				globalRateLimitTracker.markRateLimited(urlStr, 30*time.Second)
				time.Sleep(backoffDuration)
				continue
			}

			// Check for successful status code
			if resp.StatusCode < 200 || resp.StatusCode >= 300 {
				resp.Body.Close()
				e.mu.Lock()
				e.fetchErr = fmt.Errorf("HTTP %d: %s", resp.StatusCode, resp.Status)
				e.fetching = false
				e.mu.Unlock()
				return
			}

			// Success - read the full response
			e.mu.Lock()
			e.headers = make(http.Header)
			mergeResponseHeader(e.headers, resp.Header)
			e.contentType = resp.Header.Get("Content-Type")
			e.contentLength = resp.ContentLength
			e.mu.Unlock()

			// Read the entire body
			data, err := ioutil.ReadAll(resp.Body)
			resp.Body.Close()

			e.mu.Lock()
			if err != nil {
				e.fetchErr = err
				e.fetching = false
			} else {
				e.data = data
				if e.contentLength == -1 {
					e.contentLength = int64(len(data))
				}
				e.fetchComplete = true
				e.fetching = false
				log.Printf("[iptv-proxy] %v | Cached full file: %s (%d bytes)\n",
					time.Now().Format("2006/01/02 - 15:04:05"), urlStr, len(data))
			}
			e.mu.Unlock()
			return
		}
	}()
}

// getRange returns the requested byte range from cache
func (e *streamCacheEntry) getRange(start, end int64) ([]byte, error) {
	e.mu.RLock()
	defer e.mu.RUnlock()

	if e.fetchErr != nil {
		return nil, e.fetchErr
	}

	if !e.fetchComplete {
		// Cache not ready yet
		return nil, fmt.Errorf("cache not ready")
	}

	if start < 0 {
		start = 0
	}
	if end >= int64(len(e.data)) {
		end = int64(len(e.data)) - 1
	}
	if start > end {
		return nil, fmt.Errorf("invalid range")
	}

	return e.data[start : end+1], nil
}

// parseRangeHeader parses the Range header and returns start and end byte positions
// Returns -1, -1 if no valid range is found
func parseRangeHeader(rangeHeader string, contentLength int64) (start, end int64, err error) {
	if rangeHeader == "" {
		return -1, -1, nil
	}

	// Range header format: "bytes=start-end" or "bytes=start-"
	if !strings.HasPrefix(rangeHeader, "bytes=") {
		return -1, -1, fmt.Errorf("invalid range header format")
	}

	rangeSpec := strings.TrimPrefix(rangeHeader, "bytes=")
	parts := strings.Split(rangeSpec, "-")
	if len(parts) != 2 {
		return -1, -1, fmt.Errorf("invalid range spec")
	}

	var startVal, endVal int64

	if parts[0] != "" {
		_, err := fmt.Sscanf(parts[0], "%d", &startVal)
		if err != nil {
			return -1, -1, fmt.Errorf("invalid start value: %v", err)
		}
	} else {
		// Suffix range: "bytes=-suffix" means last suffix bytes
		if parts[1] != "" {
			var suffix int64
			_, err := fmt.Sscanf(parts[1], "%d", &suffix)
			if err != nil {
				return -1, -1, fmt.Errorf("invalid suffix value: %v", err)
			}
			startVal = contentLength - suffix
			if startVal < 0 {
				startVal = 0
			}
			endVal = contentLength - 1
			return startVal, endVal, nil
		}
		return -1, -1, fmt.Errorf("invalid range spec")
	}

	if parts[1] != "" {
		_, err := fmt.Sscanf(parts[1], "%d", &endVal)
		if err != nil {
			return -1, -1, fmt.Errorf("invalid end value: %v", err)
		}
	} else {
		// Open-ended range: "bytes=start-" means from start to end
		endVal = contentLength - 1
	}

	if startVal < 0 {
		startVal = 0
	}
	if endVal >= contentLength {
		endVal = contentLength - 1
	}
	if startVal > endVal {
		return -1, -1, fmt.Errorf("invalid range: start > end")
	}

	return startVal, endVal, nil
}

// waitIfRateLimited checks if a URL is currently rate-limited and waits if necessary
// Returns true if we should skip the request (rate limit active), false otherwise
func (rlt *rateLimitTracker) waitIfRateLimited(url string, maxWait time.Duration) bool {
	rlt.mu.RLock()
	expiresAt, isRateLimited := rlt.rateLimited[url]
	rlt.mu.RUnlock()

	if !isRateLimited {
		return false
	}

	now := time.Now()
	if expiresAt.After(now) {
		waitDuration := expiresAt.Sub(now)
		if waitDuration > maxWait {
			waitDuration = maxWait
		}
		log.Printf("[iptv-proxy] %v | URL is rate-limited, waiting %v before retry\n",
			time.Now().Format("2006/01/02 - 15:04:05"), waitDuration)
		time.Sleep(waitDuration)
		return true
	}

	// Rate limit expired, remove it
	rlt.mu.Lock()
	delete(rlt.rateLimited, url)
	rlt.mu.Unlock()
	return false
}

// markRateLimited marks a URL as rate-limited for a given duration
func (rlt *rateLimitTracker) markRateLimited(url string, duration time.Duration) {
	rlt.mu.Lock()
	defer rlt.mu.Unlock()
	rlt.rateLimited[url] = time.Now().Add(duration)
}

// clearRateLimit removes rate limit tracking for a URL (on successful request)
func (rlt *rateLimitTracker) clearRateLimit(url string) {
	rlt.mu.Lock()
	defer rlt.mu.Unlock()
	delete(rlt.rateLimited, url)
}

func (c *Config) getM3U(ctx *gin.Context) {
	ctx.Header("Content-Disposition", fmt.Sprintf(`attachment; filename=%q`, c.M3UFileName))
	ctx.Header("Content-Type", "application/octet-stream")

	ctx.File(c.proxyfiedM3UPath)
}

func (c *Config) reverseProxy(ctx *gin.Context) {
	rpURL, err := url.Parse(c.track.URI)
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.stream(ctx, rpURL)
}

func (c *Config) m3u8ReverseProxy(ctx *gin.Context) {
	id := ctx.Param("id")

	rpURL, err := url.Parse(strings.ReplaceAll(c.track.URI, path.Base(c.track.URI), id))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.stream(ctx, rpURL)
}

func (c *Config) stream(ctx *gin.Context, oriURL *url.URL) {
	client := sharedHTTPClient
	urlStr := oriURL.String()

	// Get or create cache entry for this URL
	cacheEntry := globalStreamCache.getOrCreateEntry(urlStr)
	cacheEntry.addClient()
	
	// Ensure background fetch is started
	cacheEntry.fetchFullFile(client, urlStr)

	// Clean up when client disconnects
	defer func() {
		if cacheEntry.removeClient() {
			// No more clients, remove from cache
			globalStreamCache.removeEntry(urlStr)
			log.Printf("[iptv-proxy] %v | Removed cache entry (no active clients): %s\n",
				time.Now().Format("2006/01/02 - 15:04:05"), urlStr)
		}
	}()

	// Check if we have a Range request
	rangeHeader := ctx.Request.Header.Get("Range")
	
	// Try to serve from cache if available
	cacheEntry.mu.RLock()
	cacheReady := cacheEntry.fetchComplete
	cacheErr := cacheEntry.fetchErr
	contentLength := cacheEntry.contentLength
	cacheEntry.mu.RUnlock()

	if cacheReady && cacheErr == nil {
		// Cache is ready, serve from cache
		if rangeHeader != "" {
			// Parse range request
			start, end, err := parseRangeHeader(rangeHeader, contentLength)
			if err != nil {
				ctx.AbortWithStatus(http.StatusRequestedRangeNotSatisfiable)
				return
			}

			// Get range from cache
			data, err := cacheEntry.getRange(start, end)
			if err != nil {
				// Range not available in cache, fall through to direct fetch
				log.Printf("[iptv-proxy] %v | Range not in cache, fetching directly: %s\n",
					time.Now().Format("2006/01/02 - 15:04:05"), urlStr)
			} else {
				// Serve from cache
				cacheEntry.mu.RLock()
				mergeResponseHeader(ctx.Writer.Header(), cacheEntry.headers)
				cacheEntry.mu.RUnlock()
				
				ctx.Writer.Header().Set("Content-Range", fmt.Sprintf("bytes %d-%d/%d", start, end, contentLength))
				ctx.Writer.Header().Set("Content-Length", fmt.Sprintf("%d", end-start+1))
				ctx.Status(http.StatusPartialContent)
				ctx.Data(http.StatusPartialContent, cacheEntry.contentType, data)
				return
			}
		} else {
			// Full file request, serve from cache
			cacheEntry.mu.RLock()
			mergeResponseHeader(ctx.Writer.Header(), cacheEntry.headers)
			data := cacheEntry.data
			contentType := cacheEntry.contentType
			cacheEntry.mu.RUnlock()
			
			ctx.Status(http.StatusOK)
			if contentType != "" {
				ctx.Writer.Header().Set("Content-Type", contentType)
			}
			ctx.Writer.Header().Set("Content-Length", fmt.Sprintf("%d", contentLength))
			ctx.Writer.Header().Set("Accept-Ranges", "bytes")
			
			ctx.Data(http.StatusOK, contentType, data)
			return
		}
	}

	// Cache not ready or error - fall back to direct streaming
	// This ensures clients can still stream even while cache is being populated
	c.streamDirect(ctx, oriURL, rangeHeader)
}

// streamDirect handles direct streaming when cache is not available
func (c *Config) streamDirect(ctx *gin.Context, oriURL *url.URL, rangeHeader string) {
	client := sharedHTTPClient

	const rateLimitStatusCode = 458
	const retryInterval = 5 * time.Second // Retry every 5 seconds after 458 response
	const rateLimitCooldown = 30 * time.Second // How long to mark URL as rate-limited
	retryTimeout := time.Duration(c.RateLimitRetryTimeout) * time.Minute // Keep retrying for configured minutes after first 458

	urlStr := oriURL.String()

	var resp *http.Response
	var first458Time *time.Time
	attempt := 0

	for {
		attempt++
		
		// Check if this URL is currently rate-limited and wait if necessary (transparent to client)
		// This prevents duplicate requests but doesn't block the client connection
		globalRateLimitTracker.waitIfRateLimited(urlStr, retryInterval)
		
		req, err := http.NewRequest("GET", urlStr, nil)
		if err != nil {
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}

		// Forward headers including Range if present
		mergeHttpHeader(req.Header, ctx.Request.Header)

		resp, err = client.Do(req)
		if err != nil {
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}

		// Check for rate limiting (458) response
		if resp.StatusCode == rateLimitStatusCode {
			resp.Body.Close()

			// Track when we first encountered 458
			if first458Time == nil {
				now := time.Now()
				first458Time = &now
				log.Printf("[iptv-proxy] %v | Rate limit (458) detected, will retry for up to %v\n",
					time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout)
			}

			// Check if we've exceeded the retry timeout
			elapsed := time.Since(*first458Time)
			if elapsed >= retryTimeout {
				log.Printf("[iptv-proxy] %v | Rate limit (458) persisted for %v, returning error after %d attempts\n",
					time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout, attempt)
				ctx.Status(rateLimitStatusCode)
				return
			}

			// Calculate remaining time and adjust retry interval if needed
			remainingTime := retryTimeout - elapsed
			backoffDuration := retryInterval
			if backoffDuration > remainingTime {
				backoffDuration = remainingTime
			}

			// Mark this URL as rate-limited to prevent other concurrent requests
			globalRateLimitTracker.markRateLimited(urlStr, rateLimitCooldown)

			log.Printf("[iptv-proxy] %v | Rate limit (458) detected on attempt %d, retrying in %v (retrying for %v more)\n",
				time.Now().Format("2006/01/02 - 15:04:05"), attempt, backoffDuration, remainingTime-backoffDuration)
			time.Sleep(backoffDuration)
			continue
		}

		// Success or non-458 error - clear rate limit tracking and break out of retry loop
		globalRateLimitTracker.clearRateLimit(urlStr)
		break
	}
	defer resp.Body.Close()

	mergeHttpHeader(ctx.Writer.Header(), resp.Header)
	ctx.Status(resp.StatusCode)
	
	// For Range requests, ensure Accept-Ranges is set if not already present
	if rangeHeader != "" && ctx.Writer.Header().Get("Accept-Ranges") == "" {
		ctx.Writer.Header().Set("Accept-Ranges", "bytes")
	}
	
	// Use 256KB buffer for better streaming performance and fewer syscalls
	// This reduces the number of read/write operations
	buf := make([]byte, 256*1024)
	
	// Stream the response body efficiently
	ctx.Stream(func(w io.Writer) bool {
		_, err := io.CopyBuffer(w, resp.Body, buf)
		// Return false to stop streaming on error or EOF
		return err == nil
	})
}

func (c *Config) xtreamStream(ctx *gin.Context, oriURL *url.URL) {
	id := ctx.Param("id")
	if strings.HasSuffix(id, ".m3u8") {
		c.hlsXtreamStream(ctx, oriURL)
		return
	}

	c.stream(ctx, oriURL)
}

type values []string

func (vs values) contains(s string) bool {
	for _, v := range vs {
		if v == s {
			return true
		}
	}

	return false
}

func mergeHttpHeader(dst, src http.Header) {
	for k, vv := range src {
		for _, v := range vv {
			if values(dst.Values(k)).contains(v) {
				continue
			}
			dst.Add(k, v)
		}
	}
}

// authRequest handle auth credentials
type authRequest struct {
	Username string `form:"username" binding:"required"`
	Password string `form:"password" binding:"required"`
}

func (c *Config) authenticate(ctx *gin.Context) {
	var authReq authRequest
	if err := ctx.Bind(&authReq); err != nil {
		ctx.AbortWithError(http.StatusBadRequest, err) // nolint: errcheck
		return
	}
	if c.ProxyConfig.User.String() != authReq.Username || c.ProxyConfig.Password.String() != authReq.Password {
		ctx.AbortWithStatus(http.StatusUnauthorized)
	}
}

func (c *Config) appAuthenticate(ctx *gin.Context) {
	contents, err := ioutil.ReadAll(ctx.Request.Body)
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	q, err := url.ParseQuery(string(contents))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}
	if len(q["username"]) == 0 || len(q["password"]) == 0 {
		ctx.AbortWithError(http.StatusBadRequest, fmt.Errorf("bad body url query parameters")) // nolint: errcheck
		return
	}
	log.Printf("[iptv-proxy] %v | %s |App Auth\n", time.Now().Format("2006/01/02 - 15:04:05"), ctx.ClientIP())
	if c.ProxyConfig.User.String() != q["username"][0] || c.ProxyConfig.Password.String() != q["password"][0] {
		ctx.AbortWithStatus(http.StatusUnauthorized)
	}

	ctx.Request.Body = ioutil.NopCloser(bytes.NewReader(contents))
}
