package xtreamcodes

import (
	"encoding/json"
	"log"
	"testing"
)

func TestAuthenticationResponseUnmarshal(t *testing.T) {
	log.Printf("TestAuthenticationResponseUnmarshal: called")
	jsonData := `{
		"user_info": {
			"username": "myxtreamuser",
			"password": "myxtreampass",
			"message": "Less is more..",
			"auth": 1,
			"status": "Active",
			"exp_date": "1733423929",
			"is_trial": "0",
			"active_cons": 0,
			"created_at": "1725565129",
			"max_connections": "4",
			"allowed_output_formats": [
				"m3u8",
				"ts"
			]
		},
		"server_info": {
			"url": "provider.com",
			"port": "80",
			"https_port": "443",
			"server_protocol": "https",
			"rtmp_port": "30002",
			"timezone": "Pacific/Easter",
			"timestamp_now": 1729207595,
			"time_now": "2024-10-17 18:26:35",
			"process": true
		}
	}`

	var authResponse AuthenticationResponse
	err := json.Unmarshal([]byte(jsonData), &authResponse)
	if err != nil {
		t.Fatalf("Failed to unmarshal AuthenticationResponse: %v", err)
	}

	// Pretty print the entire AuthenticationResponse
	prettyJSON, err := json.MarshalIndent(authResponse, "", "  ")
	if err != nil {
		t.Fatalf("Failed to marshal AuthenticationResponse to pretty JSON: %v", err)
	}
	log.Printf("AuthenticationResponse:\n%s", string(prettyJSON))

	// Pretty print UserInfo
	prettyUserInfo, err := json.MarshalIndent(authResponse.UserInfo, "", "  ")
	if err != nil {
		t.Fatalf("Failed to marshal UserInfo to pretty JSON: %v", err)
	}
	log.Printf("UserInfo:\n%s", string(prettyUserInfo))

	// Pretty print ServerInfo
	prettyServerInfo, err := json.MarshalIndent(authResponse.ServerInfo, "", "  ")
	if err != nil {
		t.Fatalf("Failed to marshal ServerInfo to pretty JSON: %v", err)
	}
	log.Printf("ServerInfo:\n%s", string(prettyServerInfo))
}

func TestSeriesUnmarshal_EmptyArray(t *testing.T) {
	jsonData := `[]`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal empty array: %v", err)
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

func TestSeriesUnmarshal_MapFormat(t *testing.T) {
	jsonData := `{
		"info": {
			"name": "Test Series",
			"cover": "http://example.com/cover.jpg",
			"plot": "Test plot",
			"cast": "Actor 1, Actor 2",
			"director": "Director 1",
			"genre": "Drama",
			"releaseDate": "2020-01-01",
			"series_id": 123,
			"rating": 8,
			"rating_5based": 4.0
		},
		"episodes": {
			"1": [
				{
					"id": "101",
					"episode_num": 1,
					"title": "Episode 1",
					"container_extension": "mkv",
					"season": 1,
					"added": "1609459200"
				},
				{
					"id": "102",
					"episode_num": 2,
					"title": "Episode 2",
					"container_extension": "mkv",
					"season": 1,
					"added": "1609545600"
				}
			],
			"2": [
				{
					"id": "201",
					"episode_num": 1,
					"title": "Episode 1",
					"container_extension": "mp4",
					"season": 2,
					"added": "1612137600"
				}
			]
		},
		"seasons": [1, 2]
	}`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal Series with map format: %v", err)
	}

	// Check Info
	if series.Info.Name != "Test Series" {
		t.Errorf("Expected Info.Name 'Test Series', got '%s'", series.Info.Name)
	}

	// Check Episodes
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
	if season1[0].ID != "101" {
		t.Errorf("Expected first episode ID '101', got '%s'", season1[0].ID)
	}

	// Check Season 2
	season2, ok := series.Episodes["2"]
	if !ok {
		t.Error("Expected season '2' in Episodes map")
	}
	if len(season2) != 1 {
		t.Errorf("Expected 1 episode in season 2, got %d", len(season2))
	}
	if season2[0].ID != "201" {
		t.Errorf("Expected first episode ID '201', got '%s'", season2[0].ID)
	}
}

func TestSeriesUnmarshal_ArrayOfArraysFormat(t *testing.T) {
	jsonData := `{
		"info": {
			"name": "Test Series",
			"cover": "http://example.com/cover.jpg",
			"plot": "Test plot",
			"cast": "Actor 1",
			"director": "Director 1",
			"genre": "Drama",
			"releaseDate": "2020-01-01",
			"series_id": 123,
			"rating": 8,
			"rating_5based": 4.0
		},
		"episodes": [
			[
				{
					"id": "101",
					"episode_num": 1,
					"title": "Episode 1",
					"container_extension": "mkv",
					"season": 1,
					"added": "1609459200"
				},
				{
					"id": "102",
					"episode_num": 2,
					"title": "Episode 2",
					"container_extension": "mkv",
					"season": 1,
					"added": "1609545600"
				}
			],
			[
				{
					"id": "201",
					"episode_num": 1,
					"title": "Episode 1",
					"container_extension": "mp4",
					"season": 2,
					"added": "1612137600"
				}
			]
		],
		"seasons": [1, 2]
	}`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal Series with array-of-arrays format: %v", err)
	}

	// Check Info
	if series.Info.Name != "Test Series" {
		t.Errorf("Expected Info.Name 'Test Series', got '%s'", series.Info.Name)
	}

	// Check Episodes - should be converted to map format
	if series.Episodes == nil {
		t.Fatal("Expected Episodes to be initialized, got nil")
	}
	if len(series.Episodes) != 2 {
		t.Errorf("Expected 2 seasons in Episodes, got %d", len(series.Episodes))
	}

	// Check Season 1 (converted from array-of-arrays)
	season1, ok := series.Episodes["1"]
	if !ok {
		t.Error("Expected season '1' in Episodes map")
	}
	if len(season1) != 2 {
		t.Errorf("Expected 2 episodes in season 1, got %d", len(season1))
	}
	if season1[0].ID != "101" {
		t.Errorf("Expected first episode ID '101', got '%s'", season1[0].ID)
	}
	if season1[0].Season != 1 {
		t.Errorf("Expected first episode Season 1, got %d", season1[0].Season)
	}

	// Check Season 2 (converted from array-of-arrays)
	season2, ok := series.Episodes["2"]
	if !ok {
		t.Error("Expected season '2' in Episodes map")
	}
	if len(season2) != 1 {
		t.Errorf("Expected 1 episode in season 2, got %d", len(season2))
	}
	if season2[0].ID != "201" {
		t.Errorf("Expected first episode ID '201', got '%s'", season2[0].ID)
	}
	if season2[0].Season != 2 {
		t.Errorf("Expected first episode Season 2, got %d", season2[0].Season)
	}
}

func TestSeriesUnmarshal_EmptyEpisodes(t *testing.T) {
	jsonData := `{
		"info": {
			"name": "Test Series",
			"cover": "http://example.com/cover.jpg"
		},
		"episodes": {},
		"seasons": []
	}`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal Series with empty episodes: %v", err)
	}

	// Should have empty Episodes map
	if series.Episodes == nil {
		t.Error("Expected Episodes to be initialized as empty map, got nil")
	}
	if len(series.Episodes) != 0 {
		t.Errorf("Expected empty Episodes map, got %d entries", len(series.Episodes))
	}

	// Should have Info
	if series.Info.Name != "Test Series" {
		t.Errorf("Expected Info.Name 'Test Series', got '%s'", series.Info.Name)
	}
}

func TestSeriesUnmarshal_NullEpisodes(t *testing.T) {
	jsonData := `{
		"info": {
			"name": "Test Series"
		},
		"episodes": null,
		"seasons": []
	}`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal Series with null episodes: %v", err)
	}

	// Should have empty Episodes map
	if series.Episodes == nil {
		t.Error("Expected Episodes to be initialized as empty map, got nil")
	}
	if len(series.Episodes) != 0 {
		t.Errorf("Expected empty Episodes map, got %d entries", len(series.Episodes))
	}
}

func TestSeriesUnmarshal_EmptyArrayEpisodes(t *testing.T) {
	jsonData := `{
		"info": {
			"name": "Test Series"
		},
		"episodes": [],
		"seasons": []
	}`

	var series Series
	err := json.Unmarshal([]byte(jsonData), &series)
	if err != nil {
		t.Fatalf("Failed to unmarshal Series with empty array episodes: %v", err)
	}

	// Should have empty Episodes map
	if series.Episodes == nil {
		t.Error("Expected Episodes to be initialized as empty map, got nil")
	}
	if len(series.Episodes) != 0 {
		t.Errorf("Expected empty Episodes map, got %d entries", len(series.Episodes))
	}
}
