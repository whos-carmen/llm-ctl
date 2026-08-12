locals {
  ssh_public_key = trimspace(file(pathexpand(var.ssh_public_key_file)))
}

# NOTE: field layout below follows bpg/proxmox; confirm exact attribute names
# with `tofu validate` before apply (provider schema is the contract).
resource "proxmox_virtual_environment_container" "this" {
  for_each = var.containers

  node_name    = var.node_name
  vm_id        = each.value.vmid
  description  = "llm-ctl ${each.key} - vmid ${each.value.vmid}, tag ${var.tag}"
  tags         = [var.tag]
  unprivileged = true
  started      = true
  start_on_boot = true

  operating_system {
    template_file_id = var.template_file_id
    type             = "debian"   # debian-13 base
  }

  cpu {
    cores = each.value.cores
  }

  memory {
    dedicated = each.value.memory
  }

  disk {
    datastore_id = var.datastore_id
    size         = each.value.disk   # GiB
  }

  network_interface {
    name   = "eth0"
    bridge = var.bridge
  }

  initialization {
    hostname = each.value.hostname
    ip_config {
      ipv4 {
        address = "${each.value.ip}/${var.netmask}"
        gateway = var.gateway
      }
    }
    user_account {
      keys = [local.ssh_public_key]
    }
  }
}
