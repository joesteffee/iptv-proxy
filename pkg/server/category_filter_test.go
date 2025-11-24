package server

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"testing"
)

func TestCategoryFilter_isCategoryDisabled(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: map[string]map[string]bool{
			"live": {
				"1": true,
				"2": false,
			},
			"movies": {
				"10": true,
			},
		},
	}

	tests := []struct {
		name       string
		catType    string
		categoryID string
		expected   bool
	}{
		{"disabled live category", "live", "1", true},
		{"enabled live category", "live", "2", false},
		{"non-existent live category", "live", "999", false},
		{"disabled movies category", "movies", "10", true},
		{"non-existent movies category", "movies", "999", false},
		{"non-existent type", "series", "1", false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := cf.isCategoryDisabled(tt.catType, tt.categoryID)
			if result != tt.expected {
				t.Errorf("isCategoryDisabled(%q, %q) = %v, expected %v", tt.catType, tt.categoryID, result, tt.expected)
			}
		})
	}
}

func TestCategoryFilter_setCategoryDisabled(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	// Test setting disabled
	cf.setCategoryDisabled("live", "1", true)
	if !cf.isCategoryDisabled("live", "1") {
		t.Error("Expected category to be disabled after setCategoryDisabled(true)")
	}

	// Test setting enabled
	cf.setCategoryDisabled("live", "1", false)
	if cf.isCategoryDisabled("live", "1") {
		t.Error("Expected category to be enabled after setCategoryDisabled(false)")
	}

	// Test creating new type
	cf.setCategoryDisabled("series", "5", true)
	if !cf.isCategoryDisabled("series", "5") {
		t.Error("Expected new type to be created and category disabled")
	}
}

func TestCategoryFilter_setDisabledCategories(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	newDisabled := map[string]map[string]bool{
		"live": {
			"1": true,
			"2": true,
		},
		"movies": {
			"10": true,
		},
		"series": {
			"20": true,
		},
	}

	cf.setDisabledCategories(newDisabled)

	// Verify all categories are set correctly
	if !cf.isCategoryDisabled("live", "1") {
		t.Error("Expected live category 1 to be disabled")
	}
	if !cf.isCategoryDisabled("live", "2") {
		t.Error("Expected live category 2 to be disabled")
	}
	if !cf.isCategoryDisabled("movies", "10") {
		t.Error("Expected movies category 10 to be disabled")
	}
	if !cf.isCategoryDisabled("series", "20") {
		t.Error("Expected series category 20 to be disabled")
	}
}

func TestCategoryFilter_getDisabledCategories(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: map[string]map[string]bool{
			"live": {
				"1": true,
				"2": false,
			},
			"movies": {
				"10": true,
			},
		},
	}

	result := cf.getDisabledCategories()

	// Verify it's a copy (not the same reference)
	if reflect.ValueOf(result).Pointer() == reflect.ValueOf(cf.disabledCats).Pointer() {
		t.Error("getDisabledCategories should return a copy, not the original map")
	}

	// Verify content
	if !result["live"]["1"] {
		t.Error("Expected live category 1 to be disabled in result")
	}
	if result["live"]["2"] {
		t.Error("Expected live category 2 to be enabled in result")
	}
	if !result["movies"]["10"] {
		t.Error("Expected movies category 10 to be disabled in result")
	}
}

func TestCategoryFilter_saveToFile(t *testing.T) {
	// Create temporary directory
	tmpDir := t.TempDir()
	filePath := filepath.Join(tmpDir, "category_filters.json")

	cf := &categoryFilter{
		disabledCats: map[string]map[string]bool{
			"live": {
				"1": true,
				"2": true,
			},
			"movies": {
				"10": true,
			},
		},
	}

	err := cf.saveToFile(filePath)
	if err != nil {
		t.Fatalf("saveToFile failed: %v", err)
	}

	// Verify file exists
	if _, err := os.Stat(filePath); os.IsNotExist(err) {
		t.Fatal("Expected file to be created")
	}

	// Verify file content
	data, err := os.ReadFile(filePath)
	if err != nil {
		t.Fatalf("Failed to read file: %v", err)
	}

	var loaded map[string]map[string]bool
	if err := json.Unmarshal(data, &loaded); err != nil {
		t.Fatalf("Failed to parse JSON: %v", err)
	}

	if !loaded["live"]["1"] {
		t.Error("Expected live category 1 to be disabled in saved file")
	}
	if !loaded["live"]["2"] {
		t.Error("Expected live category 2 to be disabled in saved file")
	}
	if !loaded["movies"]["10"] {
		t.Error("Expected movies category 10 to be disabled in saved file")
	}
}

func TestCategoryFilter_saveToFile_emptyPath(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: map[string]map[string]bool{
			"live": {"1": true},
		},
	}

	// Should not error with empty path
	err := cf.saveToFile("")
	if err != nil {
		t.Errorf("saveToFile with empty path should not error, got: %v", err)
	}
}

func TestCategoryFilter_loadFromFile(t *testing.T) {
	// Create temporary directory
	tmpDir := t.TempDir()
	filePath := filepath.Join(tmpDir, "category_filters.json")

	// Create test file
	testData := map[string]map[string]bool{
		"live": {
			"1": true,
			"2": true,
		},
		"movies": {
			"10": true,
		},
	}

	data, err := json.MarshalIndent(testData, "", "  ")
	if err != nil {
		t.Fatalf("Failed to marshal test data: %v", err)
	}

	if err := os.WriteFile(filePath, data, 0644); err != nil {
		t.Fatalf("Failed to write test file: %v", err)
	}

	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	err = cf.loadFromFile(filePath)
	if err != nil {
		t.Fatalf("loadFromFile failed: %v", err)
	}

	// Verify loaded data
	if !cf.isCategoryDisabled("live", "1") {
		t.Error("Expected live category 1 to be disabled after load")
	}
	if !cf.isCategoryDisabled("live", "2") {
		t.Error("Expected live category 2 to be disabled after load")
	}
	if !cf.isCategoryDisabled("movies", "10") {
		t.Error("Expected movies category 10 to be disabled after load")
	}
}

func TestCategoryFilter_loadFromFile_notExists(t *testing.T) {
	// Create temporary directory
	tmpDir := t.TempDir()
	filePath := filepath.Join(tmpDir, "non_existent.json")

	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	// Should not error if file doesn't exist
	err := cf.loadFromFile(filePath)
	if err != nil {
		t.Errorf("loadFromFile with non-existent file should not error, got: %v", err)
	}

	// Should have empty disabled categories
	result := cf.getDisabledCategories()
	if len(result) != 0 {
		t.Errorf("Expected empty disabled categories after loading non-existent file, got: %v", result)
	}
}

func TestCategoryFilter_loadFromFile_emptyPath(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	// Should not error with empty path
	err := cf.loadFromFile("")
	if err != nil {
		t.Errorf("loadFromFile with empty path should not error, got: %v", err)
	}
}

func TestCategoryFilter_concurrentAccess(t *testing.T) {
	cf := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	var wg sync.WaitGroup
	numGoroutines := 10
	numOps := 100

	// Test concurrent reads and writes
	for i := 0; i < numGoroutines; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			for j := 0; j < numOps; j++ {
				catType := "live"
				catID := string(rune(id)) + string(rune(j))
				
				// Write
				cf.setCategoryDisabled(catType, catID, true)
				
				// Read
				cf.isCategoryDisabled(catType, catID)
				
				// Get all
				cf.getDisabledCategories()
			}
		}(i)
	}

	wg.Wait()
	// If we get here without a race condition, the test passes
}

func TestCategoryFilter_saveAndLoadRoundTrip(t *testing.T) {
	// Create temporary directory
	tmpDir := t.TempDir()
	filePath := filepath.Join(tmpDir, "category_filters.json")

	original := &categoryFilter{
		disabledCats: map[string]map[string]bool{
			"live": {
				"1": true,
				"2": true,
				"3": false,
			},
			"movies": {
				"10": true,
				"11": true,
			},
			"series": {
				"20": true,
			},
		},
	}

	// Save
	if err := original.saveToFile(filePath); err != nil {
		t.Fatalf("Failed to save: %v", err)
	}

	// Load into new filter
	loaded := &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}

	if err := loaded.loadFromFile(filePath); err != nil {
		t.Fatalf("Failed to load: %v", err)
	}

	// Compare
	originalData := original.getDisabledCategories()
	loadedData := loaded.getDisabledCategories()

	if !reflect.DeepEqual(originalData, loadedData) {
		t.Errorf("Loaded data doesn't match original:\nOriginal: %v\nLoaded: %v", originalData, loadedData)
	}
}

func TestGlobalCategoryFilter(t *testing.T) {
	// Reset global filter for test
	originalFilter := globalCategoryFilter
	globalCategoryFilter = &categoryFilter{
		disabledCats: make(map[string]map[string]bool),
	}
	defer func() {
		globalCategoryFilter = originalFilter
	}()

	// Test global filter operations
	globalCategoryFilter.setCategoryDisabled("live", "1", true)
	if !globalCategoryFilter.isCategoryDisabled("live", "1") {
		t.Error("Expected global filter to work correctly")
	}

	// Test concurrent access to global filter
	var wg sync.WaitGroup
	for i := 0; i < 5; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			globalCategoryFilter.setCategoryDisabled("live", string(rune(id)), true)
			globalCategoryFilter.isCategoryDisabled("live", string(rune(id)))
		}(i)
	}
	wg.Wait()
}

