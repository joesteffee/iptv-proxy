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
	"encoding/json"
	"fmt"
	"log"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"strings"
	"sync"

	"github.com/gin-contrib/cors"
	"github.com/jamesnetherton/m3u"
	"github.com/pierre-emmanuelJ/iptv-proxy/pkg/config"
	uuid "github.com/satori/go.uuid"

	"github.com/gin-gonic/gin"
)

var defaultProxyfiedM3UPath = filepath.Join(os.TempDir(), uuid.NewV4().String()+".iptv-proxy.m3u")
var endpointAntiColision = strings.Split(uuid.NewV4().String(), "-")[0]

// categoryFilter manages enabled categories
type categoryFilter struct {
	mu           sync.RWMutex
	enabledCats  map[string]map[string]bool // type -> categoryID -> enabled
}

var globalCategoryFilter = &categoryFilter{
	enabledCats: make(map[string]map[string]bool),
}

// isCategoryEnabled checks if a category is enabled
func (cf *categoryFilter) isCategoryEnabled(catType, categoryID string) bool {
	cf.mu.RLock()
	defer cf.mu.RUnlock()
	
	if typeMap, ok := cf.enabledCats[catType]; ok {
		return typeMap[categoryID]
	}
	return false // By default, categories are disabled (not enabled)
}

// setCategoryEnabled sets the enabled state of a category
func (cf *categoryFilter) setCategoryEnabled(catType, categoryID string, enabled bool) {
	cf.mu.Lock()
	defer cf.mu.Unlock()
	
	if cf.enabledCats[catType] == nil {
		cf.enabledCats[catType] = make(map[string]bool)
	}
	cf.enabledCats[catType][categoryID] = enabled
}

// setEnabledCategories sets multiple enabled categories at once
func (cf *categoryFilter) setEnabledCategories(enabled map[string]map[string]bool) {
	cf.mu.Lock()
	defer cf.mu.Unlock()
	cf.enabledCats = enabled
}

// getEnabledCategories returns a copy of enabled categories
func (cf *categoryFilter) getEnabledCategories() map[string]map[string]bool {
	cf.mu.RLock()
	defer cf.mu.RUnlock()
	
	result := make(map[string]map[string]bool)
	for catType, typeMap := range cf.enabledCats {
		result[catType] = make(map[string]bool)
		for catID, enabled := range typeMap {
			result[catType][catID] = enabled
		}
	}
	return result
}

// loadFromFile loads enabled categories from a JSON file
func (cf *categoryFilter) loadFromFile(filePath string) error {
	if filePath == "" {
		return nil // No file path configured, skip loading
	}
	
	cf.mu.Lock()
	defer cf.mu.Unlock()
	
	data, err := os.ReadFile(filePath)
	if err != nil {
		if os.IsNotExist(err) {
			// File doesn't exist yet, that's okay - start with empty filters (all disabled)
			log.Printf("[iptv-proxy] Category filters file not found at %s, starting with all categories disabled\n", filePath)
			return nil
		}
		return fmt.Errorf("failed to read category filters file: %w", err)
	}
	
	var enabled map[string]map[string]bool
	if err := json.Unmarshal(data, &enabled); err != nil {
		return fmt.Errorf("failed to parse category filters file: %w", err)
	}
	
	cf.enabledCats = enabled
	if len(enabled) > 0 {
		total := 0
		for _, typeMap := range enabled {
			total += len(typeMap)
		}
		log.Printf("[iptv-proxy] Loaded %d enabled categories from %s\n", total, filePath)
	}
	return nil
}

// saveToFile saves enabled categories to a JSON file
func (cf *categoryFilter) saveToFile(filePath string) error {
	if filePath == "" {
		return nil // No file path configured, skip saving
	}
	
	cf.mu.RLock()
	enabled := cf.getEnabledCategories()
	cf.mu.RUnlock()
	
	data, err := json.MarshalIndent(enabled, "", "  ")
	if err != nil {
		return fmt.Errorf("failed to marshal category filters: %w", err)
	}
	
	// Create directory if it doesn't exist
	dir := filepath.Dir(filePath)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("failed to create directory for category filters: %w", err)
	}
	
	if err := os.WriteFile(filePath, data, 0644); err != nil {
		return fmt.Errorf("failed to write category filters file: %w", err)
	}
	
	total := 0
	for _, typeMap := range enabled {
		total += len(typeMap)
	}
	log.Printf("[iptv-proxy] Saved %d enabled categories to %s\n", total, filePath)
	return nil
}

// Config represent the server configuration
type Config struct {
	*config.ProxyConfig

	// M3U service part
	playlist *m3u.Playlist
	// this variable is set only for m3u proxy endpoints
	track *m3u.Track
	// path to the proxyfied m3u file
	proxyfiedM3UPath string

	endpointAntiColision string

	// Redis cache for Xtream API responses
	redisCache *RedisCache
}

// NewServer initialize a new server configuration
func NewServer(config *config.ProxyConfig) (*Config, error) {
	var p m3u.Playlist
	if config.RemoteURL.String() != "" {
		var err error
		p, err = m3u.Parse(config.RemoteURL.String())
		if err != nil {
			return nil, err
		}
	}

	if trimmedCustomId := strings.Trim(config.CustomId, "/"); trimmedCustomId != "" {
		endpointAntiColision = trimmedCustomId
	}

	// Initialize Redis cache if configured
	var redisCache *RedisCache
	if config.RedisURL != "" {
		var err error
		redisCache, err = NewRedisCache(config.RedisURL)
		if err != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to initialize Redis cache: %v. Continuing without cache.", err)
		}
	}

	return &Config{
		config,
		&p,
		nil,
		defaultProxyfiedM3UPath,
		endpointAntiColision,
		redisCache,
	}, nil
}

// Serve the iptv-proxy api
func (c *Config) Serve() error {
	// Load category filters from file on startup
	if c.CategoryFiltersPath != "" {
		if err := globalCategoryFilter.loadFromFile(c.CategoryFiltersPath); err != nil {
			log.Printf("[iptv-proxy] WARNING: Failed to load category filters: %v\n", err)
		}
	}
	
	if err := c.playlistInitialization(); err != nil {
		return err
	}

	router := gin.Default()
	router.Use(cors.Default())
	group := router.Group("/")
	c.routes(group)

	// Add a message to indicate the server is ready
	log.Printf("[iptv-proxy] Server is ready and listening on :%d", c.HostConfig.Port)

	return router.Run(fmt.Sprintf(":%d", c.HostConfig.Port))
}

func (c *Config) playlistInitialization() error {
	if len(c.playlist.Tracks) == 0 {
		return nil
	}

	f, err := os.Create(c.proxyfiedM3UPath)
	if err != nil {
		return err
	}
	defer f.Close()

	return c.marshallInto(f, false)
}

// MarshallInto a *bufio.Writer a Playlist.
func (c *Config) marshallInto(into *os.File, xtream bool) error {
	filteredTrack := make([]m3u.Track, 0, len(c.playlist.Tracks))

	ret := 0
	into.WriteString("#EXTM3U\n") // nolint: errcheck
	for i, track := range c.playlist.Tracks {
		var buffer bytes.Buffer

		buffer.WriteString("#EXTINF:")                       // nolint: errcheck
		buffer.WriteString(fmt.Sprintf("%d ", track.Length)) // nolint: errcheck
		for i := range track.Tags {
			if i == len(track.Tags)-1 {
				buffer.WriteString(fmt.Sprintf("%s=%q", track.Tags[i].Name, track.Tags[i].Value)) // nolint: errcheck
				continue
			}
			buffer.WriteString(fmt.Sprintf("%s=%q ", track.Tags[i].Name, track.Tags[i].Value)) // nolint: errcheck
		}

		uri, err := c.replaceURL(track.URI, i-ret, xtream)
		if err != nil {
			ret++
			log.Printf("ERROR: track: %s: %s", track.Name, err)
			continue
		}

		into.WriteString(fmt.Sprintf("%s, %s\n%s\n", buffer.String(), track.Name, uri)) // nolint: errcheck

		filteredTrack = append(filteredTrack, track)
	}
	c.playlist.Tracks = filteredTrack

	return into.Sync()
}

// ReplaceURL replace original playlist url by proxy url
func (c *Config) replaceURL(uri string, trackIndex int, xtream bool) (string, error) {
	oriURL, err := url.Parse(uri)
	if err != nil {
		return "", err
	}

	protocol := "http"
	if c.HTTPS {
		protocol = "https"
	}

	customEnd := strings.Trim(c.CustomEndpoint, "/")
	if customEnd != "" {
		customEnd = fmt.Sprintf("/%s", customEnd)
	}

	uriPath := oriURL.EscapedPath()
	if xtream {
		uriPath = strings.ReplaceAll(uriPath, c.XtreamUser.PathEscape(), c.User.PathEscape())
		uriPath = strings.ReplaceAll(uriPath, c.XtreamPassword.PathEscape(), c.Password.PathEscape())
	} else {
		uriPath = path.Join("/", c.endpointAntiColision, c.User.PathEscape(), c.Password.PathEscape(), fmt.Sprintf("%d", trackIndex), path.Base(uriPath))
	}

	basicAuth := oriURL.User.String()
	if basicAuth != "" {
		basicAuth += "@"
	}

	newURI := fmt.Sprintf(
		"%s://%s%s:%d%s%s",
		protocol,
		basicAuth,
		c.HostConfig.Hostname,
		c.AdvertisedPort,
		customEnd,
		uriPath,
	)

	newURL, err := url.Parse(newURI)
	if err != nil {
		return "", err
	}

	return newURL.String(), nil
}
