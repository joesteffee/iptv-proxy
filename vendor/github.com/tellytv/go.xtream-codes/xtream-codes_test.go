package xtreamcodes

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestGetSeriesInfo_EmptyArray(t *testing.T) {
	// Create a test server that returns empty array
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("action") == "get_series_info" {
			w.WriteHeader(http.StatusOK)
			w.Write([]byte("[]"))
			return
		}
		// Handle authentication
		authResponse := `{
			"user_info": {
				"username": "testuser",
				"password": "testpass",
				"auth": 1,
				"status": "Active"
			},
			"server_info": {
				"url": "test.com",
				"port": "80"
			}
		}`
		w.WriteHeader(http.StatusOK)
		w.Write([]byte(authResponse))
	}))
	defer server.Close()

	client, err := NewClient("testuser", "testpass", server.URL)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}

	series, err := client.GetSeriesInfo("123")
	if err != nil {
		t.Fatalf("GetSeriesInfo should not return error for empty array: %v", err)
	}

	if series == nil {
		t.Fatal("GetSeriesInfo should return Series struct, not nil")
	}

	// Should have empty Episodes map
	if series.Episodes == nil {
		t.Error("Expected Episodes to be initialized as empty map, got nil")
	}
	if len(series.Episodes) != 0 {
		t.Errorf("Expected empty Episodes map, got %d entries", len(series.Episodes))
	}

	// Should have empty Info
	if series.Info.Name != "" {
		t.Errorf("Expected empty Info.Name, got '%s'", series.Info.Name)
	}

	// Should have empty Seasons
	if series.Seasons == nil {
		t.Error("Expected Seasons to be initialized as empty slice, got nil")
	}
	if len(series.Seasons) != 0 {
		t.Errorf("Expected empty Seasons slice, got %d entries", len(series.Seasons))
	}
}

func TestGetSeriesInfo_ValidResponse(t *testing.T) {
	// Create a test server that returns valid series info
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("action") == "get_series_info" {
			seriesResponse := `{
				"info": {
					"name": "Test Series",
					"cover": "http://example.com/cover.jpg",
					"series_id": 123
				},
				"episodes": {
					"1": [
						{
							"id": "101",
							"episode_num": 1,
							"title": "Episode 1",
							"container_extension": "mkv",
							"season": 1
						}
					]
				},
				"seasons": [1]
			}`
			w.WriteHeader(http.StatusOK)
			w.Write([]byte(seriesResponse))
			return
		}
		// Handle authentication
		authResponse := `{
			"user_info": {
				"username": "testuser",
				"password": "testpass",
				"auth": 1,
				"status": "Active"
			},
			"server_info": {
				"url": "test.com",
				"port": "80"
			}
		}`
		w.WriteHeader(http.StatusOK)
		w.Write([]byte(authResponse))
	}))
	defer server.Close()

	client, err := NewClient("testuser", "testpass", server.URL)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}

	series, err := client.GetSeriesInfo("123")
	if err != nil {
		t.Fatalf("GetSeriesInfo should not return error: %v", err)
	}

	if series == nil {
		t.Fatal("GetSeriesInfo should return Series struct, not nil")
	}

	// Check Info
	if series.Info.Name != "Test Series" {
		t.Errorf("Expected Info.Name 'Test Series', got '%s'", series.Info.Name)
	}

	// Check Episodes
	if series.Episodes == nil {
		t.Fatal("Expected Episodes to be initialized, got nil")
	}
	if len(series.Episodes) != 1 {
		t.Errorf("Expected 1 season in Episodes, got %d", len(series.Episodes))
	}

	season1, ok := series.Episodes["1"]
	if !ok {
		t.Error("Expected season '1' in Episodes map")
	}
	if len(season1) != 1 {
		t.Errorf("Expected 1 episode in season 1, got %d", len(season1))
	}
	if season1[0].ID != "101" {
		t.Errorf("Expected episode ID '101', got '%s'", season1[0].ID)
	}
}

func TestGetSeriesInfo_ArrayOfArraysFormat(t *testing.T) {
	// Create a test server that returns array-of-arrays format
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("action") == "get_series_info" {
			seriesResponse := `{
				"info": {
					"name": "Test Series",
					"cover": "http://example.com/cover.jpg",
					"series_id": 123
				},
				"episodes": [
					[
						{
							"id": "101",
							"episode_num": 1,
							"title": "Episode 1",
							"container_extension": "mkv",
							"season": 1
						},
						{
							"id": "102",
							"episode_num": 2,
							"title": "Episode 2",
							"container_extension": "mkv",
							"season": 1
						}
					],
					[
						{
							"id": "201",
							"episode_num": 1,
							"title": "Episode 1",
							"container_extension": "mp4",
							"season": 2
						}
					]
				],
				"seasons": [1, 2]
			}`
			w.WriteHeader(http.StatusOK)
			w.Write([]byte(seriesResponse))
			return
		}
		// Handle authentication
		authResponse := `{
			"user_info": {
				"username": "testuser",
				"password": "testpass",
				"auth": 1,
				"status": "Active"
			},
			"server_info": {
				"url": "test.com",
				"port": "80"
			}
		}`
		w.WriteHeader(http.StatusOK)
		w.Write([]byte(authResponse))
	}))
	defer server.Close()

	client, err := NewClient("testuser", "testpass", server.URL)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}

	series, err := client.GetSeriesInfo("123")
	if err != nil {
		t.Fatalf("GetSeriesInfo should not return error: %v", err)
	}

	if series == nil {
		t.Fatal("GetSeriesInfo should return Series struct, not nil")
	}

	// Check Episodes - should be converted to map format
	if series.Episodes == nil {
		t.Fatal("Expected Episodes to be initialized, got nil")
	}
	if len(series.Episodes) != 2 {
		t.Errorf("Expected 2 seasons in Episodes, got %d", len(series.Episodes))
	}

	// Check Season 1
	season1, ok := series.Episodes["1"]
	if !ok {
		t.Error("Expected season '1' in Episodes map")
	}
	if len(season1) != 2 {
		t.Errorf("Expected 2 episodes in season 1, got %d", len(season1))
	}

	// Check Season 2
	season2, ok := series.Episodes["2"]
	if !ok {
		t.Error("Expected season '2' in Episodes map")
	}
	if len(season2) != 1 {
		t.Errorf("Expected 1 episode in season 2, got %d", len(season2))
	}
}

func TestGetSeriesInfo_HTTPError(t *testing.T) {
	// Create a test server that returns 500 error
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("action") == "get_series_info" {
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		// Handle authentication
		authResponse := `{
			"user_info": {
				"username": "testuser",
				"password": "testpass",
				"auth": 1,
				"status": "Active"
			},
			"server_info": {
				"url": "test.com",
				"port": "80"
			}
		}`
		w.WriteHeader(http.StatusOK)
		w.Write([]byte(authResponse))
	}))
	defer server.Close()

	client, err := NewClient("testuser", "testpass", server.URL)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}

	series, err := client.GetSeriesInfo("123")
	if err == nil {
		t.Error("GetSeriesInfo should return error for 500 status code")
	}
	if series != nil {
		t.Error("GetSeriesInfo should return nil Series on error")
	}
}

