package main

import (
	"context"
	"database/sql"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/bapley/project-nats-kv/internal/control"
	"github.com/bapley/project-nats-kv/internal/placement"
	"github.com/bapley/project-nats-kv/internal/probe"
	_ "github.com/mattn/go-sqlite3"
	"github.com/nats-io/nats.go"
)

func main() {
	listen := envOr("LISTEN_ADDR", ":8088")
	listenTLS := envOr("LISTEN_TLS_ADDR", "")
	tlsCert := envOr("TLS_CERT_FILE", "")
	tlsKey := envOr("TLS_KEY_FILE", "")
	natsURL := envOr("NATS_URL", "nats://kv-adapter.demo-nats-kv.svc.cluster.local:4222")
	dbPath := envOr("DB_PATH", "/var/lib/nats-kv-control/control.db")
	adminToken := os.Getenv("ADMIN_TOKEN")
	pubBaseURL := envOr("PUBLIC_BASE_URL", "https://nats-kv-demo.connected-cloud.io")
	latencyHubURL := envOr("LATENCY_HUB_URL", "https://latency.connected-cloud.io")
	latencyAuthToken := os.Getenv("LATENCY_AUTH_TOKEN")

	if adminToken == "" {
		log.Fatal("ADMIN_TOKEN env var required")
	}

	if err := os.MkdirAll(filepath.Dir(dbPath), 0o755); err != nil {
		log.Fatalf("mkdir db: %v", err)
	}

	db, err := sql.Open("sqlite3", dbPath+"?_journal=WAL&_busy_timeout=5000")
	if err != nil {
		log.Fatalf("open sqlite: %v", err)
	}
	defer db.Close()

	nc, err := nats.Connect(natsURL,
		nats.Name("nats-kv-control"),
		nats.RetryOnFailedConnect(true),
		nats.MaxReconnects(-1),
		nats.ReconnectWait(2*time.Second),
	)
	if err != nil {
		log.Fatalf("nats connect: %v", err)
	}
	defer nc.Close()
	js, err := nc.JetStream()
	if err != nil {
		log.Fatalf("jetstream context: %v", err)
	}

	store, err := control.NewStore(js, db)
	if err != nil {
		log.Fatalf("store init: %v", err)
	}

	var placer *placement.Engine
	if latencyHubURL != "" {
		placer = placement.NewEngine(placement.NewClient(latencyHubURL, latencyAuthToken))
		log.Printf("placement engine: latency hub %s (auth=%v)", latencyHubURL, latencyAuthToken != "")
	} else {
		log.Print("placement engine: disabled (LATENCY_HUB_URL empty)")
	}

	srv := control.New(store, adminToken, pubBaseURL, nc, js, placer)

	// Bootstrap the topology-freshness probe bucket (R3 + mirrors-everywhere)
	// and start the periodic prober. The prober writes a timestamped payload
	// every 5s and direct-reads each region's mirror to compute end-to-end
	// replication delay — drives the topology page's "live freshness" panel.
	probeCtx, probeCancel := context.WithCancel(context.Background())
	defer probeCancel()
	if placer != nil {
		bootstrapCtx, bcancel := context.WithTimeout(probeCtx, 30*time.Second)
		if d, err := srv.EnsureSharedBucket(bootstrapCtx, probe.BucketName, "us-ord", 3, true,
			"topology freshness probe — auto-managed"); err != nil {
			log.Printf("probe bootstrap: %v (freshness page will 503 until this resolves)", err)
		} else if d != nil {
			log.Printf("probe bootstrap: created %s in %s", probe.BucketName, d.ChosenGeo)
		} else {
			log.Printf("probe bootstrap: %s already exists", probe.BucketName)
		}
		bcancel()
		prober := probe.New(js)
		go prober.Run(probeCtx)
		srv.SetProber(prober)
		log.Print("topology freshness probe started (5s interval)")
	} else {
		log.Print("topology freshness probe disabled (placement engine unavailable)")
	}

	httpSrv := &http.Server{
		Addr:              listen,
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Printf("nats-kv-control listening on %s (nats=%s db=%s)", listen, natsURL, dbPath)
		if err := httpSrv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("http: %v", err)
		}
	}()

	var tlsSrv *http.Server
	if listenTLS != "" && tlsCert != "" && tlsKey != "" {
		tlsSrv = &http.Server{
			Addr:              listenTLS,
			Handler:           srv.Handler(),
			ReadHeaderTimeout: 5 * time.Second,
		}
		go func() {
			log.Printf("https listening on %s (cert=%s)", listenTLS, tlsCert)
			if err := tlsSrv.ListenAndServeTLS(tlsCert, tlsKey); err != nil && err != http.ErrServerClosed {
				log.Fatalf("https: %v", err)
			}
		}()
	}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)
	<-stop
	log.Print("shutting down")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = httpSrv.Shutdown(ctx)
	if tlsSrv != nil {
		_ = tlsSrv.Shutdown(ctx)
	}
}

func envOr(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}
