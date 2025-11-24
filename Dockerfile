FROM golang:1.17-alpine

RUN apk add ca-certificates

WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 GOOS=linux go build -a -installsuffix cgo -o iptv-proxy .

FROM alpine:3
RUN apk add ca-certificates
COPY --from=0  /app/iptv-proxy /
ENTRYPOINT ["/iptv-proxy"]
