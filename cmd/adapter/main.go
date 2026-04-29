package main

import (
	"context"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/bapley/project-nats-kv/internal/adapter"
	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
)

func main() {
	region := envOr("REGION", "local")
	listen := envOr("LISTEN_ADDR", ":8080")
	listenTLS := envOr("LISTEN_TLS_ADDR", "") // e.g. ":8443" — disabled if empty
	tlsCert := envOr("TLS_CERT_FILE", "")
	tlsKey := envOr("TLS_KEY_FILE", "")
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
		Tags:       []string{"region:" + region, "geo:" + geoOfRegion(region)},
		NoSigs:     true,
		Cluster: natsserver.ClusterOpts{
			Name:      "nats-kv-mesh",
			Host:      "0.0.0.0",
			Port:      clusterPort,
			Advertise: envOr("CLUSTER_ADVERTISE", ""), // empty = NATS picks default; set on LZ to NB IP so leaves can dial back
		},
	}
	if natsRoutes != "" {
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
		Region:     region,
		JS:         js,
		NC:         nc,
		DemoToken:  demoToken,
		ControlURL: envOr("CONTROL_URL", "https://cp.nats-kv.connected-cloud.io"),
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

// geoOfRegion mirrors internal/adapter/server.go geoOf — kept here to avoid
// importing internal package from main. Six geos: na, eu, ap, sa, oc, af.
func geoOfRegion(region string) string {
	switch {
	case strings.HasPrefix(region, "us-"), strings.HasPrefix(region, "ca-"):
		return "na"
	case strings.HasPrefix(region, "eu-"), strings.HasPrefix(region, "de-"), strings.HasPrefix(region, "fr-"),
		strings.HasPrefix(region, "gb-"), strings.HasPrefix(region, "it-"), strings.HasPrefix(region, "nl-"),
		strings.HasPrefix(region, "se-"), strings.HasPrefix(region, "es-"), strings.HasPrefix(region, "no-"):
		return "eu"
	case strings.HasPrefix(region, "ap-"), strings.HasPrefix(region, "jp-"), strings.HasPrefix(region, "id-"),
		strings.HasPrefix(region, "in-"), strings.HasPrefix(region, "sg-"), strings.HasPrefix(region, "my-"):
		return "ap"
	case strings.HasPrefix(region, "br-"), strings.HasPrefix(region, "co-"), strings.HasPrefix(region, "cl-"):
		return "sa"
	case strings.HasPrefix(region, "au-"), strings.HasPrefix(region, "nz-"):
		return "oc"
	case strings.HasPrefix(region, "za-"):
		return "af"
	default:
		return "unknown"
	}
}
