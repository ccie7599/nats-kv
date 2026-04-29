package placement

import (
	"context"
	"fmt"
	"sort"
	"strings"
	"time"
)

// Decision is the placement engine's verdict + the inputs that drove it. The
// caller persists this alongside the bucket so the UI can render the "why."
type Decision struct {
	Mode            string      `json:"mode"`             // "auto" | "anchor" | "manual"
	Replicas        int         `json:"replicas"`
	Anchor          string      `json:"anchor"`           // region used as anchor
	AnchorSource    string      `json:"anchor_source"`    // "request" | "default"
	ChosenGeo       string      `json:"chosen_geo"`       // "na" | "eu" | "ap" | ...
	ChosenRegions  []string     `json:"chosen_regions"`
	PlacementTag    string      `json:"placement_tag"`    // value passed to JetStream Placement.Tags
	WriteLatencyMs  float64     `json:"write_latency_ms"` // expected: anchor → leader + leader → quorum
	QuorumEdgeMs    float64     `json:"quorum_edge_ms"`   // intra-set edge (leader → ceil(k/2)-th peer)
	Candidates      []Candidate `json:"candidates"`       // every geo we evaluated, ranked best→worst
	MatrixSampledAt time.Time   `json:"matrix_sampled_at"`
	GeneratedAt     time.Time   `json:"generated_at"`
	Notes           []string    `json:"notes,omitempty"`
}

// Candidate is one geo's score + the regions that would be used.
type Candidate struct {
	Geo            string   `json:"geo"`
	Regions        []string `json:"regions"`
	WriteLatencyMs float64  `json:"write_latency_ms"`
	QuorumEdgeMs   float64  `json:"quorum_edge_ms"`
	AnchorRTTMs    float64  `json:"anchor_rtt_ms"` // anchor → closest region in set
	Eligible       bool     `json:"eligible"`
	Reason         string   `json:"reason"`
}

// Engine picks RAFT placement for a bucket given a target replica count and an
// anchor region (typically the caller's nearest probe).
type Engine struct {
	matrix *Client
}

func NewEngine(matrix *Client) *Engine { return &Engine{matrix: matrix} }

// Pick returns a Decision for `replicas` (1, 3, or 5) anchored at `anchor`.
// `mode` is recorded in the Decision for UI display.
//
// For R1 we just pick the anchor's region — placement is trivial.
// For R3/R5 we evaluate each geo bucket; within a geo, sort regions by RTT
// from anchor and take the top-k. Score = anchor → leader + leader's quorum
// edge (ceil(k/2)-1 closest peer in the set), pick the leader within the set
// that minimizes that. Best geo wins.
func (e *Engine) Pick(ctx context.Context, replicas int, anchor, mode string) (*Decision, error) {
	if replicas != 1 && replicas != 3 && replicas != 5 {
		return nil, fmt.Errorf("placement: replicas must be 1, 3, or 5 (got %d)", replicas)
	}
	if anchor == "" {
		anchor = "us-ord"
	}

	m, err := e.matrix.Get(ctx)
	if err != nil {
		return nil, fmt.Errorf("placement matrix: %w", err)
	}

	d := &Decision{
		Mode:            mode,
		Replicas:        replicas,
		Anchor:          anchor,
		MatrixSampledAt: m.SampledAt,
		GeneratedAt:     time.Now(),
	}

	// R1: bucket lives on the anchor's region. Caller can override by passing
	// a different anchor; that's the "I want my bucket in eu-central" path.
	if replicas == 1 {
		d.ChosenGeo = GeoOf(anchor)
		d.ChosenRegions = []string{anchor}
		d.PlacementTag = "region:" + anchor
		if rtt, ok := m.RTT(anchor, anchor); ok {
			d.WriteLatencyMs = rtt
		}
		d.Candidates = []Candidate{{
			Geo: d.ChosenGeo, Regions: d.ChosenRegions,
			WriteLatencyMs: 0, QuorumEdgeMs: 0, AnchorRTTMs: 0,
			Eligible: true, Reason: "R1 places bucket on the anchor region itself",
		}}
		return d, nil
	}

	// R3/R5: evaluate each geo bucket.
	byGeo := RegionsByGeo()
	var cands []Candidate
	for geo, regions := range byGeo {
		c := scoreGeo(geo, regions, replicas, anchor, m)
		cands = append(cands, c)
	}
	sort.Slice(cands, func(i, j int) bool {
		// Eligible candidates first; within those, lower write latency wins.
		if cands[i].Eligible != cands[j].Eligible {
			return cands[i].Eligible
		}
		return cands[i].WriteLatencyMs < cands[j].WriteLatencyMs
	})
	d.Candidates = cands

	if len(cands) == 0 || !cands[0].Eligible {
		return nil, fmt.Errorf("placement: no geo has %d regions available", replicas)
	}

	winner := cands[0]
	d.ChosenGeo = winner.Geo
	d.ChosenRegions = winner.Regions
	d.PlacementTag = "geo:" + winner.Geo
	d.WriteLatencyMs = winner.WriteLatencyMs
	d.QuorumEdgeMs = winner.QuorumEdgeMs

	// Add a note about how runner-up compares — useful in the UI to justify
	// the choice ("we picked EU because NA was 80ms further").
	if len(cands) > 1 && cands[1].Eligible {
		d.Notes = append(d.Notes, fmt.Sprintf(
			"Runner-up: %s (write %.0fms, +%.0fms vs winner)",
			cands[1].Geo, cands[1].WriteLatencyMs, cands[1].WriteLatencyMs-winner.WriteLatencyMs,
		))
	}
	return d, nil
}

// scoreGeo returns the Candidate for placing a `replicas`-RAFT in `geo`.
// Picks the top-`replicas` regions by anchor proximity, then finds the leader
// (within that set) that minimizes anchor→leader + leader's quorum edge.
func scoreGeo(geo string, regions []string, replicas int, anchor string, m *Matrix) Candidate {
	c := Candidate{Geo: geo, Eligible: false}
	if len(regions) < replicas {
		c.Reason = fmt.Sprintf("only %d regions in %s, need %d", len(regions), geo, replicas)
		return c
	}

	// Sort regions in this geo by RTT from anchor; pick the top-k.
	sortedByAnchor := append([]string(nil), regions...)
	sort.Slice(sortedByAnchor, func(i, j int) bool {
		ri, _ := m.RTT(anchor, sortedByAnchor[i])
		rj, _ := m.RTT(anchor, sortedByAnchor[j])
		return ri < rj
	})
	chosen := sortedByAnchor[:replicas]
	c.Regions = chosen

	// Anchor RTT = anchor → closest member of the set.
	if rtt, ok := m.RTT(anchor, chosen[0]); ok {
		c.AnchorRTTMs = rtt
	}

	// Pick the leader within `chosen` that minimizes anchor→leader + leader's
	// quorum edge. Quorum edge = (ceil(k/2)-1)-th smallest RTT from leader to
	// other members, since leader counts toward quorum and we need ceil(k/2)
	// total acks.
	quorumPeerIdx := replicas/2 - 1
	if quorumPeerIdx < 0 {
		quorumPeerIdx = 0
	}
	bestWrite := -1.0
	bestEdge := -1.0
	for _, leader := range chosen {
		leaderRTT, _ := m.RTT(anchor, leader)
		var peerRTTs []float64
		for _, peer := range chosen {
			if peer == leader {
				continue
			}
			r, ok := m.RTT(leader, peer)
			if !ok {
				continue
			}
			peerRTTs = append(peerRTTs, r)
		}
		if len(peerRTTs) <= quorumPeerIdx {
			continue
		}
		sort.Float64s(peerRTTs)
		edge := peerRTTs[quorumPeerIdx]
		write := leaderRTT + edge
		if bestWrite < 0 || write < bestWrite {
			bestWrite = write
			bestEdge = edge
		}
	}
	if bestWrite < 0 {
		c.Reason = fmt.Sprintf("no RTT data covering %s", strings.Join(chosen, ","))
		return c
	}
	c.WriteLatencyMs = bestWrite
	c.QuorumEdgeMs = bestEdge
	c.Eligible = true
	c.Reason = fmt.Sprintf("anchor %s → nearest %s (%.0fms) + quorum edge %.0fms",
		anchor, chosen[0], c.AnchorRTTMs, bestEdge)
	return c
}
