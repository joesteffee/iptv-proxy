package server

import (
	"bytes"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"
	"github.com/pierre-emmanuelJ/iptv-proxy/pkg/config"
)

func TestParseM3UFromReader(t *testing.T) {
	tests := []struct {
		name        string
		input       string
		expectError bool
		expectTracks int
	}{
		{
			name: "valid M3U with single track",
			input: `#EXTM3U
#EXTINF:-1 tvg-id="test1" tvg-name="Test Channel" tvg-logo="http://example.com/logo.png" group-title="Test Group",Test Channel
http://example.com/stream1.m3u8
`,
			expectError:  false,
			expectTracks: 1,
		},
		{
			name: "valid M3U with multiple tracks",
			input: `#EXTM3U
#EXTINF:-1 tvg-id="test1" tvg-name="Test Channel 1" group-title="Group 1",Test Channel 1
http://example.com/stream1.m3u8
#EXTINF:-1 tvg-id="test2" tvg-name="Test Channel 2" group-title="Group 2",Test Channel 2
http://example.com/stream2.m3u8
`,
			expectError:  false,
			expectTracks: 2,
		},
		{
			name: "M3U with empty lines and comments",
			input: `#EXTM3U

#EXTINF:-1 tvg-name="Test Channel",Test Channel
http://example.com/stream1.m3u8

`,
			expectError:  false,
			expectTracks: 1,
		},
		{
			name: "invalid M3U - missing EXTM3U header",
			input: `#EXTINF:-1 tvg-name="Test Channel",Test Channel
http://example.com/stream1.m3u8
`,
			expectError:  true,
			expectTracks: 0,
		},
		{
			name: "empty M3U - only header",
			input: `#EXTM3U
`,
			expectError:  false,
			expectTracks: 0,
		},
		{
			name: "M3U with URI but no tracks",
			input: `#EXTM3U
http://example.com/stream1.m3u8
`,
			expectError:  true,
			expectTracks: 0,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			reader := strings.NewReader(tt.input)
			playlist, err := parseM3UFromReader(reader)

			if tt.expectError {
				if err == nil {
					t.Errorf("Expected error but got none")
				}
				return
			}

			if err != nil {
				t.Errorf("Unexpected error: %v", err)
				return
			}

			if len(playlist.Tracks) != tt.expectTracks {
				t.Errorf("Expected %d tracks, got %d", tt.expectTracks, len(playlist.Tracks))
			}

			// Verify track content for valid cases
			if tt.expectTracks > 0 && len(playlist.Tracks) > 0 {
				firstTrack := playlist.Tracks[0]
				if firstTrack.URI == "" {
					t.Error("Expected track to have URI")
				}
			}
		})
	}
}

func TestParseM3UFromReader_TrackParsing(t *testing.T) {
	input := `#EXTM3U
#EXTINF:-1 tvg-id="test1" tvg-name="Test Channel" tvg-logo="http://example.com/logo.png" group-title="Test Group",Test Channel Name
http://example.com/stream1.m3u8
`

	reader := strings.NewReader(input)
	playlist, err := parseM3UFromReader(reader)
	if err != nil {
		t.Fatalf("Unexpected error: %v", err)
	}

	if len(playlist.Tracks) != 1 {
		t.Fatalf("Expected 1 track, got %d", len(playlist.Tracks))
	}

	track := playlist.Tracks[0]
	if track.Name != "Test Channel Name" {
		t.Errorf("Expected track name 'Test Channel Name', got '%s'", track.Name)
	}

	if track.URI != "http://example.com/stream1.m3u8" {
		t.Errorf("Expected URI 'http://example.com/stream1.m3u8', got '%s'", track.URI)
	}

	if track.Length != -1 {
		t.Errorf("Expected length -1, got %d", track.Length)
	}

	// Verify tags
	tagMap := make(map[string]string)
	for _, tag := range track.Tags {
		tagMap[tag.Name] = tag.Value
	}

	if tagMap["tvg-id"] != "test1" {
		t.Errorf("Expected tvg-id 'test1', got '%s'", tagMap["tvg-id"])
	}
	if tagMap["tvg-name"] != "Test Channel" {
		t.Errorf("Expected tvg-name 'Test Channel', got '%s'", tagMap["tvg-name"])
	}
	if tagMap["tvg-logo"] != "http://example.com/logo.png" {
		t.Errorf("Expected tvg-logo 'http://example.com/logo.png', got '%s'", tagMap["tvg-logo"])
	}
	if tagMap["group-title"] != "Test Group" {
		t.Errorf("Expected group-title 'Test Group', got '%s'", tagMap["group-title"])
	}
}

func TestMin(t *testing.T) {
	tests := []struct {
		a, b, expected int
	}{
		{1, 2, 1},
		{2, 1, 1},
		{5, 5, 5},
		{0, 10, 0},
		{-5, 5, -5},
	}

	for _, tt := range tests {
		result := min(tt.a, tt.b)
		if result != tt.expected {
			t.Errorf("min(%d, %d) = %d, expected %d", tt.a, tt.b, result, tt.expected)
		}
	}
}

func TestXtreamGet_URLEncoding(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// Create a test server that captures the request URL
	var capturedURL string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedURL = r.URL.String()
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n"))
	}))
	defer server.Close()

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("test+user@example.com"),
			XtreamPassword:       config.CredentialString("test&password=123"),
			XtreamBaseURL:        server.URL,
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			M3UCacheExpiration:   1,
			M3UFileName:          "test.m3u",
			RateLimitRetryTimeout: 10,
		},
	}

	// Clear cache to force a new request
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass&type=m3u_plus&output=ts", nil)
	ctx.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx)

	// Verify URL was properly encoded
	if !strings.Contains(capturedURL, "test%2Buser%40example.com") {
		t.Errorf("Expected URL to contain encoded username, got: %s", capturedURL)
	}
	if !strings.Contains(capturedURL, "test%26password%3D123") {
		t.Errorf("Expected URL to contain encoded password, got: %s", capturedURL)
	}
}

func TestXtreamGet_ErrorHandling(t *testing.T) {
	gin.SetMode(gin.TestMode)

	tests := []struct {
		name           string
		statusCode     int
		responseBody   string
		expectError    bool
		expectLogError bool
	}{
		{
			name:         "successful response",
			statusCode:   http.StatusOK,
			responseBody: "#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n",
			expectError:  false,
		},
		{
			name:         "non-standard status code 884",
			statusCode:   884,
			responseBody: "",
			expectError:  true,
		},
		{
			name:         "HTTP 500 error",
			statusCode:   http.StatusInternalServerError,
			responseBody: "Internal Server Error",
			expectError:  true,
		},
		{
			name:         "empty response body",
			statusCode:   http.StatusOK,
			responseBody: "",
			expectError:  false, // Should parse but log warning
		},
		{
			name:         "response with only EXTM3U header",
			statusCode:   http.StatusOK,
			responseBody: "#EXTM3U\n",
			expectError:  false, // Should parse but log warning
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(tt.statusCode)
				w.Write([]byte(tt.responseBody))
			}))
			defer server.Close()

			cfg := &Config{
				ProxyConfig: &config.ProxyConfig{
					HostConfig: &config.HostConfiguration{
						Hostname: "localhost",
						Port:     8080,
					},
					XtreamUser:           config.CredentialString("testuser"),
					XtreamPassword:       config.CredentialString("testpass"),
					XtreamBaseURL:        server.URL,
					User:                 config.CredentialString("proxyuser"),
					Password:             config.CredentialString("proxypass"),
					M3UCacheExpiration:   1,
					M3UFileName:          "test.m3u",
					RateLimitRetryTimeout: 10,
				},
			}

			// Clear cache
			xtreamM3uCacheLock.Lock()
			xtreamM3uCache = make(map[string]cacheMeta)
			xtreamM3uCacheLock.Unlock()

			w := httptest.NewRecorder()
			ctx, _ := gin.CreateTestContext(w)
			ctx.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass&type=m3u_plus", nil)
			ctx.Request.SetBasicAuth("proxyuser", "proxypass")

			cfg.xtreamGet(ctx)

			if tt.expectError {
				if w.Code < 400 {
					t.Errorf("Expected error status code (>=400), got %d", w.Code)
				}
			} else {
				if w.Code >= 400 {
					t.Errorf("Expected success status code (<400), got %d", w.Code)
				}
			}
		})
	}
}

func TestXtreamGet_UserAgent(t *testing.T) {
	gin.SetMode(gin.TestMode)

	var capturedUserAgent string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedUserAgent = r.Header.Get("User-Agent")
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n"))
	}))
	defer server.Close()

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("testuser"),
			XtreamPassword:       config.CredentialString("testpass"),
			XtreamBaseURL:        server.URL,
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			M3UCacheExpiration:   1,
			M3UFileName:          "test.m3u",
			RateLimitRetryTimeout: 10,
		},
	}

	// Clear cache
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass", nil)
	ctx.Request.Header.Set("User-Agent", "Test-User-Agent/1.0")
	ctx.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx)

	// We now always use a browser-like User-Agent to avoid Cloudflare blocking,
	// regardless of what the client sends (especially important for Python requests)
	expectedUserAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	if capturedUserAgent != expectedUserAgent {
		t.Errorf("Expected User-Agent '%s', got '%s'", expectedUserAgent, capturedUserAgent)
	}
}

func TestXtreamGet_DefaultUserAgent(t *testing.T) {
	gin.SetMode(gin.TestMode)

	var capturedUserAgent string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedUserAgent = r.Header.Get("User-Agent")
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n"))
	}))
	defer server.Close()

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("testuser"),
			XtreamPassword:       config.CredentialString("testpass"),
			XtreamBaseURL:        server.URL,
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			M3UCacheExpiration:   1,
			M3UFileName:          "test.m3u",
			RateLimitRetryTimeout: 10,
		},
	}

	// Clear cache
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass", nil)
	// Don't set User-Agent to test default
	ctx.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx)

	if capturedUserAgent == "" {
		t.Error("Expected default User-Agent to be set, got empty string")
	}
	if !strings.Contains(capturedUserAgent, "Mozilla") {
		t.Errorf("Expected default User-Agent to contain 'Mozilla', got '%s'", capturedUserAgent)
	}
}

func TestXtreamGet_Caching(t *testing.T) {
	gin.SetMode(gin.TestMode)

	requestCount := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestCount++
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n"))
	}))
	defer server.Close()

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("testuser"),
			XtreamPassword:       config.CredentialString("testpass"),
			XtreamBaseURL:        server.URL,
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			M3UCacheExpiration:   24, // 24 hours - cache should be used
			M3UFileName:          "test.m3u",
			RateLimitRetryTimeout: 10,
		},
	}

	// Clear cache
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()

	// First request - should fetch from server
	w1 := httptest.NewRecorder()
	ctx1, _ := gin.CreateTestContext(w1)
	ctx1.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass&type=m3u_plus", nil)
	ctx1.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx1)

	if requestCount != 1 {
		t.Errorf("Expected 1 request on first call, got %d", requestCount)
	}

	// Second request - should use cache
	w2 := httptest.NewRecorder()
	ctx2, _ := gin.CreateTestContext(w2)
	ctx2.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass&type=m3u_plus", nil)
	ctx2.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx2)

	if requestCount != 1 {
		t.Errorf("Expected cache to be used, but got %d requests (should still be 1)", requestCount)
	}
}

// Test helper to create a mock M3U response
func createMockM3UResponse(tracks []string) string {
	var buf bytes.Buffer
	buf.WriteString("#EXTM3U\n")
	for i, track := range tracks {
		buf.WriteString("#EXTINF:-1 tvg-name=\"Channel " + string(rune(i+1)) + "\",Channel " + string(rune(i+1)) + "\n")
		buf.WriteString(track + "\n")
	}
	return buf.String()
}

func TestParseM3UFromReader_EdgeCases(t *testing.T) {
	tests := []struct {
		name        string
		input       string
		expectError bool
		description string
	}{
		{
			name:        "very long line",
			input:        "#EXTM3U\n#EXTINF:-1 " + strings.Repeat("tvg-name=\"Test\" ", 1000) + ",Test\nhttp://example.com/stream\n",
			expectError: false,
			description: "Should handle very long EXTINF lines",
		},
		{
			name:        "special characters in values",
			input:        "#EXTM3U\n#EXTINF:-1 tvg-name=\"Test & Channel\" group-title=\"Group/Subgroup\",Test & Channel\nhttp://example.com/stream?param=value&other=123\n",
			expectError: false,
			description: "Should handle special characters in tag values",
		},
		{
			name:        "missing comma separator",
			input:        "#EXTM3U\n#EXTINF:-1 tvg-name=\"Test\" Test Channel\nhttp://example.com/stream\n",
			expectError: true,
			description: "Should error on missing comma in EXTINF",
		},
		{
			name:        "invalid length value",
			input:        "#EXTM3U\n#EXTINF:invalid tvg-name=\"Test\",Test Channel\nhttp://example.com/stream\n",
			expectError: true,
			description: "Should error on invalid length value",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			reader := strings.NewReader(tt.input)
			playlist, err := parseM3UFromReader(reader)

			if tt.expectError {
				if err == nil {
					t.Errorf("Expected error for: %s", tt.description)
				}
			} else {
				if err != nil {
					t.Errorf("Unexpected error for %s: %v", tt.description, err)
				}
				if len(playlist.Tracks) == 0 {
					t.Errorf("Expected at least one track for: %s", tt.description)
				}
			}
		})
	}
}

func TestXtreamGet_QueryParameterPreservation(t *testing.T) {
	gin.SetMode(gin.TestMode)

	var capturedURL string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedURL = r.URL.String()
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("#EXTM3U\n#EXTINF:-1,Test Channel\nhttp://example.com/stream\n"))
	}))
	defer server.Close()

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("testuser"),
			XtreamPassword:       config.CredentialString("testpass"),
			XtreamBaseURL:        server.URL,
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			M3UCacheExpiration:   1,
			M3UFileName:          "test.m3u",
			RateLimitRetryTimeout: 10,
		},
	}

	// Clear cache
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/get.php?username=proxyuser&password=proxypass&type=m3u_plus&output=ts&custom=value", nil)
	ctx.Request.SetBasicAuth("proxyuser", "proxypass")

	cfg.xtreamGet(ctx)

	// Verify custom query parameters are preserved (username and password should be replaced)
	if !strings.Contains(capturedURL, "type=m3u_plus") {
		t.Errorf("Expected URL to contain 'type=m3u_plus', got: %s", capturedURL)
	}
	if !strings.Contains(capturedURL, "output=ts") {
		t.Errorf("Expected URL to contain 'output=ts', got: %s", capturedURL)
	}
	if !strings.Contains(capturedURL, "custom=value") {
		t.Errorf("Expected URL to contain 'custom=value', got: %s", capturedURL)
	}
	// Username and password should be from XtreamUser/XtreamPassword, not from request
	if strings.Contains(capturedURL, "username=proxyuser") {
		t.Errorf("Expected URL to use XtreamUser, not proxy user, got: %s", capturedURL)
	}
}

// Note: Testing xtreamGenerateM3u would require mocking the Xtream API client,
// which is more complex. These tests focus on the HTTP client improvements
// and M3U parsing functionality that we added.

