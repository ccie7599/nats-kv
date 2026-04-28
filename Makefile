.PHONY: build build-linux test docker spin-build spin-deploy clean

ADAPTER_BIN = bin/kv-adapter

build:
	go build -o $(ADAPTER_BIN) ./cmd/adapter

build-linux:
	CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -ldflags="-s -w" -o $(ADAPTER_BIN)-linux ./cmd/adapter

test:
	go test ./...

docker:
	docker build -f deploy/docker/Dockerfile -t ghcr.io/ccie7599/nats-kv-adapter:dev .

dev-adapter:
	REGION=local LISTEN_ADDR=:8080 NATS_CLIENT_PORT=14222 NATS_CLUSTER_PORT=16222 \
		NATS_MONITOR_PORT=18222 JETSTREAM_DIR=/tmp/jstest go run ./cmd/adapter

spin-build:
	cd ui/nats-kv-user && spin build

spin-deploy:
	cd ui/nats-kv-user && spin aka deploy --no-confirm

release-binary:
	$(MAKE) build-linux
	gh release upload v0.1.0 $(ADAPTER_BIN)-linux --repo ccie7599/nats-kv --clobber

clean:
	rm -rf bin/ ui/nats-kv-user/target/
