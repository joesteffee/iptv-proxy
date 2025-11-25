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
	"bufio"
	"errors"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/jamesnetherton/m3u"
	xtreamapi "github.com/pierre-emmanuelJ/iptv-proxy/pkg/xtream-proxy"
	xtreamcodes "github.com/tellytv/go.xtream-codes"
	uuid "github.com/satori/go.uuid"
)

type cacheMeta struct {
	string
	time.Time
}

var hlsChannelsRedirectURL map[string]url.URL = map[string]url.URL{}
var hlsChannelsRedirectURLLock = sync.RWMutex{}

// XXX Use key/value storage e.g: etcd, redis...
// and remove that dirty globals
var xtreamM3uCache map[string]cacheMeta = map[string]cacheMeta{}
var xtreamM3uCacheLock = sync.RWMutex{}

// min returns the minimum of two integers
func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// getFieldValue extracts a field value from either a struct or a map[string]interface{}
// This is needed because cached JSON data is unmarshaled as map[string]interface{}
func getFieldValue(v reflect.Value, fieldName string) string {
	if !v.IsValid() {
		return ""
	}
	
	// Handle map[string]interface{} (from cached JSON)
	if v.Kind() == reflect.Interface {
		v = v.Elem()
	}
	
	if v.Kind() == reflect.Map {
		// Map of Go struct field names to JSON field names
		jsonFieldMap := map[string][]string{
			"ID":            {"category_id", "id", "ID", "vod_id", "series_id", "stream_id", "epg_channel_id"},
			"Name":          {"category_name", "name", "Name", "title", "Title"},
			"EPGChannelID":  {"epg_channel_id", "epgchannelid", "EPGChannelID"},
			"Icon":          {"stream_icon", "icon", "Icon", "cover", "Cover"},
			"ContainerExtension": {"container_extension", "containerextension", "ContainerExtension"},
			"SeriesID":      {"series_id", "seriesid", "SeriesID"},
			"Cover":         {"cover", "Cover", "cover_image"},
		}
		
		// Get possible JSON field names for this Go field name
		possibleNames := []string{fieldName} // Try exact match first
		if names, ok := jsonFieldMap[fieldName]; ok {
			possibleNames = append(possibleNames, names...)
		} else {
			// Fallback: try common variations
			possibleNames = append(possibleNames,
				strings.ToLower(fieldName),
				strings.ToUpper(fieldName),
			)
			if len(fieldName) > 0 {
				firstLower := strings.ToLower(string(fieldName[0])) + fieldName[1:]
				possibleNames = append(possibleNames, firstLower)
			}
		}
		
		// Try each possible field name
		for _, jsonName := range possibleNames {
			key := reflect.ValueOf(jsonName)
			fieldVal := v.MapIndex(key)
			if fieldVal.IsValid() && !fieldVal.IsZero() {
				val := fmt.Sprint(fieldVal.Interface())
				if val != "" && val != "<nil>" {
					return val
				}
			}
		}
		
		return ""
	}
	
	// Handle struct
	if v.Kind() == reflect.Struct {
		field := v.FieldByName(fieldName)
		if field.IsValid() {
			if field.Kind() == reflect.String {
				return field.String()
			}
			return fmt.Sprint(field.Interface())
		}
	}
	
	return ""
}

// parseM3UFromReader parses an M3U playlist from an io.Reader
// This is similar to m3u.Parse but works with an io.Reader instead of a filename/URL
func parseM3UFromReader(r io.Reader) (m3u.Playlist, error) {
	onFirstLine := true
	scanner := bufio.NewScanner(r)
	tagsRegExp, _ := regexp.Compile("([a-zA-Z0-9-]+?)=\"([^\"]+)\"")
	playlist := m3u.Playlist{}

	for scanner.Scan() {
		line := scanner.Text()
		if onFirstLine && !strings.HasPrefix(line, "#EXTM3U") {
			return m3u.Playlist{},
				errors.New("invalid m3u file format. Expected #EXTM3U file header")
		}

		onFirstLine = false

		if strings.HasPrefix(line, "#EXTINF") {
			line := strings.Replace(line, "#EXTINF:", "", -1)
			trackInfo := strings.Split(line, ",")
			if len(trackInfo) < 2 {
				return m3u.Playlist{},
					errors.New("invalid m3u file format. Expected EXTINF metadata to contain track length and name data")
			}
			length, parseErr := strconv.Atoi(strings.Split(trackInfo[0], " ")[0])
			if parseErr != nil {
				return m3u.Playlist{}, errors.New("unable to parse length")
			}
			track := &m3u.Track{strings.Trim(trackInfo[1], " "), length, "", nil}
			tagList := tagsRegExp.FindAllString(line, -1)
			for i := range tagList {
				tagInfo := strings.Split(tagList[i], "=")
				tag := &m3u.Tag{tagInfo[0], strings.Replace(tagInfo[1], "\"", "", -1)}
				track.Tags = append(track.Tags, *tag)
			}
			playlist.Tracks = append(playlist.Tracks, *track)
		} else if strings.HasPrefix(line, "#") || line == "" {
			continue
		} else if len(playlist.Tracks) == 0 {
			return m3u.Playlist{},
				errors.New("URI provided for playlist with no tracks")

		} else {
			playlist.Tracks[len(playlist.Tracks)-1].URI = strings.Trim(line, " ")
		}
	}

	if err := scanner.Err(); err != nil {
		return m3u.Playlist{}, fmt.Errorf("error reading M3U content: %v", err)
	}

	return playlist, nil
}

func (c *Config) cacheXtreamM3u(playlist *m3u.Playlist, cacheName string) error {
	xtreamM3uCacheLock.Lock()
	defer xtreamM3uCacheLock.Unlock()

	tmp := *c
	tmp.playlist = playlist

	path := filepath.Join(os.TempDir(), uuid.NewV4().String()+".iptv-proxy.m3u")
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()

	if err := tmp.marshallInto(f, true); err != nil {
		return err
	}
	xtreamM3uCache[cacheName] = cacheMeta{path, time.Now()}

	return nil
}

func (c *Config) xtreamGenerateM3u(ctx *gin.Context, output string) (*m3u.Playlist, error) {
	// Format the extension from output parameter
	extension := getExtensionFromOutput(output)
	log.Printf("[iptv-proxy] DEBUG: Starting xtreamGenerateM3u with output: %s, extension: %s\n", output, extension)
	
	log.Printf("[iptv-proxy] DEBUG: Creating Xtream client with user: %s, baseURL: %s\n", c.XtreamUser.String(), c.XtreamBaseURL)
	
	// Always use a proper browser-like User-Agent for Xtream API calls
	// Cloudflare blocks suspicious User-Agents like "python-requests" or "go.xstream-codes"
	// We ignore the incoming User-Agent to ensure we always use a browser-like one
	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	log.Printf("[iptv-proxy] DEBUG: Using User-Agent for Xtream API: %s (ignoring client User-Agent: %s)\n", userAgent, ctx.Request.UserAgent())
	
	// Retry logic for Cloudflare 520 errors (often transient)
	maxRetries := 3
	var client *xtreamapi.Client
	var err error
	for attempt := 1; attempt <= maxRetries; attempt++ {
		client, err = xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
		if err == nil {
			log.Printf("[iptv-proxy] DEBUG: Xtream client created successfully\n")
			break
		}
		
		// Check if it's a 520 error (Cloudflare) - these are often transient
		if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
			if attempt < maxRetries {
				waitTime := time.Duration(attempt) * 2 * time.Second
				log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error on attempt %d/%d, retrying in %v: %v\n", attempt, maxRetries, waitTime, err)
				time.Sleep(waitTime)
				continue
			}
		}
		
		log.Printf("[iptv-proxy] ERROR: Failed to create Xtream client (attempt %d/%d): %v\n", attempt, maxRetries, err)
		if attempt == maxRetries {
			return nil, err
		}
	}
	
	if err != nil {
		log.Printf("[iptv-proxy] ERROR: Failed to create Xtream client after %d attempts: %v\n", maxRetries, err)
		return nil, err
	}

	log.Printf("[iptv-proxy] DEBUG: Fetching live categories...\n")
	
	// Try to get from cache first
	cacheKey := "iptv:live:categories"
	cacheTTL := 7 * 24 * time.Hour // 1 week
	var cat interface{}
	cacheHit := false
	
	if c.redisCache != nil {
		var cachedCat interface{}
		if found, cacheErr := c.redisCache.GetJSON(cacheKey, &cachedCat); found && cacheErr == nil {
			cat = cachedCat
			cacheHit = true
			log.Printf("[iptv-proxy] DEBUG: Live categories cache HIT\n")
		} else if cacheErr != nil {
			log.Printf("[iptv-proxy] WARNING: Redis cache error for live categories: %v, falling back to API\n", cacheErr)
		}
	}
	
	// If cache miss, fetch from API
	if !cacheHit {
		// Retry logic for API calls that might fail due to Cloudflare (520 errors are often transient)
		catRetries := 3
		for attempt := 1; attempt <= catRetries; attempt++ {
			categories, getErr := client.GetLiveCategories()
			if getErr == nil {
				cat = categories
				err = nil
				// Store in cache
				if c.redisCache != nil {
					if cacheErr := c.redisCache.SetJSON(cacheKey, cat, cacheTTL); cacheErr != nil {
						log.Printf("[iptv-proxy] WARNING: Failed to cache live categories: %v\n", cacheErr)
					} else {
						log.Printf("[iptv-proxy] DEBUG: Live categories cached successfully\n")
					}
				}
				break
			}
			err = getErr
			
			// Check if it's a 520 error (Cloudflare) - these are often transient
			if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
				if attempt < catRetries {
					waitTime := time.Duration(attempt) * 2 * time.Second
					log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting live categories (attempt %d/%d), retrying in %v: %v\n", attempt, catRetries, waitTime, err)
					time.Sleep(waitTime)
					continue
				}
			}
			
			log.Printf("[iptv-proxy] ERROR: Failed to get live categories (attempt %d/%d): %v\n", attempt, catRetries, err)
			if attempt == catRetries {
				return nil, err
			}
		}
		
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to get live categories after %d attempts: %v\n", catRetries, err)
			return nil, err
		}
	}
	
	// GetLiveCategories returns a slice - use reflection to get length
	catValueRef := reflect.ValueOf(cat)
	if catValueRef.Kind() != reflect.Slice {
		return nil, fmt.Errorf("expected slice from GetLiveCategories, got %T", cat)
	}
	catLen := catValueRef.Len()
	log.Printf("[iptv-proxy] DEBUG: Retrieved %d live categories\n", catLen)

	// For Xtream API, we generate URLs in the format: {baseURL}/{username}/{password}/{stream_id}
	// The replaceURL function will convert these to proxy URLs with /live/ prefix and .ts extension

	var playlist = new(m3u.Playlist)
	playlist.Tracks = make([]m3u.Track, 0)

	totalTracks := 0
	catValue := reflect.ValueOf(cat)
	for i := 0; i < catValue.Len(); i++ {
		categoryElem := catValue.Index(i)
		categoryIDStr := getFieldValue(categoryElem, "ID")
		categoryNameStr := getFieldValue(categoryElem, "Name")
		
		// Only include enabled categories (skip if not enabled)
		if !globalCategoryFilter.isCategoryEnabled("live", categoryIDStr) {
			log.Printf("[iptv-proxy] DEBUG: Skipping disabled live category: %s (ID: %s)\n", categoryNameStr, categoryIDStr)
			continue
		}
		
		// Log progress every 50 categories to reduce log spam
		if i%50 == 0 || i == catValue.Len()-1 {
			log.Printf("[iptv-proxy] DEBUG: Processing category %d/%d: %s (ID: %s) - %d tracks so far\n", i+1, catValue.Len(), categoryNameStr, categoryIDStr, totalTracks)
		}
		
		// Retry logic for GetLiveStreams (may also fail with 520 errors)
		streamCacheKey := fmt.Sprintf("iptv:live:streams:%s", categoryIDStr)
		var live interface{}
		streamCacheHit := false
		
		if c.redisCache != nil {
			var cachedStreams interface{}
			if found, cacheErr := c.redisCache.GetJSON(streamCacheKey, &cachedStreams); found && cacheErr == nil {
				live = cachedStreams
				streamCacheHit = true
				err = nil
				log.Printf("[iptv-proxy] DEBUG: Live streams cache HIT for category %s\n", categoryNameStr)
			} else if cacheErr != nil {
				log.Printf("[iptv-proxy] WARNING: Redis cache error for live streams category %s: %v, falling back to API\n", categoryNameStr, cacheErr)
			}
		}
		
		if !streamCacheHit {
			streamRetries := 3
			for attempt := 1; attempt <= streamRetries; attempt++ {
				streams, streamErr := client.GetLiveStreams(categoryIDStr)
				if streamErr == nil {
					live = streams
					err = nil
					// Store in cache
					if c.redisCache != nil {
						if cacheErr := c.redisCache.SetJSON(streamCacheKey, live, cacheTTL); cacheErr != nil {
							log.Printf("[iptv-proxy] WARNING: Failed to cache live streams for category %s: %v\n", categoryNameStr, cacheErr)
						}
					}
					break
				}
				err = streamErr
				
				// Check if it's a 520 error (Cloudflare) - these are often transient
				if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
					if attempt < streamRetries {
						waitTime := time.Duration(attempt) * 1 * time.Second
						log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting streams for category %s (attempt %d/%d), retrying in %v\n", categoryNameStr, attempt, streamRetries, waitTime)
						time.Sleep(waitTime)
						continue
					}
				}
				
				if attempt == streamRetries {
					log.Printf("[iptv-proxy] ERROR: Failed to get live streams for category %s after %d attempts: %v\n", categoryNameStr, streamRetries, err)
					return nil, err
				}
			}
			
			if err != nil {
				log.Printf("[iptv-proxy] ERROR: Failed to get live streams for category %s: %v\n", categoryNameStr, err)
				return nil, err
			}
		}
		
		// Convert live to slice using reflection
		liveValue := reflect.ValueOf(live)
		if liveValue.Kind() != reflect.Slice {
			log.Printf("[iptv-proxy] ERROR: Expected slice from GetLiveStreams, got %T\n", live)
			continue
		}
		liveLen := liveValue.Len()
		if i%50 == 0 || i == catValue.Len()-1 {
			log.Printf("[iptv-proxy] DEBUG: Category %s has %d streams\n", categoryNameStr, liveLen)
		}

		for j := 0; j < liveLen; j++ {
			streamElem := liveValue.Index(j)
			streamNameStr := getFieldValue(streamElem, "Name")
			streamIDStr := getFieldValue(streamElem, "ID")
			streamEPGStr := getFieldValue(streamElem, "EPGChannelID")
			streamIconStr := getFieldValue(streamElem, "Icon")
			
			track := m3u.Track{Name: streamNameStr, Length: -1, URI: "", Tags: nil}

			//TODO: Add more tag if needed.
			if streamEPGStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-id", Value: streamEPGStr})
			}
			if streamNameStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-name", Value: streamNameStr})
			}
			if streamIconStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-logo", Value: streamIconStr})
			}
			if categoryNameStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "group-title", Value: categoryNameStr})
			}

			// Generate URL in Xtream format: {baseURL}/{username}/{password}/{stream_id}
			// The replaceURL function will convert this to proxy format: /live/{proxy_user}/{proxy_pass}/{stream_id}.ts
			track.URI = fmt.Sprintf("%s/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, streamIDStr)
			playlist.Tracks = append(playlist.Tracks, track)
			totalTracks++
		}
	}

	log.Printf("[iptv-proxy] DEBUG: Live TV: Generated %d tracks\n", totalTracks)

	// Fetch VOD (Movies) categories and streams
	log.Printf("[iptv-proxy] DEBUG: Fetching VOD (Movies) categories...\n")
	vodCacheKey := "iptv:vod:categories"
	var vodCats interface{}
	vodCacheHit := false
	
	if c.redisCache != nil {
		var cachedVodCats interface{}
		if found, cacheErr := c.redisCache.GetJSON(vodCacheKey, &cachedVodCats); found && cacheErr == nil {
			vodCats = cachedVodCats
			vodCacheHit = true
			log.Printf("[iptv-proxy] DEBUG: VOD categories cache HIT\n")
		} else if cacheErr != nil {
			log.Printf("[iptv-proxy] WARNING: Redis cache error for VOD categories: %v, falling back to API\n", cacheErr)
		}
	}
	
	if !vodCacheHit {
		vodRetries := 3
		for attempt := 1; attempt <= vodRetries; attempt++ {
			categories, getErr := client.GetVideoOnDemandCategories()
			if getErr == nil {
				vodCats = categories
				err = nil
				// Store in cache
				if c.redisCache != nil {
					if cacheErr := c.redisCache.SetJSON(vodCacheKey, vodCats, cacheTTL); cacheErr != nil {
						log.Printf("[iptv-proxy] WARNING: Failed to cache VOD categories: %v\n", cacheErr)
					} else {
						log.Printf("[iptv-proxy] DEBUG: VOD categories cached successfully\n")
					}
				}
				break
			}
			err = getErr
			
			// Check if it's a 520 error (Cloudflare) - these are often transient
			if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
				if attempt < vodRetries {
					waitTime := time.Duration(attempt) * 2 * time.Second
					log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting VOD categories (attempt %d/%d), retrying in %v: %v\n", attempt, vodRetries, waitTime, err)
					time.Sleep(waitTime)
					continue
				}
			}
			
			log.Printf("[iptv-proxy] ERROR: Failed to get VOD categories (attempt %d/%d): %v\n", attempt, vodRetries, err)
			if attempt == vodRetries {
				return nil, err
			}
		}
		
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to get VOD categories after %d attempts: %v\n", vodRetries, err)
			return nil, err
		}
	}
	
	vodCatsValue := reflect.ValueOf(vodCats)
	if vodCatsValue.Kind() != reflect.Slice {
		return nil, fmt.Errorf("expected slice from GetVideoOnDemandCategories, got %T", vodCats)
	}
	vodCatsLen := vodCatsValue.Len()
	log.Printf("[iptv-proxy] DEBUG: Retrieved %d VOD categories\n", vodCatsLen)

	vodTracks := 0
	for i := 0; i < vodCatsLen; i++ {
		categoryElem := vodCatsValue.Index(i)
		categoryIDStr := getFieldValue(categoryElem, "ID")
		categoryNameStr := getFieldValue(categoryElem, "Name")
		
		// Only include enabled categories (skip if not enabled)
		if !globalCategoryFilter.isCategoryEnabled("movies", categoryIDStr) {
			log.Printf("[iptv-proxy] DEBUG: Skipping disabled movies category: %s (ID: %s)\n", categoryNameStr, categoryIDStr)
			continue
		}
		
		if i%50 == 0 || i == vodCatsLen-1 {
			log.Printf("[iptv-proxy] DEBUG: Processing VOD category %d/%d: %s (ID: %s) - %d VOD tracks so far\n", i+1, vodCatsLen, categoryNameStr, categoryIDStr, vodTracks)
		}
		
		// Retry logic for GetVideoOnDemandStreams
		vodStreamCacheKey := fmt.Sprintf("iptv:vod:streams:%s", categoryIDStr)
		var movies interface{}
		vodStreamCacheHit := false
		
		if c.redisCache != nil {
			var cachedMovies interface{}
			if found, cacheErr := c.redisCache.GetJSON(vodStreamCacheKey, &cachedMovies); found && cacheErr == nil {
				movies = cachedMovies
				vodStreamCacheHit = true
				err = nil
				log.Printf("[iptv-proxy] DEBUG: VOD streams cache HIT for category %s\n", categoryNameStr)
			} else if cacheErr != nil {
				log.Printf("[iptv-proxy] WARNING: Redis cache error for VOD streams category %s: %v, falling back to API\n", categoryNameStr, cacheErr)
			}
		}
		
		if !vodStreamCacheHit {
			movieRetries := 3
			for attempt := 1; attempt <= movieRetries; attempt++ {
				streams, streamErr := client.GetVideoOnDemandStreams(categoryIDStr)
				if streamErr == nil {
					movies = streams
					err = nil
					// Store in cache
					if c.redisCache != nil {
						if cacheErr := c.redisCache.SetJSON(vodStreamCacheKey, movies, cacheTTL); cacheErr != nil {
							log.Printf("[iptv-proxy] WARNING: Failed to cache VOD streams for category %s: %v\n", categoryNameStr, cacheErr)
						}
					}
					break
				}
				err = streamErr
				
				if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
					if attempt < movieRetries {
						waitTime := time.Duration(attempt) * 1 * time.Second
						log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting VOD streams for category %s (attempt %d/%d), retrying in %v\n", categoryNameStr, attempt, movieRetries, waitTime)
						time.Sleep(waitTime)
						continue
					}
				}
				
				if attempt == movieRetries {
					log.Printf("[iptv-proxy] ERROR: Failed to get VOD streams for category %s after %d attempts: %v\n", categoryNameStr, movieRetries, err)
					return nil, err
				}
			}
			
			if err != nil {
				log.Printf("[iptv-proxy] ERROR: Failed to get VOD streams for category %s: %v\n", categoryNameStr, err)
				return nil, err
			}
		}
		
		moviesValue := reflect.ValueOf(movies)
		if moviesValue.Kind() != reflect.Slice {
			log.Printf("[iptv-proxy] ERROR: Expected slice from GetVideoOnDemandStreams, got %T\n", movies)
			continue
		}
		moviesLen := moviesValue.Len()
		
		if i%50 == 0 || i == vodCatsLen-1 {
			log.Printf("[iptv-proxy] DEBUG: VOD category %s has %d movies\n", categoryNameStr, moviesLen)
		}

		for j := 0; j < moviesLen; j++ {
			movieElem := moviesValue.Index(j)
			movieNameStr := getFieldValue(movieElem, "Name")
			movieIDStr := getFieldValue(movieElem, "ID")
			movieEPGStr := getFieldValue(movieElem, "EPGChannelID")
			movieIconStr := getFieldValue(movieElem, "Icon")
			movieContainerExtStr := getFieldValue(movieElem, "ContainerExtension")
			
			// Use container_extension from API if available, otherwise fall back to output parameter
			movieExtension := extension
			if movieContainerExtStr != "" {
				movieExtension = "." + movieContainerExtStr
			}
			
			track := m3u.Track{Name: movieNameStr, Length: -1, URI: "", Tags: nil}

			if movieEPGStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-id", Value: movieEPGStr})
			}
			if movieNameStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-name", Value: movieNameStr})
			}
			if movieIconStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-logo", Value: movieIconStr})
			}
			if categoryNameStr != "" {
				track.Tags = append(track.Tags, m3u.Tag{Name: "group-title", Value: categoryNameStr})
			}

			// Movies use /movie/ prefix
			track.URI = fmt.Sprintf("%s/movie/%s/%s/%s%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, movieIDStr, movieExtension)
			playlist.Tracks = append(playlist.Tracks, track)
			vodTracks++
			totalTracks++
		}
	}
	log.Printf("[iptv-proxy] DEBUG: VOD (Movies): Generated %d tracks\n", vodTracks)

	// Fetch Series categories and series
	log.Printf("[iptv-proxy] DEBUG: Fetching Series categories...\n")
	seriesCacheKey := "iptv:series:categories"
	var seriesCats interface{}
	seriesCacheHit := false
	
	if c.redisCache != nil {
		var cachedSeriesCats interface{}
		if found, cacheErr := c.redisCache.GetJSON(seriesCacheKey, &cachedSeriesCats); found && cacheErr == nil {
			seriesCats = cachedSeriesCats
			seriesCacheHit = true
			log.Printf("[iptv-proxy] DEBUG: Series categories cache HIT\n")
		} else if cacheErr != nil {
			log.Printf("[iptv-proxy] WARNING: Redis cache error for Series categories: %v, falling back to API\n", cacheErr)
		}
	}
	
	if !seriesCacheHit {
		seriesRetries := 3
		for attempt := 1; attempt <= seriesRetries; attempt++ {
			categories, getErr := client.GetSeriesCategories()
			if getErr == nil {
				seriesCats = categories
				err = nil
				// Store in cache
				if c.redisCache != nil {
					if cacheErr := c.redisCache.SetJSON(seriesCacheKey, seriesCats, cacheTTL); cacheErr != nil {
						log.Printf("[iptv-proxy] WARNING: Failed to cache Series categories: %v\n", cacheErr)
					} else {
						log.Printf("[iptv-proxy] DEBUG: Series categories cached successfully\n")
					}
				}
				break
			}
			err = getErr
			
			// Check if it's a 520 error (Cloudflare) - these are often transient
			if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
				if attempt < seriesRetries {
					waitTime := time.Duration(attempt) * 2 * time.Second
					log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting Series categories (attempt %d/%d), retrying in %v: %v\n", attempt, seriesRetries, waitTime, err)
					time.Sleep(waitTime)
					continue
				}
			}
			
			log.Printf("[iptv-proxy] ERROR: Failed to get Series categories (attempt %d/%d): %v\n", attempt, seriesRetries, err)
			if attempt == seriesRetries {
				return nil, err
			}
		}
		
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to get Series categories after %d attempts: %v\n", seriesRetries, err)
			return nil, err
		}
	}
	
	seriesCatsValue := reflect.ValueOf(seriesCats)
	if seriesCatsValue.Kind() != reflect.Slice {
		return nil, fmt.Errorf("expected slice from GetSeriesCategories, got %T", seriesCats)
	}
	seriesCatsLen := seriesCatsValue.Len()
	log.Printf("[iptv-proxy] DEBUG: Retrieved %d Series categories\n", seriesCatsLen)

	seriesTracks := 0
	for i := 0; i < seriesCatsLen; i++ {
		categoryElem := seriesCatsValue.Index(i)
		categoryIDStr := getFieldValue(categoryElem, "ID")
		categoryNameStr := getFieldValue(categoryElem, "Name")
		
		// Only include enabled categories (skip if not enabled)
		if !globalCategoryFilter.isCategoryEnabled("series", categoryIDStr) {
			log.Printf("[iptv-proxy] DEBUG: Skipping disabled series category: %s (ID: %s)\n", categoryNameStr, categoryIDStr)
			continue
		}
		
		if i%50 == 0 || i == seriesCatsLen-1 {
			log.Printf("[iptv-proxy] DEBUG: Processing Series category %d/%d: %s (ID: %s) - %d Series tracks so far\n", i+1, seriesCatsLen, categoryNameStr, categoryIDStr, seriesTracks)
		}
		
		// Retry logic for GetSeries
		seriesListCacheKey := fmt.Sprintf("iptv:series:list:%s", categoryIDStr)
		var series interface{}
		seriesListCacheHit := false
		
		if c.redisCache != nil {
			var cachedSeries interface{}
			if found, cacheErr := c.redisCache.GetJSON(seriesListCacheKey, &cachedSeries); found && cacheErr == nil {
				series = cachedSeries
				seriesListCacheHit = true
				err = nil
				log.Printf("[iptv-proxy] DEBUG: Series list cache HIT for category %s\n", categoryNameStr)
			} else if cacheErr != nil {
				log.Printf("[iptv-proxy] WARNING: Redis cache error for series list category %s: %v, falling back to API\n", categoryNameStr, cacheErr)
			}
		}
		
		if !seriesListCacheHit {
			seriesStreamRetries := 3
			for attempt := 1; attempt <= seriesStreamRetries; attempt++ {
				streams, streamErr := client.GetSeries(categoryIDStr)
				if streamErr == nil {
					series = streams
					err = nil
					// Store in cache
					if c.redisCache != nil {
						if cacheErr := c.redisCache.SetJSON(seriesListCacheKey, series, cacheTTL); cacheErr != nil {
							log.Printf("[iptv-proxy] WARNING: Failed to cache series list for category %s: %v\n", categoryNameStr, cacheErr)
						}
					}
					break
				}
				err = streamErr
				
				if strings.Contains(err.Error(), "520") || strings.Contains(err.Error(), "status code was 520") {
					if attempt < seriesStreamRetries {
						waitTime := time.Duration(attempt) * 1 * time.Second
						log.Printf("[iptv-proxy] WARNING: Cloudflare 520 error getting series for category %s (attempt %d/%d), retrying in %v\n", categoryNameStr, attempt, seriesStreamRetries, waitTime)
						time.Sleep(waitTime)
						continue
					}
				}
				
				if attempt == seriesStreamRetries {
					log.Printf("[iptv-proxy] ERROR: Failed to get series for category %s after %d attempts: %v\n", categoryNameStr, seriesStreamRetries, err)
					return nil, err
				}
			}
			
			if err != nil {
				log.Printf("[iptv-proxy] ERROR: Failed to get series for category %s: %v\n", categoryNameStr, err)
				return nil, err
			}
		}
		
		seriesValue := reflect.ValueOf(series)
		if seriesValue.Kind() != reflect.Slice {
			log.Printf("[iptv-proxy] ERROR: Expected slice from GetSeries, got %T\n", series)
			continue
		}
		seriesLen := seriesValue.Len()
		
		if i%50 == 0 || i == seriesCatsLen-1 {
			log.Printf("[iptv-proxy] DEBUG: Series category %s has %d series\n", categoryNameStr, seriesLen)
		}

		for j := 0; j < seriesLen; j++ {
			serieElem := seriesValue.Index(j)
			serieNameStr := getFieldValue(serieElem, "Name")
			serieIDStr := getFieldValue(serieElem, "SeriesID")
			serieCoverStr := getFieldValue(serieElem, "Cover")
			
			// Fetch series info to get episodes with season/episode information
			// Add retry logic for transient errors (500, 520, etc.) and empty array responses
			seriesInfoCacheKey := fmt.Sprintf("iptv:series:info:%s", serieIDStr)
			var seriesInfo *xtreamcodes.Series
			var seriesErr error
			seriesInfoCacheHit := false
			
			// Try cache first
			if c.redisCache != nil {
				var cachedSeriesInfo xtreamcodes.Series
				if found, cacheErr := c.redisCache.GetJSON(seriesInfoCacheKey, &cachedSeriesInfo); found && cacheErr == nil {
					seriesInfo = &cachedSeriesInfo
					seriesInfoCacheHit = true
					seriesErr = nil
					log.Printf("[iptv-proxy] DEBUG: Series info cache HIT for %s (ID: %s)\n", serieNameStr, serieIDStr)
				} else if cacheErr != nil {
					log.Printf("[iptv-proxy] WARNING: Redis cache error for series info %s (ID: %s): %v, falling back to API\n", serieNameStr, serieIDStr, cacheErr)
				}
			}
			
			// If cache miss, fetch from API
			if !seriesInfoCacheHit {
				seriesInfoRetries := 3
				for attempt := 1; attempt <= seriesInfoRetries; attempt++ {
					seriesInfo, seriesErr = client.GetSeriesInfo(serieIDStr)
					
					// Check if we got a valid response with episodes
					hasValidEpisodes := false
					if seriesErr == nil && seriesInfo != nil {
						// Check if series has episodes
						if seriesInfo.Episodes != nil && len(seriesInfo.Episodes) > 0 {
							totalEpisodes := 0
							for _, episodes := range seriesInfo.Episodes {
								totalEpisodes += len(episodes)
							}
							if totalEpisodes > 0 {
								hasValidEpisodes = true
							}
						}
						// If we have valid episodes, we're done
						if hasValidEpisodes {
							// Store in cache
							if c.redisCache != nil {
								if cacheErr := c.redisCache.SetJSON(seriesInfoCacheKey, seriesInfo, cacheTTL); cacheErr != nil {
									log.Printf("[iptv-proxy] WARNING: Failed to cache series info for %s (ID: %s): %v\n", serieNameStr, serieIDStr, cacheErr)
								}
							}
							break
						}
						// If we have series info but no episodes, check if it's an empty array response
						// (empty array responses have no Info.Name and no Episodes)
						if seriesInfo.Info.Name == "" && (seriesInfo.Episodes == nil || len(seriesInfo.Episodes) == 0) {
							// This is likely an empty array response - treat as transient and retry
							seriesErr = fmt.Errorf("API returned empty array [] for series ID %s", serieIDStr)
						}
					}
					
					// Check if it's a transient error (500, 520, etc.) or empty array that we should retry
					shouldRetry := false
					if seriesErr != nil {
						errStr := seriesErr.Error()
						if strings.Contains(errStr, "500") || strings.Contains(errStr, "status code was 500") ||
							strings.Contains(errStr, "520") || strings.Contains(errStr, "status code was 520") ||
							strings.Contains(errStr, "502") || strings.Contains(errStr, "status code was 502") ||
							strings.Contains(errStr, "503") || strings.Contains(errStr, "status code was 503") ||
							strings.Contains(errStr, "empty array") {
							shouldRetry = true
						}
					}
					
					if shouldRetry && attempt < seriesInfoRetries {
						waitTime := time.Duration(attempt) * 1 * time.Second
						log.Printf("[iptv-proxy] WARNING: Transient error getting series info for %s (ID: %s) (attempt %d/%d), retrying in %v: %v\n", serieNameStr, serieIDStr, attempt, seriesInfoRetries, waitTime, seriesErr)
						time.Sleep(waitTime)
						continue
					}
					
					// If we've exhausted retries or it's not a retryable error, log and break
					if attempt == seriesInfoRetries {
						log.Printf("[iptv-proxy] WARNING: Failed to get series info for %s (ID: %s) after %d attempts: %v\n", serieNameStr, serieIDStr, seriesInfoRetries, seriesErr)
					}
					break
				}
			}
			
			if seriesErr != nil || seriesInfo == nil {
				// Skip this series and continue with the next one
				continue
			}
			
			// Debug: Log episode information to help diagnose issues
			episodesCount := 0
			if seriesInfo.Episodes != nil {
				for _, episodes := range seriesInfo.Episodes {
					episodesCount += len(episodes)
				}
			}
			
			// Skip if series has no episodes
			if seriesInfo.Episodes == nil || len(seriesInfo.Episodes) == 0 || episodesCount == 0 {
				// Check if we have series info but no episodes (might indicate API issue or legitimately no episodes)
				hasInfo := seriesInfo.Info.Name != "" || seriesInfo.Info.Cover != ""
				if hasInfo {
					log.Printf("[iptv-proxy] DEBUG: Series %s (ID: %s) has series info but no episodes (Info.Name: %s, Episodes map len: %d, total episodes: %d), skipping\n", 
						serieNameStr, serieIDStr, seriesInfo.Info.Name, len(seriesInfo.Episodes), episodesCount)
				} else {
					log.Printf("[iptv-proxy] DEBUG: Series %s (ID: %s) has no episodes and no series info (Episodes map len: %d, total episodes: %d), skipping\n", 
						serieNameStr, serieIDStr, len(seriesInfo.Episodes), episodesCount)
				}
				continue
			}
			
			log.Printf("[iptv-proxy] DEBUG: Series %s (ID: %s) has %d seasons with %d total episodes\n", 
				serieNameStr, serieIDStr, len(seriesInfo.Episodes), episodesCount)
			
			// Generate M3U entries for each episode
			for seasonKey, episodes := range seriesInfo.Episodes {
				for _, episode := range episodes {
					// Get season and episode numbers
					seasonNum := int(episode.Season)
					episodeNum := int(episode.EpisodeNum)
					
					// Format season/episode as S01E01
					seasonEpisode := fmt.Sprintf("S%02dE%02d", seasonNum, episodeNum)
					
					// Get episode title
					episodeTitle := episode.Title
					if episodeTitle == "" && episode.Info.Name != "" {
						episodeTitle = episode.Info.Name
					}
					
					var episodeName string
					// Check if episode title already contains both series name and season/episode format
					// If it does, use it as-is to avoid duplication
					hasSeasonEpisode := strings.Contains(episodeTitle, seasonEpisode)
					hasSeriesName := serieNameStr != "" && strings.Contains(episodeTitle, serieNameStr)
					
					if episodeTitle != "" && hasSeasonEpisode && hasSeriesName {
						// Episode title already has series name and season/episode, use as-is
						episodeName = episodeTitle
					} else if episodeTitle != "" && hasSeasonEpisode {
						// Episode title has season/episode but not series name, add series name
						episodeName = fmt.Sprintf("%s - %s", serieNameStr, episodeTitle)
					} else if episodeTitle != "" {
						// Episode title is just the episode name, build full name
						episodeName = fmt.Sprintf("%s - %s - %s", serieNameStr, seasonEpisode, episodeTitle)
					} else {
						// No episode title, just use series name and season/episode
						episodeName = fmt.Sprintf("%s - %s", serieNameStr, seasonEpisode)
					}
					
					// Get container extension from episode
					episodeExtension := extension
					if episode.ContainerExtension != "" {
						episodeExtension = "." + episode.ContainerExtension
					}
					
					// Get episode ID
					episodeID := episode.ID
					if episodeID == "" {
						log.Printf("[iptv-proxy] WARNING: Episode %s has no ID, skipping\n", episodeName)
						continue
					}
					
					track := m3u.Track{Name: episodeName, Length: -1, URI: "", Tags: nil}
					
					// Use series cover for episode if available
					if serieCoverStr != "" {
						track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-logo", Value: serieCoverStr})
					}
					if episodeName != "" {
						track.Tags = append(track.Tags, m3u.Tag{Name: "tvg-name", Value: episodeName})
					}
					if categoryNameStr != "" {
						track.Tags = append(track.Tags, m3u.Tag{Name: "group-title", Value: categoryNameStr})
					}
					
					// Episodes use /series/ prefix with episode ID
					track.URI = fmt.Sprintf("%s/series/%s/%s/%s%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, episodeID, episodeExtension)
					playlist.Tracks = append(playlist.Tracks, track)
					seriesTracks++
					totalTracks++
				}
				_ = seasonKey // seasonKey is the season number as string, we use episode.Season instead
			}
		}
	}
	log.Printf("[iptv-proxy] DEBUG: Series: Generated %d tracks\n", seriesTracks)

	log.Printf("[iptv-proxy] DEBUG: Generated M3U playlist with %d total tracks (Live: %d, VOD: %d, Series: %d)\n", totalTracks, totalTracks-vodTracks-seriesTracks, vodTracks, seriesTracks)
	return playlist, nil
}

func (c *Config) xtreamGetAuto(ctx *gin.Context) {
	newQuery := ctx.Request.URL.Query()
	q := c.RemoteURL.Query()
	for k, v := range q {
		if k == "username" || k == "password" {
			continue
		}

		newQuery.Add(k, strings.Join(v, ","))
	}
	ctx.Request.URL.RawQuery = newQuery.Encode()

	c.xtreamGet(ctx)
}

func (c *Config) xtreamGet(ctx *gin.Context) {
	// Build URL with proper query parameter encoding
	m3uURL, err := url.Parse(c.XtreamBaseURL)
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}
	
	m3uURL.Path = "/get.php"
	query := m3uURL.Query()
	query.Set("username", c.XtreamUser.String())
	query.Set("password", c.XtreamPassword.String())
	
	// Add other query parameters from the request
	q := ctx.Request.URL.Query()
	for k, v := range q {
		if k == "username" || k == "password" {
			continue
		}
		query[k] = v
	}
	
	m3uURL.RawQuery = query.Encode()

	xtreamM3uCacheLock.RLock()
	meta, ok := xtreamM3uCache[m3uURL.String()]
	d := time.Since(meta.Time)
	if !ok || d.Hours() >= float64(c.M3UCacheExpiration) {
		log.Printf("[iptv-proxy] %v | %s | xtream cache m3u file\n", time.Now().Format("2006/01/02 - 15:04:05"), ctx.ClientIP())
		xtreamM3uCacheLock.RUnlock()
		
		// Use proper HTTP client instead of m3u.Parse's simple http.Get()
		req, err := http.NewRequest("GET", m3uURL.String(), nil)
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to create request: %v\n", err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		
		// Always use a proper browser-like User-Agent for Xtream API calls
		// Cloudflare blocks suspicious User-Agents like "python-requests" or default Go user agents
		// We ignore the incoming User-Agent to ensure we always use a browser-like one
		userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
		req.Header.Set("User-Agent", userAgent)
		log.Printf("[iptv-proxy] DEBUG: Using User-Agent for Xtream get.php request: %s (ignoring client User-Agent: %s)\n", userAgent, ctx.Request.UserAgent())
		
		resp, err := sharedHTTPClient.Do(req)
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to fetch M3U from Xtream: %v\n", err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		defer resp.Body.Close()
		
		// Read the response body regardless of status code to see error messages
		body, err := ioutil.ReadAll(resp.Body)
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to read M3U response: %v\n", err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		
		if resp.StatusCode != http.StatusOK {
			bodyStr := strings.TrimSpace(string(body))
			log.Printf("[iptv-proxy] ERROR: Xtream server returned status %d for M3U request. Response: %s\n", resp.StatusCode, bodyStr[:min(500, len(bodyStr))])
			log.Printf("[iptv-proxy] DEBUG: Request URL was: %s\n", m3uURL.String())
			// Some Xtream servers return non-standard status codes but still include valid M3U content
			// Try to parse it anyway if it looks like M3U content
			if strings.Contains(bodyStr, "#EXTM3U") {
				log.Printf("[iptv-proxy] WARNING: Non-200 status but response contains #EXTM3U, attempting to parse anyway\n")
			} else {
				ctx.AbortWithStatus(resp.StatusCode)
				return
			}
		}
		
		// Check if the response is empty or only contains #EXTM3U
		bodyStr := strings.TrimSpace(string(body))
		if bodyStr == "" || bodyStr == "#EXTM3U" || !strings.Contains(bodyStr, "#EXTINF") {
			log.Printf("[iptv-proxy] WARNING: Xtream server returned empty or invalid M3U file (length: %d bytes). Response preview: %s\n", len(body), bodyStr[:min(200, len(bodyStr))])
			// Try to log the full URL for debugging
			log.Printf("[iptv-proxy] DEBUG: Request URL was: %s\n", m3uURL.String())
		}
		
		// Parse the M3U content from the response body
		playlist, err := parseM3UFromReader(strings.NewReader(string(body)))
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to parse M3U content: %v\n", err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		
		if len(playlist.Tracks) == 0 {
			log.Printf("[iptv-proxy] WARNING: Parsed M3U playlist contains 0 tracks. This might indicate an authentication or configuration issue.\n")
		} else {
			log.Printf("[iptv-proxy] Successfully parsed M3U with %d tracks\n", len(playlist.Tracks))
		}
		
		if err := c.cacheXtreamM3u(&playlist, m3uURL.String()); err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to cache M3U: %v\n", err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
	} else {
		xtreamM3uCacheLock.RUnlock()
	}

	ctx.Header("Content-Disposition", fmt.Sprintf(`attachment; filename=%q`, c.M3UFileName))
	xtreamM3uCacheLock.RLock()
	path := xtreamM3uCache[m3uURL.String()].string
	xtreamM3uCacheLock.RUnlock()
	ctx.Header("Content-Type", "application/octet-stream")

	ctx.File(path)
}

// getExtensionFromOutput maps Xtream output formats to file extensions
// This is used as a fallback when container_extension is not available from the API
// Returns the extension with a leading dot, defaults to ".ts" (mpegts) if output is empty
func getExtensionFromOutput(output string) string {
	// Default to mpegts (.ts) if no output specified
	if output == "" {
		return ".ts"
	}
	
	// Map common output formats to standard file extensions
	extensionMap := map[string]string{
		"mpegts": "ts",
		"ts":     "ts",
		"hls":    "m3u8",
		"mkv":    "mkv",
		"mp4":    "mp4",
		"flv":    "flv",
		"webm":   "webm",
	}
	
	// Check if we have a mapping, otherwise use the output value directly
	if ext, ok := extensionMap[output]; ok {
		return "." + ext
	}
	
	// Default: use output value as extension
	return "." + output
}

func (c *Config) xtreamApiGet(ctx *gin.Context) {
	const (
		apiGet = "apiget"
	)

	var (
		output    = ctx.Query("output")
		extension = getExtensionFromOutput(output)
		cacheName = apiGet + output
	)

	log.Printf("[iptv-proxy] DEBUG: xtreamApiGet called with extension: %s, cacheName: %s\n", extension, cacheName)

	xtreamM3uCacheLock.RLock()
	meta, ok := xtreamM3uCache[cacheName]
	d := time.Since(meta.Time)
	cacheExpired := !ok || d.Hours() >= float64(c.M3UCacheExpiration)
	xtreamM3uCacheLock.RUnlock()

	log.Printf("[iptv-proxy] DEBUG: Cache check - exists: %v, expired: %v, age: %v hours, expiration: %d hours\n", ok, cacheExpired, d.Hours(), c.M3UCacheExpiration)

	if cacheExpired {
		log.Printf("[iptv-proxy] %v | %s | xtream cache API m3u file\n", time.Now().Format("2006/01/02 - 15:04:05"), ctx.ClientIP())
		log.Printf("[iptv-proxy] DEBUG: Cache expired or missing, generating new M3U...\n")
		startTime := time.Now()
		playlist, err := c.xtreamGenerateM3u(ctx, output)
		elapsed := time.Since(startTime)
		if err != nil {
			log.Printf("[iptv-proxy] ERROR: xtreamGenerateM3u failed after %v: %v\n", elapsed, err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		log.Printf("[iptv-proxy] DEBUG: M3U generation completed in %v, caching...\n", elapsed)
		
		startTime = time.Now()
		if err := c.cacheXtreamM3u(playlist, cacheName); err != nil {
			log.Printf("[iptv-proxy] ERROR: cacheXtreamM3u failed after %v: %v\n", time.Since(startTime), err)
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		log.Printf("[iptv-proxy] DEBUG: M3U cached successfully in %v\n", time.Since(startTime))
	} else {
		log.Printf("[iptv-proxy] DEBUG: Using cached M3U file\n")
	}

	ctx.Header("Content-Disposition", fmt.Sprintf(`attachment; filename=%q`, c.M3UFileName))
	xtreamM3uCacheLock.RLock()
	path := xtreamM3uCache[cacheName].string
	xtreamM3uCacheLock.RUnlock()
	ctx.Header("Content-Type", "application/octet-stream")

	log.Printf("[iptv-proxy] DEBUG: Serving M3U file from: %s\n", path)
	ctx.File(path)

}

func (c *Config) xtreamPlayerAPIGET(ctx *gin.Context) {
	c.xtreamPlayerAPI(ctx, ctx.Request.URL.Query())
}

func (c *Config) xtreamPlayerAPIPOST(ctx *gin.Context) {
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

	c.xtreamPlayerAPI(ctx, q)
}

func (c *Config) xtreamPlayerAPI(ctx *gin.Context, q url.Values) {
	var action string
	if len(q["action"]) > 0 {
		action = q["action"][0]
	}

	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, ctx.Request.UserAgent())
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	resp, httpcode, err := client.Action(c.ProxyConfig, action, q)
	if err != nil {
		ctx.AbortWithError(httpcode, err) // nolint: errcheck
		return
	}

	log.Printf("[iptv-proxy] %v | %s |Action\t%s\n", time.Now().Format("2006/01/02 - 15:04:05"), ctx.ClientIP(), action)

	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	ctx.JSON(http.StatusOK, resp)
}

func (c *Config) xtreamXMLTV(ctx *gin.Context) {
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, ctx.Request.UserAgent())
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	resp, err := client.GetXMLTV()
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	ctx.Data(http.StatusOK, "application/xml", resp)
}

func (c *Config) xtreamStreamHandler(ctx *gin.Context) {
	id := ctx.Param("id")
	rpURL, err := url.Parse(fmt.Sprintf("%s/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, id))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, rpURL)
}

func (c *Config) xtreamStreamLive(ctx *gin.Context) {
	id := ctx.Param("id")
	// Remove .ts extension if present (Xtream API doesn't use extensions in the URL)
	streamID := strings.TrimSuffix(id, ".ts")
	// Xtream API format for live streams is: {baseURL}/{username}/{password}/{stream_id}
	// NOT {baseURL}/live/{username}/{password}/{stream_id}
	rpURL, err := url.Parse(fmt.Sprintf("%s/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, streamID))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, rpURL)
}

func (c *Config) xtreamStreamPlay(ctx *gin.Context) {
	token := ctx.Param("token")
	t := ctx.Param("type")
	rpURL, err := url.Parse(fmt.Sprintf("%s/play/%s/%s", c.XtreamBaseURL, token, t))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, rpURL)
}

func (c *Config) xtreamStreamTimeshift(ctx *gin.Context) {
	duration := ctx.Param("duration")
	start := ctx.Param("start")
	id := ctx.Param("id")
	rpURL, err := url.Parse(fmt.Sprintf("%s/timeshift/%s/%s/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, duration, start, id))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.stream(ctx, rpURL)
}

func (c *Config) xtreamStreamMovie(ctx *gin.Context) {
	id := ctx.Param("id")
	// Remove file extension if present (Xtream API doesn't use extensions in the URL)
	streamID := strings.TrimSuffix(id, ".ts")
	streamID = strings.TrimSuffix(streamID, ".mkv")
	streamID = strings.TrimSuffix(streamID, ".mp4")
	rpURL, err := url.Parse(fmt.Sprintf("%s/movie/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, streamID))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, rpURL)
}

func (c *Config) xtreamStreamSeries(ctx *gin.Context) {
	id := ctx.Param("id")
	// Remove file extension if present (Xtream API doesn't use extensions in the URL)
	streamID := strings.TrimSuffix(id, ".ts")
	streamID = strings.TrimSuffix(streamID, ".mkv")
	streamID = strings.TrimSuffix(streamID, ".mp4")
	rpURL, err := url.Parse(fmt.Sprintf("%s/series/%s/%s/%s", c.XtreamBaseURL, c.XtreamUser, c.XtreamPassword, streamID))
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, rpURL)
}

func (c *Config) xtreamHlsStream(ctx *gin.Context) {
	chunk := ctx.Param("chunk")
	s := strings.Split(chunk, "_")
	if len(s) != 2 {
		ctx.AbortWithError( // nolint: errcheck
			http.StatusInternalServerError,
			errors.New("HSL malformed chunk"),
		)
		return
	}
	channel := s[0]

	url, err := getHlsRedirectURL(channel)
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	req, err := url.Parse(
		fmt.Sprintf(
			"%s://%s/hls/%s/%s",
			url.Scheme,
			url.Host,
			ctx.Param("token"),
			ctx.Param("chunk"),
		),
	)

	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, req)
}

func (c *Config) xtreamHlsrStream(ctx *gin.Context) {
	channel := ctx.Param("channel")

	url, err := getHlsRedirectURL(channel)
	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	req, err := url.Parse(
		fmt.Sprintf(
			"%s://%s/hlsr/%s/%s/%s/%s/%s/%s",
			url.Scheme,
			url.Host,
			ctx.Param("token"),
			c.XtreamUser,
			c.XtreamPassword,
			ctx.Param("channel"),
			ctx.Param("hash"),
			ctx.Param("chunk"),
		),
	)

	if err != nil {
		ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
		return
	}

	c.xtreamStream(ctx, req)
}

func getHlsRedirectURL(channel string) (*url.URL, error) {
	hlsChannelsRedirectURLLock.RLock()
	defer hlsChannelsRedirectURLLock.RUnlock()

	url, ok := hlsChannelsRedirectURL[channel+".m3u8"]
	if !ok {
		return nil, errors.New("HSL redirect url not found")
	}

	return &url, nil
}

func (c *Config) hlsXtreamStream(ctx *gin.Context, oriURL *url.URL) {
	client := sharedHTTPClientHLS

	const rateLimitStatusCode = 458
	const retryInterval = 5 * time.Second // Retry every 5 seconds after 458 response
	const rateLimitCooldown = 30 * time.Second // How long to mark URL as rate-limited
	retryTimeout := time.Duration(c.RateLimitRetryTimeout) * time.Minute // Keep retrying for configured minutes after first 458

	urlStr := oriURL.String()

	var resp *http.Response
	var first458Time *time.Time
	attempt := 0

	// Retry loop for initial request
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
				log.Printf("[iptv-proxy] %v | HLS: Rate limit (458) detected, will retry for up to %v\n",
					time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout)
			}

			// Check if we've exceeded the retry timeout
			elapsed := time.Since(*first458Time)
			if elapsed >= retryTimeout {
				log.Printf("[iptv-proxy] %v | HLS: Rate limit (458) persisted for %v, returning error after %d attempts\n",
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

			log.Printf("[iptv-proxy] %v | HLS: Rate limit (458) detected on attempt %d, retrying in %v (retrying for %v more)\n",
				time.Now().Format("2006/01/02 - 15:04:05"), attempt, backoffDuration, remainingTime-backoffDuration)
			time.Sleep(backoffDuration)
			continue
		}

		// Success or non-458 error - clear rate limit tracking and break out of retry loop
		globalRateLimitTracker.clearRateLimit(urlStr)
		break
	}
	defer resp.Body.Close()

	if resp.StatusCode == http.StatusFound {
		location, err := resp.Location()
		if err != nil {
			ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
			return
		}
		id := ctx.Param("id")
		if strings.Contains(location.String(), id) {
			hlsChannelsRedirectURLLock.Lock()
			hlsChannelsRedirectURL[id] = *location
			hlsChannelsRedirectURLLock.Unlock()

			locationStr := location.String()

			var hlsResp *http.Response
			var first458TimeRedirect *time.Time
			redirectAttempt := 0

			// Retry loop for redirect request
			for {
				redirectAttempt++
				
				// Check if redirect URL is currently rate-limited and wait if necessary (transparent to client)
				// This prevents duplicate requests but doesn't block the client connection
				globalRateLimitTracker.waitIfRateLimited(locationStr, retryInterval)
				
				hlsReq, err := http.NewRequest("GET", locationStr, nil)
				if err != nil {
					ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
					return
				}

				mergeHttpHeader(hlsReq.Header, ctx.Request.Header)

				hlsResp, err = client.Do(hlsReq)
				if err != nil {
					ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
					return
				}

				// Check for rate limiting (458) response on redirect
				if hlsResp.StatusCode == rateLimitStatusCode {
					hlsResp.Body.Close()

					// Track when we first encountered 458
					if first458TimeRedirect == nil {
						now := time.Now()
						first458TimeRedirect = &now
						log.Printf("[iptv-proxy] %v | HLS redirect: Rate limit (458) detected, will retry for up to %v\n",
							time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout)
					}

					// Check if we've exceeded the retry timeout
					elapsed := time.Since(*first458TimeRedirect)
					if elapsed >= retryTimeout {
						log.Printf("[iptv-proxy] %v | HLS redirect: Rate limit (458) persisted for %v, returning error after %d attempts\n",
							time.Now().Format("2006/01/02 - 15:04:05"), retryTimeout, redirectAttempt)
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
					globalRateLimitTracker.markRateLimited(locationStr, rateLimitCooldown)

					log.Printf("[iptv-proxy] %v | HLS redirect: Rate limit (458) detected on attempt %d, retrying in %v (retrying for %v more)\n",
						time.Now().Format("2006/01/02 - 15:04:05"), redirectAttempt, backoffDuration, remainingTime-backoffDuration)
					time.Sleep(backoffDuration)
					continue
				}

				// Success or non-458 error - clear rate limit tracking and break out of retry loop
				globalRateLimitTracker.clearRateLimit(locationStr)
				break
			}
			defer hlsResp.Body.Close()

			b, err := ioutil.ReadAll(hlsResp.Body)
			if err != nil {
				ctx.AbortWithError(http.StatusInternalServerError, err) // nolint: errcheck
				return
			}
			body := string(b)
			body = strings.ReplaceAll(body, "/"+c.XtreamUser.String()+"/"+c.XtreamPassword.String()+"/", "/"+c.User.String()+"/"+c.Password.String()+"/")

			mergeHttpHeader(ctx.Writer.Header(), hlsResp.Header)

			ctx.Data(http.StatusOK, hlsResp.Header.Get("Content-Type"), []byte(body))
			return
		}
		ctx.AbortWithError(http.StatusInternalServerError, errors.New("Unable to HLS stream")) // nolint: errcheck
		return
	}

	ctx.Status(resp.StatusCode)
}

// Refresh callback functions for cache refresh worker

// refreshLiveCategories refreshes live categories cache
func (c *Config) refreshLiveCategories(key string) error {
	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	categories, err := client.GetLiveCategories()
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, categories, cacheTTL)
}

// refreshLiveStreams refreshes live streams for a category
func (c *Config) refreshLiveStreams(key string) error {
	// Extract category ID from key: iptv:live:streams:{categoryID}
	parts := strings.Split(key, ":")
	if len(parts) != 4 {
		return fmt.Errorf("invalid cache key format: %s", key)
	}
	categoryID := parts[3]

	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	streams, err := client.GetLiveStreams(categoryID)
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, streams, cacheTTL)
}

// refreshVodCategories refreshes VOD categories cache
func (c *Config) refreshVodCategories(key string) error {
	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	categories, err := client.GetVideoOnDemandCategories()
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, categories, cacheTTL)
}

// refreshVodStreams refreshes VOD streams for a category
func (c *Config) refreshVodStreams(key string) error {
	// Extract category ID from key: iptv:vod:streams:{categoryID}
	parts := strings.Split(key, ":")
	if len(parts) != 4 {
		return fmt.Errorf("invalid cache key format: %s", key)
	}
	categoryID := parts[3]

	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	streams, err := client.GetVideoOnDemandStreams(categoryID)
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, streams, cacheTTL)
}

// refreshSeriesCategories refreshes series categories cache
func (c *Config) refreshSeriesCategories(key string) error {
	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	categories, err := client.GetSeriesCategories()
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, categories, cacheTTL)
}

// refreshSeriesList refreshes series list for a category
func (c *Config) refreshSeriesList(key string) error {
	// Extract category ID from key: iptv:series:list:{categoryID}
	parts := strings.Split(key, ":")
	if len(parts) != 4 {
		return fmt.Errorf("invalid cache key format: %s", key)
	}
	categoryID := parts[3]

	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	series, err := client.GetSeries(categoryID)
	if err != nil {
		return err
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, series, cacheTTL)
}

// refreshSeriesInfo refreshes series info (episodes) for a series
func (c *Config) refreshSeriesInfo(key string) error {
	// Extract series ID from key: iptv:series:info:{seriesID}
	parts := strings.Split(key, ":")
	if len(parts) != 4 {
		return fmt.Errorf("invalid cache key format: %s", key)
	}
	seriesID := parts[3]

	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		return err
	}

	seriesInfo, err := client.GetSeriesInfo(seriesID)
	if err != nil {
		return err
	}

	// Check if we got valid data
	if seriesInfo == nil || (seriesInfo.Info.Name == "" && (seriesInfo.Episodes == nil || len(seriesInfo.Episodes) == 0)) {
		return fmt.Errorf("empty response for series ID %s", seriesID)
	}

	cacheTTL := 7 * 24 * time.Hour
	return c.redisCache.SetJSON(key, seriesInfo, cacheTTL)
}

