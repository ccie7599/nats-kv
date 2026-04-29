package placement

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"sync"
	"time"
)

// Matrix is a snapshot of inter-region RTT values in milliseconds, keyed
// from -> to. Symmetric in practice but we keep both directions because the
// upstream feed publishes both.
type Matrix struct {
	Data        map[string]map[string]float64 `json:"data"`
	SampledAt   time.Time                     `json:"sampled_at"`
	FetchedAt   time.Time                     `json:"fetched_at"`
	SourceURL   string                        `json:"source_url"`
}

// RTT returns the from->to RTT in ms. ok=false if missing.
func (m *Matrix) RTT(from, to string) (float64, bool) {
	if m == nil || m.Data == nil {
		return 0, false
	}
	if from == to {
		return 0, true
	}
	row, ok := m.Data[from]
	if !ok {
		return 0, false
	}
	v, ok := row[to]
	return v, ok
}

// Client fetches the matrix from project-latency's hub and caches it. Callers
// should treat Get as cheap — at most one HTTP round trip per cacheTTL.
type Client struct {
	BaseURL  string
	AuthToken string
	HTTP     *http.Client
	TTL      time.Duration

	mu         sync.RWMutex
	cached     *Matrix
	lastError  error
}

func NewClient(baseURL, authToken string) *Client {
	return &Client{
		BaseURL:   baseURL,
		AuthToken: authToken,
		HTTP:      &http.Client{Timeout: 5 * time.Second},
		TTL:       60 * time.Second,
	}
}

// Get returns the latest matrix, refreshing in the background if the cache is
// stale. Returns the cached copy + a bool indicating freshness.
func (c *Client) Get(ctx context.Context) (*Matrix, error) {
	c.mu.RLock()
	cached := c.cached
	c.mu.RUnlock()
	if cached != nil && time.Since(cached.FetchedAt) < c.TTL {
		return cached, nil
	}
	return c.refresh(ctx)
}

func (c *Client) refresh(ctx context.Context) (*Matrix, error) {
	u, err := url.Parse(c.BaseURL + "/api/v1/matrix")
	if err != nil {
		return nil, fmt.Errorf("parse url: %w", err)
	}
	if c.AuthToken != "" {
		q := u.Query()
		q.Set("auth", c.AuthToken)
		u.RawQuery = q.Encode()
	}

	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
	resp, err := c.HTTP.Do(req)
	if err != nil {
		c.mu.Lock()
		c.lastError = err
		cached := c.cached
		c.mu.Unlock()
		if cached != nil {
			return cached, nil // serve stale rather than fail
		}
		return nil, fmt.Errorf("matrix fetch: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("matrix http %d", resp.StatusCode)
	}

	var raw struct {
		Results []struct {
			Source    string    `json:"source"`
			Target    string    `json:"target"`
			RTTMs     float64   `json:"rtt_ms"`
			Timestamp time.Time `json:"timestamp"`
		} `json:"results"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&raw); err != nil {
		return nil, fmt.Errorf("matrix decode: %w", err)
	}

	m := &Matrix{
		Data:      map[string]map[string]float64{},
		FetchedAt: time.Now(),
		SourceURL: c.BaseURL,
	}
	var maxTs time.Time
	for _, r := range raw.Results {
		if _, ok := m.Data[r.Source]; !ok {
			m.Data[r.Source] = map[string]float64{}
		}
		m.Data[r.Source][r.Target] = r.RTTMs
		if r.Timestamp.After(maxTs) {
			maxTs = r.Timestamp
		}
	}
	m.SampledAt = maxTs

	c.mu.Lock()
	c.cached = m
	c.lastError = nil
	c.mu.Unlock()
	return m, nil
}

// LastError returns the most recent fetch error, if any (for /v1/health).
func (c *Client) LastError() error {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.lastError
}
