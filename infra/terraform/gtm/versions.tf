terraform {
  required_version = ">= 1.5.0"

  required_providers {
    akamai = {
      source  = "akamai/akamai"
      version = "~> 6.0"
    }
  }

  backend "s3" {
    bucket                      = "presales-landing-zone-tfstate"
    key                         = "nats-kv/gtm/terraform.tfstate"
    region                      = "us-ord"
    endpoints                   = { s3 = "https://us-ord-10.linodeobjects.com" }
    skip_credentials_validation = true
    skip_metadata_api_check     = true
    skip_region_validation      = true
    skip_requesting_account_id  = true
    skip_s3_checksum            = true
    use_path_style              = true
  }
}

provider "akamai" {
  edgerc         = "~/.edgerc"
  config_section = "default"
}
