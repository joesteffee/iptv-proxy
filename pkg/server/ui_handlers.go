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
	"fmt"
	"log"
	"net/http"
	"reflect"
	"time"

	"github.com/gin-gonic/gin"
	xtreamapi "github.com/pierre-emmanuelJ/iptv-proxy/pkg/xtream-proxy"
)

// CategoryInfo represents a category with its enabled/disabled status
type CategoryInfo struct {
	ID      string `json:"id"`
	Name    string `json:"name"`
	Type    string `json:"type"`
	Enabled bool   `json:"enabled"`
}

// CategoriesResponse represents the response for getting categories
type CategoriesResponse struct {
	Live   []CategoryInfo `json:"live"`
	Movies []CategoryInfo `json:"movies"`
	Series []CategoryInfo `json:"series"`
}

// UpdateCategoriesRequest represents the request to update enabled categories
type UpdateCategoriesRequest struct {
	Enabled map[string]map[string]bool `json:"enabled"` // type -> categoryID -> enabled
}

// getCategoriesHandler fetches all categories from xtream and returns them with disabled status
func (c *Config) getCategoriesHandler(ctx *gin.Context) {
	if c.XtreamBaseURL == "" {
		ctx.JSON(http.StatusBadRequest, gin.H{"error": "Xtream service not configured"})
		return
	}

	userAgent := "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
	
	client, err := xtreamapi.New(c.XtreamUser.String(), c.XtreamPassword.String(), c.XtreamBaseURL, userAgent)
	if err != nil {
		log.Printf("[iptv-proxy] ERROR: Failed to create Xtream client: %v\n", err)
		ctx.JSON(http.StatusInternalServerError, gin.H{"error": fmt.Sprintf("Failed to connect to Xtream: %v", err)})
		return
	}

	enabledCats := globalCategoryFilter.getEnabledCategories()
	response := CategoriesResponse{
		Live:   []CategoryInfo{},
		Movies: []CategoryInfo{},
		Series: []CategoryInfo{},
	}

	// Fetch Live categories
	liveCats, err := client.GetLiveCategories()
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to get live categories: %v\n", err)
	} else {
		liveCatsValue := reflect.ValueOf(liveCats)
		if liveCatsValue.Kind() == reflect.Slice {
			for i := 0; i < liveCatsValue.Len(); i++ {
				catElem := liveCatsValue.Index(i)
				catID := catElem.FieldByName("ID")
				catName := catElem.FieldByName("Name")
				
				catIDStr := ""
				if catID.IsValid() {
					catIDStr = fmt.Sprint(catID.Interface())
				}
				catNameStr := ""
				if catName.IsValid() {
					catNameStr = catName.String()
				}
				
				enabled := false
				if typeMap, ok := enabledCats["live"]; ok {
					enabled = typeMap[catIDStr]
				}
				
				response.Live = append(response.Live, CategoryInfo{
					ID:      catIDStr,
					Name:    catNameStr,
					Type:    "live",
					Enabled: enabled,
				})
			}
		}
	}

	// Fetch Movies (VOD) categories
	vodCats, err := client.GetVideoOnDemandCategories()
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to get VOD categories: %v\n", err)
	} else {
		vodCatsValue := reflect.ValueOf(vodCats)
		if vodCatsValue.Kind() == reflect.Slice {
			for i := 0; i < vodCatsValue.Len(); i++ {
				catElem := vodCatsValue.Index(i)
				catID := catElem.FieldByName("ID")
				catName := catElem.FieldByName("Name")
				
				catIDStr := ""
				if catID.IsValid() {
					catIDStr = fmt.Sprint(catID.Interface())
				}
				catNameStr := ""
				if catName.IsValid() {
					catNameStr = catName.String()
				}
				
				enabled := false
				if typeMap, ok := enabledCats["movies"]; ok {
					enabled = typeMap[catIDStr]
				}
				
				response.Movies = append(response.Movies, CategoryInfo{
					ID:      catIDStr,
					Name:    catNameStr,
					Type:    "movies",
					Enabled: enabled,
				})
			}
		}
	}

	// Fetch Series categories
	seriesCats, err := client.GetSeriesCategories()
	if err != nil {
		log.Printf("[iptv-proxy] WARNING: Failed to get series categories: %v\n", err)
	} else {
		seriesCatsValue := reflect.ValueOf(seriesCats)
		if seriesCatsValue.Kind() == reflect.Slice {
			for i := 0; i < seriesCatsValue.Len(); i++ {
				catElem := seriesCatsValue.Index(i)
				catID := catElem.FieldByName("ID")
				catName := catElem.FieldByName("Name")
				
				catIDStr := ""
				if catID.IsValid() {
					catIDStr = fmt.Sprint(catID.Interface())
				}
				catNameStr := ""
				if catName.IsValid() {
					catNameStr = catName.String()
				}
				
				enabled := false
				if typeMap, ok := enabledCats["series"]; ok {
					enabled = typeMap[catIDStr]
				}
				
				response.Series = append(response.Series, CategoryInfo{
					ID:      catIDStr,
					Name:    catNameStr,
					Type:    "series",
					Enabled: enabled,
				})
			}
		}
	}

	ctx.JSON(http.StatusOK, response)
}

// updateCategoriesHandler updates the disabled categories
func (c *Config) updateCategoriesHandler(ctx *gin.Context) {
	var req UpdateCategoriesRequest
	if err := ctx.ShouldBindJSON(&req); err != nil {
		ctx.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("Invalid request: %v", err)})
		return
	}

	globalCategoryFilter.setEnabledCategories(req.Enabled)
	
	// Save to file if path is configured
	if c.CategoryFiltersPath != "" {
		if err := globalCategoryFilter.saveToFile(c.CategoryFiltersPath); err != nil {
			log.Printf("[iptv-proxy] ERROR: Failed to save category filters: %v\n", err)
			ctx.JSON(http.StatusInternalServerError, gin.H{"error": fmt.Sprintf("Failed to save filters: %v", err)})
			return
		}
	}
	
	// Clear M3U cache to force regeneration with new filters
	xtreamM3uCacheLock.Lock()
	xtreamM3uCache = make(map[string]cacheMeta)
	xtreamM3uCacheLock.Unlock()
	
	log.Printf("[iptv-proxy] %v | Category filters updated\n", time.Now().Format("2006/01/02 - 15:04:05"))
	ctx.JSON(http.StatusOK, gin.H{"message": "Categories updated successfully"})
}

// serveWebUI serves the web UI HTML page
func (c *Config) serveWebUI(ctx *gin.Context) {
	html := `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>IPTV Proxy - Category Manager</title>
    <style>
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            padding: 20px;
        }
        .container {
            max-width: 1200px;
            margin: 0 auto;
            background: white;
            border-radius: 12px;
            box-shadow: 0 20px 60px rgba(0,0,0,0.3);
            overflow: hidden;
        }
        .header {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 30px;
            text-align: center;
        }
        .header h1 {
            font-size: 2em;
            margin-bottom: 10px;
        }
        .header p {
            opacity: 0.9;
        }
        .content {
            padding: 30px;
        }
        .section {
            margin-bottom: 40px;
        }
        .section-title {
            font-size: 1.5em;
            color: #333;
            margin-bottom: 20px;
            padding-bottom: 10px;
            border-bottom: 2px solid #667eea;
        }
        .category-grid {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
            gap: 15px;
        }
        .category-item {
            display: flex;
            align-items: center;
            padding: 12px;
            background: #f8f9fa;
            border-radius: 8px;
            border: 2px solid #e9ecef;
            transition: all 0.2s;
        }
        .category-item:hover {
            border-color: #667eea;
            background: #f0f4ff;
        }
        .category-item.disabled {
            opacity: 0.6;
            background: #e9ecef;
        }
        .category-item input[type="checkbox"] {
            width: 20px;
            height: 20px;
            margin-right: 12px;
            cursor: pointer;
        }
        .category-item label {
            flex: 1;
            cursor: pointer;
            font-size: 0.95em;
            color: #333;
        }
        .buttons {
            display: flex;
            gap: 15px;
            justify-content: center;
            margin-top: 30px;
        }
        button {
            padding: 12px 30px;
            font-size: 1em;
            border: none;
            border-radius: 8px;
            cursor: pointer;
            transition: all 0.2s;
            font-weight: 600;
        }
        .btn-primary {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
        }
        .btn-primary:hover {
            transform: translateY(-2px);
            box-shadow: 0 5px 15px rgba(102, 126, 234, 0.4);
        }
        .btn-secondary {
            background: #6c757d;
            color: white;
        }
        .btn-secondary:hover {
            background: #5a6268;
        }
        .loading {
            text-align: center;
            padding: 40px;
            color: #666;
        }
        .error {
            background: #f8d7da;
            color: #721c24;
            padding: 15px;
            border-radius: 8px;
            margin-bottom: 20px;
        }
        .success {
            background: #d4edda;
            color: #155724;
            padding: 15px;
            border-radius: 8px;
            margin-bottom: 20px;
            display: none;
        }
        .stats {
            display: flex;
            gap: 20px;
            margin-bottom: 30px;
            flex-wrap: wrap;
        }
        .stat-card {
            flex: 1;
            min-width: 150px;
            padding: 20px;
            background: #f8f9fa;
            border-radius: 8px;
            text-align: center;
        }
        .stat-card h3 {
            font-size: 2em;
            color: #667eea;
            margin-bottom: 5px;
        }
        .stat-card p {
            color: #666;
            font-size: 0.9em;
        }
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>📺 IPTV Proxy</h1>
            <p>Manage Categories - Check boxes to enable desired categories</p>
        </div>
        <div class="content">
            <div class="error" id="error" style="display: none;"></div>
            <div class="success" id="success">Categories updated successfully!</div>
            
            <div class="stats">
                <div class="stat-card">
                    <h3 id="live-count">0</h3>
                    <p>Live TV Categories</p>
                </div>
                <div class="stat-card">
                    <h3 id="movies-count">0</h3>
                    <p>Movie Categories</p>
                </div>
                <div class="stat-card">
                    <h3 id="series-count">0</h3>
                    <p>Series Categories</p>
                </div>
            </div>

            <div class="loading" id="loading">Loading categories...</div>
            
            <div id="categories" style="display: none;">
                <div class="section">
                    <h2 class="section-title">📡 Live TV</h2>
                    <div class="category-grid" id="live-categories"></div>
                </div>
                
                <div class="section">
                    <h2 class="section-title">🎬 Movies</h2>
                    <div class="category-grid" id="movies-categories"></div>
                </div>
                
                <div class="section">
                    <h2 class="section-title">📺 Series</h2>
                    <div class="category-grid" id="series-categories"></div>
                </div>
                
                <div class="buttons">
                    <button class="btn-primary" onclick="saveCategories()">Save Changes</button>
                    <button class="btn-secondary" onclick="loadCategories()">Refresh</button>
                </div>
            </div>
        </div>
    </div>

    <script>
        let categoriesData = { live: [], movies: [], series: [] };
        
        function showError(message) {
            const errorDiv = document.getElementById('error');
            errorDiv.textContent = message;
            errorDiv.style.display = 'block';
            setTimeout(() => {
                errorDiv.style.display = 'none';
            }, 5000);
        }
        
        function showSuccess() {
            const successDiv = document.getElementById('success');
            successDiv.style.display = 'block';
            setTimeout(() => {
                successDiv.style.display = 'none';
            }, 3000);
        }
        
        function renderCategories() {
            const liveContainer = document.getElementById('live-categories');
            const moviesContainer = document.getElementById('movies-categories');
            const seriesContainer = document.getElementById('series-categories');
            
            liveContainer.innerHTML = '';
            moviesContainer.innerHTML = '';
            seriesContainer.innerHTML = '';
            
            document.getElementById('live-count').textContent = categoriesData.live.length;
            document.getElementById('movies-count').textContent = categoriesData.movies.length;
            document.getElementById('series-count').textContent = categoriesData.series.length;
            
            function renderCategoryList(container, categories) {
                categories.forEach(cat => {
                    const div = document.createElement('div');
                    div.className = 'category-item' + (!cat.enabled ? ' disabled' : '');
                    const checked = cat.enabled ? 'checked' : '';
                    const catId = escapeHtml(cat.id);
                    const catType = escapeHtml(cat.type);
                    const catName = escapeHtml(cat.name);
                    div.innerHTML = 
                        '<input type="checkbox" id="cat-' + catType + '-' + catId + '" ' +
                        checked + ' ' +
                        'onchange="toggleCategory(\'' + catType + '\', \'' + catId + '\', this.checked)">' +
                        '<label for="cat-' + catType + '-' + catId + '">' + catName + '</label>';
                    container.appendChild(div);
                });
            }
            
            renderCategoryList(liveContainer, categoriesData.live);
            renderCategoryList(moviesContainer, categoriesData.movies);
            renderCategoryList(seriesContainer, categoriesData.series);
        }
        
        function escapeHtml(text) {
            const div = document.createElement('div');
            div.textContent = text;
            return div.innerHTML;
        }
        
        function toggleCategory(type, id, enabled) {
            const category = categoriesData[type].find(c => c.id === id);
            if (category) {
                category.enabled = enabled;
                const item = document.getElementById('cat-' + type + '-' + id).closest('.category-item');
                if (!enabled) {
                    item.classList.add('disabled');
                } else {
                    item.classList.remove('disabled');
                }
            }
        }
        
        async function loadCategories() {
            document.getElementById('loading').style.display = 'block';
            document.getElementById('categories').style.display = 'none';
            
            try {
                const response = await fetch('/api/categories');
                if (!response.ok) {
                    throw new Error('Failed to load categories: ' + response.statusText);
                }
                const data = await response.json();
                categoriesData = {
                    live: data.live || [],
                    movies: data.movies || [],
                    series: data.series || []
                };
                renderCategories();
                document.getElementById('loading').style.display = 'none';
                document.getElementById('categories').style.display = 'block';
            } catch (error) {
                document.getElementById('loading').style.display = 'none';
                showError('Error loading categories: ' + error.message);
            }
        }
        
        async function saveCategories() {
            const enabled = {
                live: {},
                movies: {},
                series: {}
            };
            
            categoriesData.live.forEach(cat => {
                if (cat.enabled) {
                    enabled.live[cat.id] = true;
                }
            });
            categoriesData.movies.forEach(cat => {
                if (cat.enabled) {
                    enabled.movies[cat.id] = true;
                }
            });
            categoriesData.series.forEach(cat => {
                if (cat.enabled) {
                    enabled.series[cat.id] = true;
                }
            });
            
            try {
                const response = await fetch('/api/categories', {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/json'
                    },
                    body: JSON.stringify({ enabled: enabled })
                });
                
                if (!response.ok) {
                    throw new Error('Failed to save categories: ' + response.statusText);
                }
                
                showSuccess();
            } catch (error) {
                showError('Error saving categories: ' + error.message);
            }
        }
        
        // Load categories on page load
        loadCategories();
    </script>
</body>
</html>`
	
	ctx.Header("Content-Type", "text/html; charset=utf-8")
	ctx.String(http.StatusOK, html)
}

