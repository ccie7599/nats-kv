# Terraform: NATS-KV adapter VMs (v0.1 — bare Linode VMs, single region for MVP)
#
# v0.2 will pivot to LKE node pools (g8-dedicated-4-2) on the existing 27 leaf
# clusters in the presales account, once container registry distribution is sorted.
# See SCOPE.md and DECISIONS.md ADR-012.

terraform {
  required_version = ">= 1.5.0"
  required_providers {
    linode = {
      source  = "linode/linode"
      version = "~> 2.0"
    }
  }
}

variable "linode_token" {
  description = "Linode API token (presales account)"
  type        = string
  sensitive   = true
}

variable "ssh_pub_key" {
  description = "SSH public key authorized on the VMs"
  type        = string
}

variable "binary_release_url" {
  description = "URL to the kv-adapter binary (Linux x86_64)"
  type        = string
  default     = "https://github.com/ccie7599/nats-kv/releases/download/v0.1.0/kv-adapter-linux"
}

variable "demo_token" {
  description = "Bearer token for adapter API"
  type        = string
  default     = "akv_demo_open"
}

variable "regions" {
  description = "Regions to deploy KV adapter VMs into"
  type        = map(object({ short = string }))
  default = {
    "us-ord" = { short = "ord" }
  }
}

provider "linode" {
  token = var.linode_token
}

locals {
  cloud_init = templatefile("${path.module}/cloud-init.yaml.tmpl", {
    binary_url   = var.binary_release_url
    demo_token   = var.demo_token
  })
}

resource "linode_instance" "kv" {
  for_each         = var.regions
  region           = each.key
  type             = "g8-dedicated-4-2"
  image            = "linode/ubuntu24.04"
  label            = "nats-kv-${each.key}"
  authorized_keys  = [var.ssh_pub_key]
  tags             = ["project:nats-kv", "owner:brian"]
  metadata {
    user_data = base64encode(replace(local.cloud_init, "REGION_PLACEHOLDER", each.key))
  }
}

output "endpoints" {
  value = {
    for k, v in linode_instance.kv : k => "http://${v.ip_address}:8080/v1/health"
  }
}
