FROM golang:1.25-alpine AS builder
WORKDIR /build
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -ldflags="-s -w" -o /kv-adapter ./cmd/adapter

FROM alpine:3.20
RUN apk add --no-cache ca-certificates curl
COPY --from=builder /kv-adapter /usr/local/bin/kv-adapter
EXPOSE 4222 6222 8080 8222
ENTRYPOINT ["/usr/local/bin/kv-adapter"]
