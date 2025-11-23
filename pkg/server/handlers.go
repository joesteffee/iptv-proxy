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
	"net/http"
	"net/url"
	"path"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

// rateLimitTracker tracks URLs that are currently rate-limited
// to avoid making duplicate requests to the same URL
type rateLimitTracker struct {
	mu          sync.RWMutex
	rateLimited map[string]time.Time // URL -> time when rate limit expires
}

var globalRateLimitTracker = &rateLimitTracker{
	rateLimited: make(map[string]time.Time),
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
	client := &http.Client{}

	const rateLimitStatusCode = 458
	const initialBackoff = 2 * time.Second
	const maxBackoff = 60 * time.Second
	const rateLimitCooldown = 30 * time.Second // How long to mark URL as rate-limited
	const retryTimeout = 1 * time.Minute        // Keep retrying for 1 minute after first 458

	urlStr := oriURL.String()

	// Check if this URL is currently rate-limited and wait if necessary
	globalRateLimitTracker.waitIfRateLimited(urlStr, maxBackoff)

	var resp *http.Response
	var first458Time *time.Time
	attempt := 0

	for {
		attempt++
		req, err := http.NewRequest("GET", urlStr, nil)
		if err != nil {
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}

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
			if time.Since(*first458Time) >= retryTimeout {
				log.Printf("[iptv-proxy] %v | Rate limit (458) persisted for %v, returning error after %d attempts\n",
					time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout, attempt)
				ctx.Status(rateLimitStatusCode)
				return
			}

			// Calculate exponential backoff: 2s, 4s, 8s, etc., capped at maxBackoff
			backoffDuration := initialBackoff * time.Duration(1<<uint(attempt-1))
			if backoffDuration > maxBackoff {
				backoffDuration = maxBackoff
			}

			// Ensure we don't exceed the retry timeout
			elapsed := time.Since(*first458Time)
			remainingTime := retryTimeout - elapsed
			if backoffDuration > remainingTime {
				backoffDuration = remainingTime
			}

			// Mark this URL as rate-limited to prevent other concurrent requests
			globalRateLimitTracker.markRateLimited(urlStr, rateLimitCooldown)

			log.Printf("[iptv-proxy] %v | Rate limit (458) detected on attempt %d, backing off for %v (retrying for %v more)\n",
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
	// Use 128KB buffer (4x the default 32KB) to reduce number of requests
	buf := make([]byte, 128*1024)
	ctx.Stream(func(w io.Writer) bool {
		io.CopyBuffer(w, resp.Body, buf) // nolint: errcheck
		return false
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
