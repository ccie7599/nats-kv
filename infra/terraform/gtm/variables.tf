# Per-region IP for the KV adapter NodeBalancer (us-ord) or hostNetwork-bound
# kv-pool node (leaves). Supplied via region-ips.auto.tfvars; regenerated from
# /tmp/kv-nodes.tsv after any kv-pool node replacement.
variable "region_ips" {
  type        = map(string)
  description = "Region ID -> public IPv4 of the kv-adapter endpoint"
}
