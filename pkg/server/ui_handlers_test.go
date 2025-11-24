package server

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/pierre-emmanuelJ/iptv-proxy/pkg/config"
)

func setupTestConfig() *Config {
	return &Config{
		ProxyConfig: &config.ProxyConfig{
			HostConfig: &config.HostConfiguration{
				Hostname: "localhost",
				Port:     8080,
			},
			XtreamUser:           config.CredentialString("testuser"),
			XtreamPassword:       config.CredentialString("testpass"),
			XtreamBaseURL:        "http://test.example.com",
			User:                 config.CredentialString("proxyuser"),
			Password:             config.CredentialString("proxypass"),
			CategoryFiltersPath:  filepath.Join(os.TempDir(), "test_category_filters.json"),
			RateLimitRetryTimeout: 10,
		},
	}
}

func TestServeWebUI(t *testing.T) {
	gin.SetMode(gin.TestMode)

	cfg := setupTestConfig()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/", nil)

	cfg.serveWebUI(ctx)

	if w.Code != http.StatusOK {
		t.Errorf("Expected status 200, got %d", w.Code)
	}

	body := w.Body.String()
	if !strings.Contains(body, "<title>IPTV Proxy - Category Manager</title>") {
		t.Error("Expected HTML to contain title")
	}
	if !strings.Contains(body, "IPTV Proxy") {
		t.Error("Expected HTML to contain 'IPTV Proxy'")
	}
	if !strings.Contains(body, "Live TV") {
		t.Error("Expected HTML to contain 'Live TV' section")
	}
	if !strings.Contains(body, "Movies") {
		t.Error("Expected HTML to contain 'Movies' section")
	}
	if !strings.Contains(body, "Series") {
		t.Error("Expected HTML to contain 'Series' section")
	}
	if !strings.Contains(body, "api/categories") {
		t.Error("Expected HTML to contain API endpoint reference")
	}
}

func TestGetCategoriesHandler_noXtreamConfig(t *testing.T) {
	gin.SetMode(gin.TestMode)

	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			XtreamBaseURL: "", // No Xtream configured
		},
	}

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/api/categories", nil)

	cfg.getCategoriesHandler(ctx)

	if w.Code != http.StatusBadRequest {
		t.Errorf("Expected status 400, got %d", w.Code)
	}

	var response map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &response); err != nil {
		t.Fatalf("Failed to parse response: %v", err)
	}

	if response["error"] == nil {
		t.Error("Expected error message in response")
	}
}

func TestUpdateCategoriesHandler(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// Reset global filter
	originalFilter := globalCategoryFilter
	globalCategoryFilter = &categoryFilter{
		enabledCats: make(map[string]map[string]bool),
	}
	defer func() {
		globalCategoryFilter = originalFilter
	}()

	// Create temporary directory for filters file
	tmpDir := t.TempDir()
	cfg := setupTestConfig()
	cfg.CategoryFiltersPath = filepath.Join(tmpDir, "category_filters.json")

	// Prepare request
	requestBody := UpdateCategoriesRequest{
		Enabled: map[string]map[string]bool{
			"live": {
				"1": true,
				"2": true,
			},
			"movies": {
				"10": true,
			},
		},
	}

	jsonData, err := json.Marshal(requestBody)
	if err != nil {
		t.Fatalf("Failed to marshal request: %v", err)
	}

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBuffer(jsonData))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	if w.Code != http.StatusOK {
		t.Errorf("Expected status 200, got %d", w.Code)
	}

	var response map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &response); err != nil {
		t.Fatalf("Failed to parse response: %v", err)
	}

	if response["message"] == nil {
		t.Error("Expected success message in response")
	}

	// Verify categories were set
	if !globalCategoryFilter.isCategoryEnabled("live", "1") {
		t.Error("Expected live category 1 to be enabled")
	}
	if !globalCategoryFilter.isCategoryEnabled("live", "2") {
		t.Error("Expected live category 2 to be enabled")
	}
	if !globalCategoryFilter.isCategoryEnabled("movies", "10") {
		t.Error("Expected movies category 10 to be enabled")
	}

	// Verify file was created
	if _, err := os.Stat(cfg.CategoryFiltersPath); os.IsNotExist(err) {
		t.Error("Expected category filters file to be created")
	}
}

func TestUpdateCategoriesHandler_invalidJSON(t *testing.T) {
	gin.SetMode(gin.TestMode)

	cfg := setupTestConfig()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBufferString("invalid json"))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	if w.Code != http.StatusBadRequest {
		t.Errorf("Expected status 400, got %d", w.Code)
	}

	var response map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &response); err != nil {
		t.Fatalf("Failed to parse response: %v", err)
	}

	if response["error"] == nil {
		t.Error("Expected error message in response")
	}
}

func TestUpdateCategoriesHandler_emptyBody(t *testing.T) {
	gin.SetMode(gin.TestMode)

	cfg := setupTestConfig()

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBufferString("{}"))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	// Should succeed with empty disabled map
	if w.Code != http.StatusOK {
		t.Errorf("Expected status 200, got %d", w.Code)
	}
}

func TestUpdateCategoriesHandler_clearsCache(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// Reset global filter and cache
	originalFilter := globalCategoryFilter
	globalCategoryFilter = &categoryFilter{
		enabledCats: make(map[string]map[string]bool),
	}
	defer func() {
		globalCategoryFilter = originalFilter
	}()

	// Set up cache
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = map[string]cacheMeta{
		"test": {string: "/tmp/test.m3u", Time: time.Now()},
	}
	xtreamM3uCacheLock.Unlock()

	cfg := setupTestConfig()

	requestBody := UpdateCategoriesRequest{
		Enabled: map[string]map[string]bool{
			"live": {"1": true},
		},
	}

	jsonData, _ := json.Marshal(requestBody)

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBuffer(jsonData))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	// Verify cache was cleared
	xtreamM3uCacheLock.RLock()
	cacheLen := len(xtreamM3uCache)
	xtreamM3uCacheLock.RUnlock()

	if cacheLen != 0 {
		t.Error("Expected cache to be cleared after updating categories")
	}
}

func TestGetCategoriesHandler_xtreamError(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// This test would require mocking the Xtream client, which is complex
	// For now, we test the error handling path with invalid credentials
	cfg := &Config{
		ProxyConfig: &config.ProxyConfig{
			XtreamUser:    config.CredentialString("invalid"),
			XtreamPassword: config.CredentialString("invalid"),
			XtreamBaseURL: "http://invalid.example.com",
		},
	}

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("GET", "/api/categories", nil)

	cfg.getCategoriesHandler(ctx)

	// Should return error status
	if w.Code < 400 {
		t.Errorf("Expected error status (>=400), got %d", w.Code)
	}
}

func TestUpdateCategoriesHandler_fileSaveError(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// Reset global filter
	originalFilter := globalCategoryFilter
	globalCategoryFilter = &categoryFilter{
		enabledCats: make(map[string]map[string]bool),
	}
	defer func() {
		globalCategoryFilter = originalFilter
	}()

	// Use invalid path (directory that doesn't exist and can't be created)
	cfg := setupTestConfig()
	cfg.CategoryFiltersPath = "/nonexistent/path/that/cannot/be/created/category_filters.json"

	requestBody := UpdateCategoriesRequest{
		Enabled: map[string]map[string]bool{
			"live": {"1": true},
		},
	}

	jsonData, _ := json.Marshal(requestBody)

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBuffer(jsonData))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	// Should return error if file save fails
	if w.Code != http.StatusInternalServerError {
		t.Errorf("Expected status 500 on file save error, got %d", w.Code)
	}
}

func TestUpdateCategoriesHandler_noFiltersPath(t *testing.T) {
	gin.SetMode(gin.TestMode)

	// Reset global filter
	originalFilter := globalCategoryFilter
	globalCategoryFilter = &categoryFilter{
		enabledCats: make(map[string]map[string]bool),
	}
	defer func() {
		globalCategoryFilter = originalFilter
	}()

	cfg := setupTestConfig()
	cfg.CategoryFiltersPath = "" // No path configured

	requestBody := UpdateCategoriesRequest{
		Enabled: map[string]map[string]bool{
			"live": {"1": true},
		},
	}

	jsonData, _ := json.Marshal(requestBody)

	w := httptest.NewRecorder()
	ctx, _ := gin.CreateTestContext(w)
	ctx.Request = httptest.NewRequest("POST", "/api/categories", bytes.NewBuffer(jsonData))
	ctx.Request.Header.Set("Content-Type", "application/json")

	cfg.updateCategoriesHandler(ctx)

	// Should still succeed (just won't save to file)
	if w.Code != http.StatusOK {
		t.Errorf("Expected status 200 even without filters path, got %d", w.Code)
	}

	// Verify categories were still set in memory
	if !globalCategoryFilter.isCategoryEnabled("live", "1") {
		t.Error("Expected categories to be set in memory even without file path")
	}
}

