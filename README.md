# IPTV Proxy

A reverse proxy for IPTV M3U playlists and Xtream Codes server API. This Go application allows you to proxy and transform IPTV streams and API endpoints.

## Features

- Proxy M3U playlist files
- Proxy Xtream Codes server API endpoints
- Transform and customize stream URLs
- Dockerized for easy deployment

## Requirements

- Go 1.17+
- Docker (for containerized deployment)

## Usage

### Docker

1. Build the Docker image:
   ```bash
   docker build -t iptv-proxy:latest .
   ```

2. Run the container:
   ```bash
   docker run -d \
     -p 8080:8080 \
     -e M3U_URL="https://example.com/playlist.m3u" \
     iptv-proxy:latest
   ```

### Local Go

1. Build the application:
   ```bash
   go build -o iptv-proxy .
   ```

2. Run the application:
   ```bash
   ./iptv-proxy --m3u-url="https://example.com/playlist.m3u"
   ```

## Configuration

The application can be configured via command-line flags or environment variables. See `--help` for available options.
