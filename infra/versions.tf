# OpenTofu + bpg/proxmox
# https://registry.terraform.io/providers/bpg/proxmox/latest
terraform {
  required_version = ">= 1.7"
  required_providers {
    proxmox = {
      source  = "bpg/proxmox"
      version = "~> 0.70"
    }
  }
}
