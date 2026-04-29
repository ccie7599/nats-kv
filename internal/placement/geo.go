package placement

import "strings"

// AllRegions is the list of 27 KV cluster peers. Kept in code so the placement
// engine can enumerate candidate sets without round-tripping JetStream meta.
// Update when regions are added/removed (also update internal/control/handlers.go
// allRegions list — same content, different package).
var AllRegions = []string{
	"us-ord", "us-east", "us-central", "us-west", "us-southeast",
	"us-lax", "us-mia", "us-sea", "ca-central", "br-gru",
	"gb-lon", "eu-central", "de-fra-2", "fr-par-2", "nl-ams",
	"se-sto", "it-mil",
	"ap-south", "sg-sin-2", "ap-northeast", "jp-tyo-3", "jp-osa",
	"ap-west", "in-bom-2", "in-maa", "id-cgk", "ap-southeast",
}

// GeoOf returns the geo bucket (na/eu/ap/sa/oc/af) for a region short-name.
// Mirrors cmd/adapter/main.go geoOfRegion — kept duplicated to avoid an
// import-cycle from control → adapter.
func GeoOf(region string) string {
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

// RegionsByGeo groups AllRegions by geo bucket.
func RegionsByGeo() map[string][]string {
	out := map[string][]string{}
	for _, r := range AllRegions {
		g := GeoOf(r)
		out[g] = append(out[g], r)
	}
	return out
}
