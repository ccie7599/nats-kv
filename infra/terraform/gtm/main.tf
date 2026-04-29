# GTM for nats-kv data plane. Adds a `nats-kv` property + 27 dedicated
# `kv-<region>` datacenters into the existing connectedcloud5.akadns.net domain.
# Cannot create a new GTM domain on this contract (Performance Plus blocks
# config-gtm API creation, see ADR-015) — domain stays shared with sibling demos
# (mortgage-inference, etc.). DS2-feed isolation is therefore deferred until a
# dedicated domain can be provisioned out-of-band.
#
# Datacenter nicknames are kv-prefixed so we never collide with sibling
# properties' DCs in the same domain.

locals {
  gtm_domain = "connectedcloud5.akadns.net"

  # 27 datacenters: 1 LZ (us-ord) + 26 leaves. Nicknames kv-prefixed.
  # city/country/continent/lat/lon mirror project-latency's regions.go.
  regions = {
    "us-ord"       = { nick = "kv-us-ord",       city = "Chicago",      state = "IL", country = "US", continent = "NA", lat =  41.8781, lon =  -87.6298 }
    "us-east"      = { nick = "kv-us-east",      city = "Newark",       state = "NJ", country = "US", continent = "NA", lat =  40.7357, lon =  -74.1724 }
    "us-central"   = { nick = "kv-us-central",   city = "Dallas",       state = "TX", country = "US", continent = "NA", lat =  32.7767, lon =  -96.7970 }
    "us-west"      = { nick = "kv-us-west",      city = "Fremont",      state = "CA", country = "US", continent = "NA", lat =  37.5485, lon = -121.9886 }
    "us-southeast" = { nick = "kv-us-southeast", city = "Atlanta",      state = "GA", country = "US", continent = "NA", lat =  33.7490, lon =  -84.3880 }
    "us-lax"       = { nick = "kv-us-lax",       city = "Los Angeles",  state = "CA", country = "US", continent = "NA", lat =  34.0522, lon = -118.2437 }
    "us-mia"       = { nick = "kv-us-mia",       city = "Miami",        state = "FL", country = "US", continent = "NA", lat =  25.7617, lon =  -80.1918 }
    "us-sea"       = { nick = "kv-us-sea",       city = "Seattle",      state = "WA", country = "US", continent = "NA", lat =  47.6062, lon = -122.3321 }
    "ca-central"   = { nick = "kv-ca-central",   city = "Toronto",      state = "",   country = "CA", continent = "NA", lat =  43.6532, lon =  -79.3832 }
    "br-gru"       = { nick = "kv-br-gru",       city = "Sao Paulo",    state = "",   country = "BR", continent = "SA", lat = -23.5505, lon =  -46.6333 }
    "gb-lon"       = { nick = "kv-gb-lon",       city = "London",       state = "",   country = "GB", continent = "EU", lat =  51.5074, lon =   -0.1278 }
    "eu-central"   = { nick = "kv-eu-central",   city = "Frankfurt",    state = "",   country = "DE", continent = "EU", lat =  50.1109, lon =    8.6821 }
    "de-fra-2"     = { nick = "kv-de-fra-2",     city = "Frankfurt 2",  state = "",   country = "DE", continent = "EU", lat =  50.1109, lon =    8.6821 }
    "fr-par-2"     = { nick = "kv-fr-par-2",     city = "Paris 2",      state = "",   country = "FR", continent = "EU", lat =  48.8566, lon =    2.3522 }
    "nl-ams"       = { nick = "kv-nl-ams",       city = "Amsterdam",    state = "",   country = "NL", continent = "EU", lat =  52.3676, lon =    4.9041 }
    "se-sto"       = { nick = "kv-se-sto",       city = "Stockholm",    state = "",   country = "SE", continent = "EU", lat =  59.3293, lon =   18.0686 }
    "it-mil"       = { nick = "kv-it-mil",       city = "Milan",        state = "",   country = "IT", continent = "EU", lat =  45.4642, lon =    9.1900 }
    "ap-south"     = { nick = "kv-ap-south",     city = "Singapore",    state = "",   country = "SG", continent = "AS", lat =   1.3521, lon =  103.8198 }
    "sg-sin-2"     = { nick = "kv-sg-sin-2",     city = "Singapore 2",  state = "",   country = "SG", continent = "AS", lat =   1.3521, lon =  103.8198 }
    "ap-northeast" = { nick = "kv-ap-northeast", city = "Tokyo 2",      state = "",   country = "JP", continent = "AS", lat =  35.6762, lon =  139.6503 }
    "jp-tyo-3"     = { nick = "kv-jp-tyo-3",     city = "Tokyo 3",      state = "",   country = "JP", continent = "AS", lat =  35.6762, lon =  139.6503 }
    "jp-osa"       = { nick = "kv-jp-osa",       city = "Osaka",        state = "",   country = "JP", continent = "AS", lat =  34.6937, lon =  135.5023 }
    "ap-west"      = { nick = "kv-ap-west",      city = "Mumbai",       state = "",   country = "IN", continent = "AS", lat =  19.0760, lon =   72.8777 }
    "in-bom-2"     = { nick = "kv-in-bom-2",     city = "Mumbai 2",     state = "",   country = "IN", continent = "AS", lat =  19.0760, lon =   72.8777 }
    "in-maa"       = { nick = "kv-in-maa",       city = "Chennai",      state = "",   country = "IN", continent = "AS", lat =  13.0827, lon =   80.2707 }
    "id-cgk"       = { nick = "kv-id-cgk",       city = "Jakarta",      state = "",   country = "ID", continent = "AS", lat =  -6.2088, lon =  106.8456 }
    "ap-southeast" = { nick = "kv-ap-southeast", city = "Sydney",       state = "",   country = "AU", continent = "OC", lat = -33.8688, lon =  151.2093 }
  }
}

# 27 dedicated datacenters added under the existing connectedcloud5 domain.
# Other properties' DCs (non-kv-prefixed) are untouched.
resource "akamai_gtm_datacenter" "leaf" {
  for_each          = local.regions
  domain            = local.gtm_domain
  nickname          = each.value.nick
  city              = each.value.city
  state_or_province = each.value.state
  country           = each.value.country
  continent         = each.value.continent
  latitude          = each.value.lat
  longitude         = each.value.lon
}

# Performance property — pure-perf routing (load_imbalance_percentage=100 on the
# domain), HTTPS liveness on /v1/health.
resource "akamai_gtm_property" "kv" {
  domain                      = local.gtm_domain
  name                        = "nats-kv"
  type                        = "performance"
  score_aggregation_type      = "mean"
  stickiness_bonus_percentage = 50
  stickiness_bonus_constant   = 0
  handout_limit               = 8
  handout_mode                = "normal"
  failover_delay              = 0
  failback_delay              = 0
  use_computed_targets        = true

  dynamic "traffic_target" {
    for_each = akamai_gtm_datacenter.leaf
    content {
      datacenter_id = traffic_target.value.datacenter_id
      enabled       = contains(keys(var.region_ips), traffic_target.key)
      weight        = 1.0
      servers       = contains(keys(var.region_ips), traffic_target.key) ? [var.region_ips[traffic_target.key]] : []
      handout_cname = ""
    }
  }

  liveness_test {
    name                             = "kv-https-health"
    test_object_protocol             = "HTTPS"
    test_object_port                 = 443
    test_object                      = "/v1/health"
    test_timeout                     = 10.0
    test_interval                    = 60
    http_error3xx                    = true
    http_error4xx                    = true
    http_error5xx                    = true
    peer_certificate_verification    = false  # GTM probe can't supply per-DC SNI; cert validity is enforced at the client tier
    disable_nonstandard_port_warning = false
  }

  # Akamai TF provider returns null for http_method on read but defaults to "GET"
  # on apply, causing perpetual drift. Real value matches.
  lifecycle {
    ignore_changes = [liveness_test]
  }
}

# CNAME the user-facing hostname onto the GTM property.
resource "akamai_dns_record" "edge_cname" {
  zone       = "connected-cloud.io"
  name       = "nats-kv-edge.connected-cloud.io"
  recordtype = "CNAME"
  ttl        = 300
  target     = ["${akamai_gtm_property.kv.name}.${local.gtm_domain}."]
}
