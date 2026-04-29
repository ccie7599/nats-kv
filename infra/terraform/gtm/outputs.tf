output "gtm_domain" {
  value = local.gtm_domain
}

output "gtm_property_fqdn" {
  value = "${akamai_gtm_property.kv.name}.${local.gtm_domain}"
}

output "edge_hostname" {
  value = "nats-kv-edge.connected-cloud.io"
}

output "datacenter_count" {
  value = length(akamai_gtm_datacenter.leaf)
}

output "enabled_targets" {
  value = length(var.region_ips)
}
