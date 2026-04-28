package main

import (
	"context"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"github.com/bapley/project-nats-kv/internal/adapter"
	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
)

func main() {
	region := envOr("REGION", "local")
	listen := envOr("LISTEN_ADDR", ":8080")
	jsDir := envOr("JETSTREAM_DIR", "/var/lib/nats/jetstream")
	clientPort := envInt("NATS_CLIENT_PORT", 4222)
	clusterPort := envInt("NATS_CLUSTER_PORT", 6222)
	monitorPort := envInt("NATS_MONITOR_PORT", 8222)
	demoToken := envOr("DEMO_TOKEN", "akv_demo_open")
	natsRoutes := os.Getenv("NATS_ROUTES") // comma-separated nats-route:// URLs

	if err := os.MkdirAll(jsDir, 0o755); err != nil {
		log.Fatalf("jetstream dir: %v", err)
	}
	storeDir := filepath.Join(jsDir, region)

	opts := &natsserver.Options{
		ServerName: "kv-" + region,
		Host:       "0.0.0.0",
		Port:       clientPort,
		HTTPPort:   monitorPort,
		JetStream:  true,
		StoreDir:   storeDir,
		Tags:       []string{"region:" + region},
		NoSigs:     true,
	}
	if natsRoutes != "" {
		opts.Cluster = natsserver.ClusterOpts{
			Name: "nats-kv-mesh",
			Host: "0.0.0.0",
			Port: clusterPort,
		}
		opts.Routes = natsserver.RoutesFromStr(natsRoutes)
	}

	log.SetOutput(os.Stderr)
	log.Printf("BOOT: region=%s listen=%s js=%s", region, listen, storeDir)
	opts.Debug = true
	opts.Trace = false
	opts.Logtime = true
	ns, err := natsserver.NewServer(opts)
	if err != nil {
		log.Fatalf("nats server new: %v", err)
	}
	ns.ConfigureLogger()
	log.Printf("BOOT: nats configured, starting")
	go ns.Start()
	if !ns.ReadyForConnections(30 * time.Second) {
		log.Fatal("nats server not ready")
	}
	log.Printf("BOOT: nats ready, JS=%v", ns.JetStreamEnabled())
	if !ns.JetStreamEnabled() {
		log.Fatal("jetstream not enabled")
	}
	log.Printf("nats started region=%s client=%d cluster=%d monitor=%d store=%s", region, clientPort, clusterPort, monitorPort, storeDir)

	log.Printf("BOOT: connecting in-process")
	nc, err := nats.Connect("", nats.InProcessServer(ns), nats.Name("adapter-"+region))
	if err != nil {
		log.Fatalf("in-process nats connect: %v", err)
	}
	defer nc.Close()

	js, err := nc.JetStream()
	if err != nil {
		log.Fatalf("jetstream context: %v", err)
	}

	srv := adapter.New(adapter.Config{
		Region:    region,
		JS:        js,
		NC:        nc,
		DemoToken: demoToken,
	})

	httpSrv := &http.Server{
		Addr:              listen,
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Printf("http listening on %s", listen)
		if err := httpSrv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("http: %v", err)
		}
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)
	<-stop
	log.Print("shutting down")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = httpSrv.Shutdown(ctx)
	ns.Shutdown()
	ns.WaitForShutdown()
}

func envOr(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

func envInt(k string, d int) int {
	if v := os.Getenv(k); v != "" {
		if i, err := strconv.Atoi(v); err == nil {
			return i
		}
	}
	return d
}
